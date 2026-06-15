// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end round trip over both transports — the Phase 1 DoD: create a
//! collection, upsert points with payloads, run a filtered top-k, and read back
//! correct results, over REST and gRPC, with API-key auth enforced.

// A test harness; panics are the failure signal (ADR-0017 scopes the
// unwrap/expect ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
use quiver_server::{Config, serve};
use tokio::net::TcpListener;

fn auth_request<T>(key: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {key}").parse().expect("valid metadata"),
    );
    request
}

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
async fn rest_and_grpc_round_trip() {
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
        // Exercise the full encrypted path (server → engine → AEAD codec) over
        // both transports, not just plaintext storage.
        encryption_key: Some(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_owned(),
        ),
        // This loopback test exercises the plaintext transport path; TLS has its
        // own end-to-end test in tls.rs.
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        leader_url: None,
        leader_api_key: None,
        insecure: false,
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // --- REST: auth is enforced (no key -> 401) ---
    let unauth = http
        .get(format!("{base}/v1/collections"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // --- REST: create collection (index defaults to hnsw) ---
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({"name": "items", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["index"], "hnsw");

    // --- REST: the index choice flows through (the memory-frugal disk path) ---
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "name": "frugal", "dim": 4, "metric": "l2",
            "index": "disk_vamana", "pq_subspaces": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["index"], "disk_vamana");
    assert_eq!(body["pq_subspaces"], 1);
    // And inner product with a non-HNSW index is a 400, not a 500.
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({"name": "bad", "dim": 4, "metric": "dot", "index": "vamana"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // --- REST: upsert points with payloads ---
    let resp = http
        .post(format!("{base}/v1/collections/items/points"))
        .bearer_auth(key)
        .json(&serde_json::json!({"points": [
            {"id": "a", "vector": [0.0, 0.0, 0.0, 0.0], "payload": {"color": "red"}},
            {"id": "b", "vector": [1.0, 0.0, 0.0, 0.0], "payload": {"color": "blue"}},
            {"id": "c", "vector": [5.0, 5.0, 5.0, 5.0], "payload": {"color": "red"}}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["upserted"], 3);

    // --- REST: filtered top-k (color = red) near the origin ---
    let resp = http
        .post(format!("{base}/v1/collections/items/query"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "vector": [0.1, 0.0, 0.0, 0.0],
            "k": 2,
            "filter": {"eq": {"field": "color", "value": "red"}},
            "with_payload": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches[0]["id"], "a"); // nearest red point
    for m in matches {
        assert_eq!(m["payload"]["color"], "red"); // blue point filtered out
    }

    // --- REST: filterable fields are declared, echoed back, and drive the
    // hybrid pre-filter path end to end ---
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "name": "people", "dim": 4, "metric": "l2",
            "filterable": [{"path": "city", "field_type": "keyword"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["filterable"][0]["path"], "city");
    assert_eq!(body["filterable"][0]["field_type"], "keyword");

    let resp = http
        .post(format!("{base}/v1/collections/people/points"))
        .bearer_auth(key)
        .json(&serde_json::json!({"points": [
            {"id": "p", "vector": [0.0, 0.0, 0.0, 0.0], "payload": {"city": "paris"}},
            {"id": "l", "vector": [1.0, 0.0, 0.0, 0.0], "payload": {"city": "lyon"}},
            {"id": "p2", "vector": [2.0, 0.0, 0.0, 0.0], "payload": {"city": "paris"}}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let resp = http
        .post(format!("{base}/v1/collections/people/query"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "vector": [0.0, 0.0, 0.0, 0.0],
            "k": 5,
            "filter": {"eq": {"field": "city", "value": "paris"}},
            "with_payload": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches[0]["id"], "p"); // nearest paris point via the pre-filter
    for m in matches {
        assert_eq!(m["payload"]["city"], "paris"); // lyon filtered out
    }

    // --- gRPC: connect and exercise the same collection ---
    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();

    // Unauthenticated gRPC call is rejected.
    let denied = client
        .get_collection(tonic::Request::new(v1::GetCollectionRequest {
            name: "items".to_owned(),
        }))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::Unauthenticated);

    // Collection reports the three upserted points.
    let collection = client
        .get_collection(auth_request(
            key,
            v1::GetCollectionRequest {
                name: "items".to_owned(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(collection.count, 3);
    assert_eq!(collection.dim, 4);

    // gRPC search returns the nearest point.
    let response = client
        .search(auth_request(
            key,
            v1::SearchRequest {
                collection: "items".to_owned(),
                vector: vec![0.9, 0.0, 0.0, 0.0],
                k: 1,
                filter: Vec::new(),
                ef_search: 64,
                with_payload: true,
                with_vector: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.matches.len(), 1);
    assert_eq!(response.matches[0].id, "b"); // closest to [0.9, 0, 0, 0]

    server.abort();
}
