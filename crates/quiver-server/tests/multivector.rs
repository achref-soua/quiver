// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end round trip for the multi-vector (late-interaction / ColBERT) API
//! over both transports (ADR-0028): create a multi-vector collection, upsert
//! documents as token sets, run a MaxSim search (with and without a filter),
//! delete a document, and confirm the single-vector API is rejected on it.

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
async fn multivector_round_trip() {
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
        encryption_key: Some(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_owned(),
        ),
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        leader_url: None,
        leader_api_key: None,
        insecure: false,
        limits: quiver_server::Limits::default(),
        embedding: Default::default(),
        rerank: Default::default(),
        rate_limit: Default::default(),
        otlp: Default::default(),
        mvcc_reads: false,
        cluster_shards: Vec::new(),
        cluster_replicas: Vec::new(),
        cluster_shard_key: None,
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // --- REST: create a multi-vector collection (cosine) ---
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "name": "papers", "dim": 3, "metric": "cosine", "multivector": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["multivector"], true);

    // A multi-vector collection rejects L2 at creation (400, not 500).
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "name": "bad", "dim": 3, "metric": "l2", "multivector": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // --- REST: upsert documents as token sets ---
    let resp = http
        .post(format!("{base}/v1/collections/papers/documents"))
        .bearer_auth(key)
        .json(&serde_json::json!({"documents": [
            {"id": "cat", "vectors": [[1.0,0.0,0.0],[0.0,1.0,0.0]], "payload": {"lang": "en"}},
            {"id": "dog", "vectors": [[0.0,1.0,0.0],[0.0,0.0,1.0]], "payload": {"lang": "en"}},
            {"id": "fish", "vectors": [[0.0,0.0,1.0],[1.0,0.0,1.0]], "payload": {"lang": "fr"}}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["upserted"],
        3
    );

    // The collection now reports its document count (not the token-row count).
    let resp = http
        .get(format!("{base}/v1/collections/papers"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["count"], 3);
    assert_eq!(body["multivector"], true);

    // --- REST: MaxSim search ranks "fish" first ---
    let resp = http
        .post(format!("{base}/v1/collections/papers/documents/query"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "query": [[1.0,0.0,0.0],[0.0,0.0,1.0]], "k": 3, "with_payload": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches[0]["id"], "fish");

    // --- REST: a document-level filter is honoured ---
    let resp = http
        .post(format!("{base}/v1/collections/papers/documents/query"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "query": [[1.0,0.0,0.0]], "k": 10,
            "filter": {"eq": {"field": "lang", "value": "fr"}}, "with_payload": true
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["id"], "fish");

    // --- REST: the single-vector API is rejected on a multi-vector collection ---
    let resp = http
        .post(format!("{base}/v1/collections/papers/points"))
        .bearer_auth(key)
        .json(&serde_json::json!({"points": [
            {"id": "x", "vector": [1.0,0.0,0.0], "payload": {}}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // --- gRPC: SearchMultiVector returns the same top document ---
    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();
    let response = client
        .search_multi_vector(auth_request(
            key,
            v1::SearchMultiVectorRequest {
                collection: "papers".to_owned(),
                query: vec![
                    v1::Vector {
                        values: vec![1.0, 0.0, 0.0],
                    },
                    v1::Vector {
                        values: vec![0.0, 0.0, 1.0],
                    },
                ],
                k: 3,
                filter: Vec::new(),
                ef_search: 64,
                with_payload: true,
                with_vector: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.matches[0].id, "fish");

    // --- REST: delete a document; it disappears from search ---
    let resp = http
        .request(
            reqwest::Method::DELETE,
            format!("{base}/v1/collections/papers/documents"),
        )
        .bearer_auth(key)
        .json(&serde_json::json!({"ids": ["fish"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["deleted"],
        1
    );

    let resp = http
        .post(format!("{base}/v1/collections/papers/documents/query"))
        .bearer_auth(key)
        .json(&serde_json::json!({"query": [[0.0,0.0,1.0]], "k": 10}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert!(matches.iter().all(|m| m["id"] != "fish"));

    server.abort();
}

/// The ColBERTv2/PLAID token-pool index (ADR-0034) is selectable over the wire
/// (REST + gRPC + the SDK enums): a `colbert` multi-vector collection is created,
/// reads its kind back over both transports, and `colbert` without `multivector`
/// is rejected at creation.
#[tokio::test]
async fn colbert_index_round_trip() {
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
        encryption_key: Some(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_owned(),
        ),
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        leader_url: None,
        leader_api_key: None,
        insecure: false,
        limits: quiver_server::Limits::default(),
        embedding: Default::default(),
        rerank: Default::default(),
        rate_limit: Default::default(),
        otlp: Default::default(),
        mvcc_reads: false,
        cluster_shards: Vec::new(),
        cluster_replicas: Vec::new(),
        cluster_shard_key: None,
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // --- REST: create a ColBERT multi-vector collection ---
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "name": "papers", "dim": 3, "metric": "cosine",
            "multivector": true, "index": "colbert"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["index"], "colbert");
    assert_eq!(body["multivector"], true);

    // --- REST: the kind survives a read-back ---
    let resp = http
        .get(format!("{base}/v1/collections/papers"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["index"], "colbert");
    assert_eq!(body["multivector"], true);

    // --- REST: colbert without multivector is rejected (400, not 500) ---
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "name": "bad", "dim": 3, "metric": "cosine", "index": "colbert"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // --- gRPC: create a colbert collection and read its kind back ---
    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();
    client
        .create_collection(auth_request(
            key,
            v1::CreateCollectionRequest {
                name: "grpc_papers".to_owned(),
                dim: 3,
                metric: v1::Metric::Cosine as i32,
                index: v1::IndexKind::Colbert as i32,
                pq_subspaces: None,
                filterable: Vec::new(),
                multivector: true,
                vector_encryption: v1::VectorEncryption::None as i32,
            },
        ))
        .await
        .unwrap();
    let collection = client
        .get_collection(auth_request(
            key,
            v1::GetCollectionRequest {
                name: "grpc_papers".to_owned(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(collection.index, v1::IndexKind::Colbert as i32);
    assert!(collection.multivector);

    server.abort();
}
