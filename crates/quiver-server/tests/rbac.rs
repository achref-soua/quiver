// SPDX-License-Identifier: AGPL-3.0-only
//! Role-based access control end-to-end (ADR-0011): prove that scoped API keys
//! are **default-deny** — an over-privileged action and a cross-namespace
//! access are both refused (HTTP 403 / gRPC `PermissionDenied`) — while in-scope
//! requests succeed, and that listing never reveals out-of-scope collections.
//! Enforcement lives at the engine-facing op layer, so both transports honour
//! it; this test exercises REST for the full matrix and gRPC for one denial.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
use quiver_server::{Action, ApiKey, CollectionScope, Config, serve};
use tokio::net::TcpListener;

const ENC_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn auth_request<T>(secret: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {secret}").parse().expect("valid metadata"),
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
async fn scoped_keys_deny_over_scope_and_cross_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    // An admin over everything, plus read/write keys scoped to the `acme.`
    // namespace only.
    let acme_only = CollectionScope::Patterns(vec!["acme.*".to_owned()]);
    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        api_keys: vec![
            ApiKey::admin("admin-secret"),
            ApiKey {
                secret: "reader-secret".to_owned(),
                role: Action::Read,
                collections: acme_only.clone(),
            },
            ApiKey {
                secret: "writer-secret".to_owned(),
                role: Action::Write,
                collections: acme_only,
            },
        ],
        encryption_key: Some(ENC_KEY.to_owned()),
        tls_cert: None,
        tls_key: None,
        insecure: false,
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    let create = |name: &str, secret: &str| {
        http.post(format!("{base}/v1/collections"))
            .bearer_auth(secret)
            .json(&serde_json::json!({"name": name, "dim": 3, "metric": "l2"}))
            .send()
    };
    let upsert = |name: &str, secret: &str| {
        http.post(format!("{base}/v1/collections/{name}/points"))
            .bearer_auth(secret)
            .json(&serde_json::json!({
                "points": [{"id": "p1", "vector": [0.1, 0.2, 0.3], "payload": {}}]
            }))
            .send()
    };
    let search = |name: &str, secret: &str| {
        http.post(format!("{base}/v1/collections/{name}/query"))
            .bearer_auth(secret)
            .json(&serde_json::json!({"vector": [0.1, 0.2, 0.3], "k": 5}))
            .send()
    };

    // The admin provisions one collection per namespace and seeds a point.
    for name in ["acme.items", "beta.items"] {
        assert_eq!(create(name, "admin-secret").await.unwrap().status(), 200);
        assert_eq!(upsert(name, "admin-secret").await.unwrap().status(), 200);
    }

    // --- In-scope: the reader reads its namespace. ---
    assert_eq!(
        search("acme.items", "reader-secret")
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        http.get(format!("{base}/v1/collections/acme.items"))
            .bearer_auth("reader-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // --- Over-scope on the action: a reader cannot write or administer. ---
    assert_eq!(
        upsert("acme.items", "reader-secret")
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        create("acme.new", "reader-secret").await.unwrap().status(),
        403
    );
    assert_eq!(
        http.delete(format!("{base}/v1/collections/acme.items"))
            .bearer_auth("reader-secret")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );

    // --- Over-scope on the action: a writer cannot administer. ---
    assert_eq!(
        upsert("acme.items", "writer-secret")
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        create("acme.new", "writer-secret").await.unwrap().status(),
        403
    );

    // --- Cross-namespace: acme-scoped keys cannot touch `beta.`. ---
    assert_eq!(
        search("beta.items", "reader-secret")
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        http.get(format!("{base}/v1/collections/beta.items"))
            .bearer_auth("reader-secret")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        upsert("beta.items", "writer-secret")
            .await
            .unwrap()
            .status(),
        403
    );

    // --- Listing is filtered to the caller's scope. ---
    let listed: serde_json::Value = http
        .get(format!("{base}/v1/collections"))
        .bearer_auth("reader-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["acme.items"], "the reader must not see beta.items");

    // --- Authentication still gates everything. ---
    assert_eq!(
        http.get(format!("{base}/v1/collections"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        search("acme.items", "wrong-secret").await.unwrap().status(),
        401
    );

    // --- gRPC enforces the same rules: the reader's write is denied, its read
    // allowed. ---
    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();
    let denied = client
        .upsert(auth_request(
            "reader-secret",
            v1::UpsertRequest {
                collection: "acme.items".to_owned(),
                points: vec![v1::Point {
                    id: "g1".to_owned(),
                    vector: vec![0.1, 0.2, 0.3],
                    payload: b"{}".to_vec(),
                }],
            },
        ))
        .await
        .expect_err("reader must be denied write over gRPC");
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);

    let allowed = client
        .search(auth_request(
            "reader-secret",
            v1::SearchRequest {
                collection: "acme.items".to_owned(),
                vector: vec![0.1, 0.2, 0.3],
                k: 5,
                filter: Vec::new(),
                ef_search: 0,
                with_payload: true,
                with_vector: false,
            },
        ))
        .await;
    assert!(allowed.is_ok(), "reader must be allowed to search in scope");

    server.abort();
}
