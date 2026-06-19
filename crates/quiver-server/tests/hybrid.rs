// SPDX-License-Identifier: AGPL-3.0-only
//! Hybrid (dense + sparse) search over REST end-to-end (ADR-0043): a point that
//! is the dense nearest neighbour and a point that matches the sparse query both
//! rank above a point that matches neither, the RRF fusion is reachable through
//! the `/query/hybrid` endpoint, and pure-dense / pure-sparse work through it too.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
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
async fn hybrid_search_over_rest_fuses_dense_and_sparse() {
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
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // Collection + three points: "a" is the dense nearest neighbour of the query;
    // "b" shares the query's sparse terms; "c" matches neither.
    let create = http
        .post(format!("{base}/v1/collections"))
        .json(&serde_json::json!({"name": "kb", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap();
    assert!(create.status().is_success());
    let points = serde_json::json!({"points": [
        {"id": "a", "vector": [1.0, 0.0, 0.0, 0.0], "payload": {"__quiver_sparse__": {"indices": [100], "values": [0.1]}}},
        {"id": "b", "vector": [0.0, 1.0, 0.0, 0.0], "payload": {"__quiver_sparse__": {"indices": [1, 2], "values": [5.0, 5.0]}}},
        {"id": "c", "vector": [0.0, 0.0, 0.0, 1.0], "payload": {"__quiver_sparse__": {"indices": [9], "values": [1.0]}}}
    ]});
    let up = http
        .post(format!("{base}/v1/collections/kb/points"))
        .json(&points)
        .send()
        .await
        .unwrap();
    assert!(up.status().is_success());

    let hybrid = format!("{base}/v1/collections/kb/query/hybrid");

    // Hybrid: "a" (dense) and "b" (sparse) rank above "c".
    let resp = http
        .post(&hybrid)
        .json(&serde_json::json!({
            "vector": [1.0, 0.0, 0.0, 0.0],
            "sparse_indices": [1, 2],
            "sparse_values": [1.0, 1.0],
            "k": 3
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let ids: Vec<String> = resp.json::<serde_json::Value>().await.unwrap()["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        ids.contains(&"a".to_owned()) && ids.contains(&"b".to_owned()),
        "got {ids:?}"
    );
    assert_eq!(ids.last().unwrap(), "c", "c matches neither; got {ids:?}");

    // Pure sparse: only "b" shares the query's terms.
    let sparse_only = http
        .post(&hybrid)
        .json(&serde_json::json!({"sparse_indices": [1, 2], "sparse_values": [1.0, 1.0], "k": 3}))
        .send()
        .await
        .unwrap();
    let body = sparse_only.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["matches"][0]["id"].as_str().unwrap(), "b");

    // A request with neither a dense nor a sparse query is a 400.
    let empty = http
        .post(&hybrid)
        .json(&serde_json::json!({"k": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);

    server.abort();
}
