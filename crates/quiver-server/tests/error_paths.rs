// SPDX-License-Identifier: AGPL-3.0-only
//! The bad-input contract: every public entry point rejects a malformed or
//! out-of-policy request **cleanly** — a 4xx HTTP status (gRPC `InvalidArgument`
//! / `NotFound`), never a 5xx, never a dropped connection from a panicking
//! handler. A 500 here means the engine leaked an unmapped error through the
//! server's `_ => INTERNAL_SERVER_ERROR` catch-all (error.rs); a connection
//! error means a handler panicked. Both are bugs this test is meant to catch.
//!
//! The contract is transport-agnostic — REST and gRPC share the single
//! `Error::category()` mapping (ADR-0017) — so the battery runs over REST and a
//! representative case is re-checked over gRPC to prove parity.

// A test harness; panics are the failure signal (ADR-0017 scopes the
// unwrap/expect ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
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

/// Assert a request was answered (no panic-induced connection drop) with a 4xx,
/// never a 5xx. `label` identifies the case in a failure message.
fn assert_client_error(label: &str, result: reqwest::Result<reqwest::Response>) {
    let resp =
        result.unwrap_or_else(|e| panic!("{label}: no HTTP response (handler panicked?): {e}"));
    let status = resp.status();
    assert!(
        status.is_client_error(),
        "{label}: expected a 4xx, got {status}"
    );
}

#[tokio::test]
async fn bad_input_is_rejected_cleanly_never_500() {
    let tmp = tempfile::tempdir().unwrap();
    let key = "test-api-key";

    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        api_keys: vec![key.into()],
        // Exercise the full encrypted path, like the round-trip test.
        encryption_key: Some(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_owned(),
        ),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // A valid dim-4 L2 collection to aim malformed requests at.
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({"name": "items", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // --- Unknown collection on every collection-scoped op -> 404 (4xx) ---
    assert_client_error(
        "upsert to unknown collection",
        http.post(format!("{base}/v1/collections/ghost/points"))
            .bearer_auth(key)
            .json(&serde_json::json!({"points": [
                {"id": "x", "vector": [0.0, 0.0, 0.0, 0.0], "payload": {}}
            ]}))
            .send()
            .await,
    );
    assert_client_error(
        "query unknown collection",
        http.post(format!("{base}/v1/collections/ghost/query"))
            .bearer_auth(key)
            .json(&serde_json::json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 1}))
            .send()
            .await,
    );
    assert_client_error(
        "get from unknown collection",
        http.get(format!("{base}/v1/collections/ghost/points/x"))
            .bearer_auth(key)
            .send()
            .await,
    );
    assert_client_error(
        "get unknown collection",
        http.get(format!("{base}/v1/collections/ghost"))
            .bearer_auth(key)
            .send()
            .await,
    );

    // --- Wrong vector dimensionality (3 into a dim-4 collection) -> 400 ---
    assert_client_error(
        "upsert wrong-dim vector",
        http.post(format!("{base}/v1/collections/items/points"))
            .bearer_auth(key)
            .json(&serde_json::json!({"points": [
                {"id": "x", "vector": [0.0, 0.0, 0.0], "payload": {}}
            ]}))
            .send()
            .await,
    );
    assert_client_error(
        "query wrong-dim vector",
        http.post(format!("{base}/v1/collections/items/query"))
            .bearer_auth(key)
            .json(&serde_json::json!({"vector": [0.0, 0.0, 0.0], "k": 1}))
            .send()
            .await,
    );

    // --- Out-of-policy cost limits (ADR-0040) -> 400 ---
    assert_client_error(
        "k over max_k",
        http.post(format!("{base}/v1/collections/items/query"))
            .bearer_auth(key)
            .json(&serde_json::json!({"vector": [0.0, 0.0, 0.0, 0.0], "k": 100_000}))
            .send()
            .await,
    );
    assert_client_error(
        "ef_search over max_ef_search",
        http.post(format!("{base}/v1/collections/items/query"))
            .bearer_auth(key)
            .json(&serde_json::json!({
                "vector": [0.0, 0.0, 0.0, 0.0], "k": 1, "ef_search": 1_000_000
            }))
            .send()
            .await,
    );
    // A payload past max_payload_bytes (default 64 KiB).
    let huge = "x".repeat(70_000);
    assert_client_error(
        "payload over max_payload_bytes",
        http.post(format!("{base}/v1/collections/items/points"))
            .bearer_auth(key)
            .json(&serde_json::json!({"points": [
                {"id": "x", "vector": [0.0, 0.0, 0.0, 0.0], "payload": {"blob": huge}}
            ]}))
            .send()
            .await,
    );

    // --- Malformed bodies (serde rejection at the DTO edge) -> 4xx ---
    assert_client_error(
        "unknown metric on create",
        http.post(format!("{base}/v1/collections"))
            .bearer_auth(key)
            .json(&serde_json::json!({"name": "bad_metric", "dim": 4, "metric": "banana"}))
            .send()
            .await,
    );
    assert_client_error(
        "missing required dim on create",
        http.post(format!("{base}/v1/collections"))
            .bearer_auth(key)
            .json(&serde_json::json!({"name": "no_dim", "metric": "l2"}))
            .send()
            .await,
    );
    assert_client_error(
        "non-JSON-object body on create",
        http.post(format!("{base}/v1/collections"))
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body("not json at all")
            .send()
            .await,
    );

    // --- gRPC parity: a wrong-dim search returns InvalidArgument, not Internal ---
    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();
    let mut request = tonic::Request::new(v1::SearchRequest {
        collection: "items".to_owned(),
        vector: vec![0.0, 0.0, 0.0], // dim 3 into a dim-4 collection
        k: 1,
        filter: Vec::new(),
        ef_search: 64,
        with_payload: false,
        with_vector: false,
    });
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let status = client.search(request).await.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "grpc wrong-dim search must be InvalidArgument, got {:?}",
        status.code()
    );

    // --- The server is still alive after the whole battery (no handler panic
    //     took the process or a connection pool down) ---
    let health = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert!(
        health.status().is_success(),
        "server alive after bad inputs"
    );

    server.abort();
}
