// SPDX-License-Identifier: AGPL-3.0-only
//! Lock-free MVCC reads at the server (ADR-0064 increment 3): with `mvcc_reads` on,
//! a **pure-vector** query is served from the cached `arc-swap` snapshot cell with
//! no database lock (the first read warms the cache via the locked path; the rest
//! take the fast path), stays correct under concurrent writes, and payload-bearing
//! reads still work through the snapshot. Drives REST.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn wait_ready(http: &reqwest::Client, base: &str) {
    for _ in 0..200 {
        if let Ok(resp) = http.get(format!("{base}/healthz")).send().await
            && resp.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not become ready");
}

#[tokio::test]
async fn mvcc_pure_vector_search_is_correct_under_concurrent_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        insecure: true,
        mvcc_reads: true, // the lock-free cutover under test
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // Create a single-vector L2 collection (MVCC-eligible).
    http.post(format!("{base}/v1/collections"))
        .json(&json!({"name": "c", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // A sentinel at the query location (distance 0) plus 20 far points.
    let points = format!("{base}/v1/collections/c/points");
    let mut batch =
        vec![json!({"id": "S", "vector": [0.0, 0.0, 0.0, 0.0], "payload": {"tag": "sentinel"}})];
    for i in 0..20u32 {
        let f = (i + 5) as f64;
        batch.push(json!({"id": format!("p{i}"), "vector": [f, f, f, f]}));
    }
    http.post(&points)
        .json(&json!({ "points": batch }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let query = format!("{base}/v1/collections/c/query");
    let pure = json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 3, "ef_search": 16, "with_payload": false, "with_vector": false});

    // Pure-vector reads: the first warms the cell cache via the locked path, the
    // rest take the lock-free fast path. The sentinel is always nearest, and a
    // pure-vector read carries no payload.
    for _ in 0..4 {
        let resp: Value = http
            .post(&query)
            .json(&pure)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let matches = resp["matches"].as_array().unwrap();
        assert_eq!(matches[0]["id"], "S");
        assert!(matches[0]["payload"].is_null());
    }

    // A payload-bearing read still works (locked path, served from the snapshot).
    let with_payload = json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 1, "ef_search": 16, "with_payload": true, "with_vector": false});
    let resp: Value = http
        .post(&query)
        .json(&with_payload)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["matches"][0]["id"], "S");
    assert_eq!(resp["matches"][0]["payload"]["tag"], "sentinel");

    // Concurrent writes while pure-vector reads run on the fast path: the writer
    // republishes the snapshot on every upsert; the sentinel must stay top-1 on
    // every read (a torn/empty publish would drop it).
    let writer = {
        let http = http.clone();
        let points = points.clone();
        tokio::spawn(async move {
            for i in 0..60u32 {
                let f = (i + 1000) as f64;
                let _ = http
                    .post(&points)
                    .json(&json!({"points": [{"id": format!("w{i}"), "vector": [f, f, f, f]}]}))
                    .send()
                    .await;
            }
        })
    };
    for _ in 0..40 {
        let resp: Value = http
            .post(&query)
            .json(&pure)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            resp["matches"][0]["id"], "S",
            "sentinel lost under concurrent writes"
        );
    }
    writer.await.unwrap();

    server.abort();
}
