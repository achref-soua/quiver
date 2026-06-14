// SPDX-License-Identifier: AGPL-3.0-only
//! The cockpit's data layer against a live server: start a real `quiver-server`,
//! seed a collection over REST, and assert [`quiver_tui::fetch_snapshot`] reports
//! it — the contract the cockpit renders.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017 scopes the ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use quiver_tui::{TuiOptions, fetch_snapshot};
use tokio::net::TcpListener;

#[tokio::test]
async fn cockpit_reads_live_server_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest.local_addr().unwrap();
    let grpc_addr = grpc.local_addr().unwrap();

    // Insecure loopback keeps the test focused on the cockpit↔server contract.
    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        api_keys: vec![],
        encryption_key: None,
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        audit_log: None,
        insecure: true,
    };
    tokio::spawn(async move {
        let _ = serve(config, rest, grpc).await;
    });

    let base_url = format!("http://{rest_addr}");
    let client = reqwest::Client::new();

    let mut ready = false;
    for _ in 0..200 {
        if let Ok(resp) = client.get(format!("{base_url}/readyz")).send().await
            && resp.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "server did not become ready");

    // Seed a collection, then read it back through the cockpit's snapshot path.
    client
        .post(format!("{base_url}/v1/collections"))
        .json(&serde_json::json!({ "name": "items", "dim": 4, "metric": "cosine" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let options = TuiOptions {
        base_url,
        api_key: None,
    };
    let snapshot = fetch_snapshot(&client, &options).await.unwrap();
    assert!(snapshot.ready, "snapshot should report the server ready");
    assert_eq!(snapshot.collections.len(), 1);
    assert_eq!(snapshot.collections[0].name, "items");
    assert_eq!(snapshot.collections[0].dim, 4);
    assert_eq!(snapshot.collections[0].metric, "cosine");
}
