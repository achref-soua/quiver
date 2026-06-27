// SPDX-License-Identifier: AGPL-3.0-only
//! The cluster coordinator's membership API is authenticated (ADR-0011): a
//! network-reachable coordinator cannot be reshaped by an unauthenticated caller.
//! Reads (`/cluster/map`, `/cluster/health`) require any valid key; the mutating
//! shard ops require the `admin` role; `/healthz` and `/readyz` stay open. With no
//! keys configured (insecure mode) any caller is admitted — covered by the other
//! cluster tests, which run keyless.

// A test harness; panics are the failure signal (ADR-0017 scopes the
// unwrap/expect ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Action, ApiKey, CollectionScope, Config, serve};
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
    panic!("coordinator did not become ready");
}

#[tokio::test]
async fn coordinator_membership_api_requires_auth() {
    let admin = "admin-secret";
    let reader = "reader-secret";

    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    let config = Config {
        rest_addr,
        grpc_addr,
        coordinator: true,
        // A shard to remove and a read-only key alongside the admin key.
        cluster_shards: vec!["http://127.0.0.1:1".into(), "http://127.0.0.1:2".into()],
        api_keys: vec![
            ApiKey::admin(admin),
            ApiKey {
                secret: reader.to_owned(),
                role: Action::Read,
                collections: CollectionScope::All,
                id: None,
            },
        ],
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // /healthz is open (liveness) even with keys configured.
    let h = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(
        h.status(),
        reqwest::StatusCode::OK,
        "healthz must stay open"
    );

    // --- A mutating op (add a shard) ---
    let add_body = serde_json::json!({"primary_url": "http://127.0.0.1:3", "replica_urls": []});

    // No bearer -> 401.
    let no_key = http
        .post(format!("{base}/cluster/shards"))
        .json(&add_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        no_key.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "add_shard without a key must be 401"
    );

    // Read-only key -> 403 (authenticated but role too low for a mutation).
    let ro = http
        .post(format!("{base}/cluster/shards"))
        .bearer_auth(reader)
        .json(&add_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        ro.status(),
        reqwest::StatusCode::FORBIDDEN,
        "add_shard with a read-only key must be 403"
    );

    // Admin key -> 200, and the shard is added.
    let ok = http
        .post(format!("{base}/cluster/shards"))
        .bearer_auth(admin)
        .json(&add_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status(),
        reqwest::StatusCode::OK,
        "add_shard with the admin key must succeed"
    );
    let map: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(map["shards"].as_array().unwrap().len(), 3);

    // --- DELETE is admin-gated too: no key -> 401 ---
    let del_no_key = http
        .delete(format!("{base}/cluster/shards/2"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        del_no_key.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "remove_shard without a key must be 401"
    );

    // --- A read op (/cluster/map): no key -> 401, read-only key -> 200 ---
    let map_no_key = http
        .get(format!("{base}/cluster/map"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        map_no_key.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "/cluster/map without a key must be 401"
    );
    let map_ro = http
        .get(format!("{base}/cluster/map"))
        .bearer_auth(reader)
        .send()
        .await
        .unwrap();
    assert_eq!(
        map_ro.status(),
        reqwest::StatusCode::OK,
        "/cluster/map with a read-only key must succeed"
    );

    server.abort();
}
