// SPDX-License-Identifier: AGPL-3.0-only
//! Audit logging end-to-end (ADR-0011): prove that the append-only log records
//! mutating and administrative operations and access-control denials with the
//! acting principal, the action, the resource, and the outcome — while
//! successful reads are not logged and **no secret ever appears in the file**.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Action, ApiKey, CollectionScope, Config, serve};
use serde_json::Value;
use tokio::net::TcpListener;

const ENC_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

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

fn find<'a>(
    records: &'a [Value],
    action: &str,
    resource: &str,
    outcome: &str,
) -> Option<&'a Value> {
    records
        .iter()
        .find(|r| r["action"] == action && r["resource"] == resource && r["outcome"] == outcome)
}

#[tokio::test]
async fn audit_log_records_mutations_and_denials_without_leaking_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let audit_path = tmp.path().join("audit.log");
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    // A labelled admin (identified as `ci-admin`) and an unlabelled reader
    // scoped to `acme.` (identified by a non-secret fingerprint).
    let config = Config {
        data_dir: tmp.path().join("data"),
        rest_addr,
        grpc_addr,
        api_keys: vec![
            ApiKey {
                id: Some("ci-admin".to_owned()),
                ..ApiKey::admin("admin-secret")
            },
            ApiKey {
                secret: "reader-secret".to_owned(),
                role: Action::Read,
                collections: CollectionScope::Patterns(vec!["acme.*".to_owned()]),
                id: None,
            },
        ],
        encryption_key: Some(ENC_KEY.to_owned()),
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: Some(audit_path.clone()),
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

    // The admin creates a collection and upserts a point (both audited as ok).
    assert_eq!(
        http.post(format!("{base}/v1/collections"))
            .bearer_auth("admin-secret")
            .json(&serde_json::json!({"name": "acme.docs", "dim": 3, "metric": "l2"}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        http.post(format!("{base}/v1/collections/acme.docs/points"))
            .bearer_auth("admin-secret")
            .json(&serde_json::json!({
                "points": [{"id": "p1", "vector": [0.1, 0.2, 0.3], "payload": {}}]
            }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // The reader is denied a write (role too low) and a cross-namespace read
    // (out of scope) — both audited as denied.
    assert_eq!(
        http.post(format!("{base}/v1/collections/acme.docs/points"))
            .bearer_auth("reader-secret")
            .json(&serde_json::json!({
                "points": [{"id": "p2", "vector": [0.1, 0.2, 0.3], "payload": {}}]
            }))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        http.get(format!("{base}/v1/collections/beta.secret"))
            .bearer_auth("reader-secret")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );

    // The reader's in-scope read succeeds — and must NOT be audited.
    assert_eq!(
        http.post(format!("{base}/v1/collections/acme.docs/query"))
            .bearer_auth("reader-secret")
            .json(&serde_json::json!({"vector": [0.1, 0.2, 0.3], "k": 5}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    server.abort();

    let raw = std::fs::read_to_string(&audit_path).expect("audit log exists");
    let records: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).expect("each audit line is valid JSON"))
        .collect();
    assert!(!records.is_empty(), "the audit log must have records");

    // Mutations by the admin are recorded with its label and outcome `ok`.
    let create = find(&records, "create_collection", "acme.docs", "ok")
        .expect("create_collection ok recorded");
    assert_eq!(create["actor"], "ci-admin");
    assert!(
        create["ts_ms"].as_u64().is_some(),
        "every record is timestamped"
    );
    let upsert = find(&records, "upsert", "acme.docs", "ok").expect("upsert ok recorded");
    assert_eq!(upsert["actor"], "ci-admin");

    // Both denials are recorded, attributed to the reader by a non-secret
    // `key:<fingerprint>` identity.
    let denied_write =
        find(&records, "upsert", "acme.docs", "denied").expect("denied write recorded");
    assert!(
        denied_write["actor"].as_str().unwrap().starts_with("key:"),
        "an unlabelled key is identified by a fingerprint, not a secret"
    );
    let denied_read = find(&records, "get_collection", "beta.secret", "denied")
        .expect("cross-namespace denial recorded");
    assert!(denied_read["actor"].as_str().unwrap().starts_with("key:"));

    // Successful reads are never audited.
    assert!(
        records.iter().all(|r| r["action"] != "search"),
        "successful reads must not be audited"
    );

    // The crucial property: no secret is ever written to the audit log.
    assert!(
        !raw.contains("admin-secret") && !raw.contains("reader-secret"),
        "the audit log must never contain a key secret"
    );
}
