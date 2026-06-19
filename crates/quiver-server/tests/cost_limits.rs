// SPDX-License-Identifier: AGPL-3.0-only
//! Query cost limits end-to-end (ADR-0040): an authenticated request that
//! exceeds a configured cap — `k`, `ef_search`, `fetch` limit, vector dimension,
//! payload size, or upsert batch size — is rejected with HTTP 400, while a
//! request at the limit succeeds. Enforcement lives at the shared op layer, so
//! both transports honour it; this test drives REST. A unit check confirms a
//! zero cap is refused at startup.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, Limits, serve};
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
async fn over_limit_requests_are_rejected_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    // Low caps make the limits cheap to exercise deterministically.
    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        insecure: true,
        limits: Limits {
            max_k: 5,
            max_ef_search: 8,
            max_fetch_limit: 3,
            max_vector_dim: 4,
            max_payload_bytes: 32,
            max_batch_size: 2,
            max_request_body_bytes: 1 << 20,
            max_sparse_terms: 8,
        },
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    let collections = format!("{base}/v1/collections");

    // A collection at the dimension cap is fine; over the cap is a 400.
    let ok = http
        .post(&collections)
        .json(&serde_json::json!({"name": "v", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap();
    assert!(
        ok.status().is_success(),
        "create within dim cap should succeed"
    );

    let too_wide = http
        .post(&collections)
        .json(&serde_json::json!({"name": "wide", "dim": 5, "metric": "l2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        too_wide.status(),
        400,
        "dim over max_vector_dim must be 400"
    );

    // Seed one point so search has something to scan.
    let points = format!("{base}/v1/collections/v/points");
    let seed = http
        .post(&points)
        .json(&serde_json::json!({"points": [{"id": "a", "vector": [0.0, 0.0, 0.0, 0.0]}]}))
        .send()
        .await
        .unwrap();
    assert!(seed.status().is_success(), "in-limit upsert should succeed");

    let query = format!("{base}/v1/collections/v/query");

    // A query exactly at the caps is allowed.
    let at_limit = http
        .post(&query)
        .json(&serde_json::json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 5, "ef_search": 8}))
        .send()
        .await
        .unwrap();
    assert!(
        at_limit.status().is_success(),
        "k/ef at the cap should succeed"
    );

    // Each over-limit query dimension is independently a 400.
    let cases = [
        serde_json::json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 6, "ef_search": 8}),
        serde_json::json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 5, "ef_search": 9}),
        serde_json::json!({"vector": [0.0, 0.0, 0.0, 0.0, 0.0], "k": 5, "ef_search": 8}),
    ];
    for body in cases {
        let resp = http.post(&query).json(&body).send().await.unwrap();
        assert_eq!(resp.status(), 400, "over-limit query must be 400: {body}");
    }

    // fetch limit cap.
    let fetch = format!("{base}/v1/collections/v/fetch");
    let over_fetch = http
        .post(&fetch)
        .json(&serde_json::json!({"limit": 4}))
        .send()
        .await
        .unwrap();
    assert_eq!(over_fetch.status(), 400, "fetch limit over cap must be 400");
    let at_fetch = http
        .post(&fetch)
        .json(&serde_json::json!({"limit": 3}))
        .send()
        .await
        .unwrap();
    assert!(
        at_fetch.status().is_success(),
        "fetch at the cap should succeed"
    );

    // Batch size cap (3 points > max_batch_size 2).
    let big_batch = http
        .post(&points)
        .json(&serde_json::json!({"points": [
            {"id": "b", "vector": [0.0, 0.0, 0.0, 0.0]},
            {"id": "c", "vector": [0.0, 0.0, 0.0, 0.0]},
            {"id": "d", "vector": [0.0, 0.0, 0.0, 0.0]}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(big_batch.status(), 400, "batch over cap must be 400");

    // Payload size cap (a string well over 32 serialized bytes).
    let big_payload = http
        .post(&points)
        .json(&serde_json::json!({"points": [
            {"id": "e", "vector": [0.0, 0.0, 0.0, 0.0],
             "payload": {"note": "this payload is comfortably over thirty-two bytes"}}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(big_payload.status(), 400, "payload over cap must be 400");

    server.abort();
}

#[test]
fn validate_rejects_a_zero_limit() {
    let mut config = Config {
        insecure: true,
        ..Default::default()
    };
    config.limits.max_k = 0;
    assert!(
        config.validate().is_err(),
        "a zero cap would refuse every request and must be rejected at startup"
    );
}
