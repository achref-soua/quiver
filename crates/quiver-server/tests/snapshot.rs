// SPDX-License-Identifier: AGPL-3.0-only
//! Online snapshot over REST end-to-end (ADR-0050): `POST /v1/snapshot` writes a
//! consistent copy of the live database to a server-local directory, which then
//! opens as an identical database; snapshotting onto an existing directory is a
//! 409.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_embed::Database;
use quiver_server::{Config, serve};
use serde_json::json;
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
async fn rest_snapshot_copies_a_consistent_database() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let snap_dir = out.path().join("snap");

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

    // Create a collection and a point.
    let resp = http
        .post(format!("{base}/v1/collections"))
        .json(&json!({ "name": "kb", "dim": 4, "dtype": "f32", "metric": "l2" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create: {}", resp.status());
    let resp = http
        .post(format!("{base}/v1/collections/kb/points"))
        .json(&json!({ "points": [{ "id": "a", "vector": [1.0, 0.0, 0.0, 0.0], "payload": { "n": 1 } }] }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "upsert: {}", resp.status());

    // Snapshot to a server-local directory.
    let resp = http
        .post(format!("{base}/v1/snapshot"))
        .json(&json!({ "destination": snap_dir.to_str().unwrap() }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "snapshot: {}", resp.status());
    let info: serde_json::Value = resp.json().await.unwrap();
    assert!(info["files"].as_u64().unwrap() > 0);
    assert!(info["bytes"].as_u64().unwrap() > 0);

    // The snapshot opens as an identical database.
    let db = Database::open(&snap_dir).unwrap();
    assert_eq!(db.collection_names(), vec!["kb".to_owned()]);
    assert_eq!(db.len("kb").unwrap(), 1);
    let got = db.get("kb", "a").unwrap().unwrap();
    assert_eq!(got.payload, Some(json!({ "n": 1 })));

    // Snapshotting onto the existing directory is a conflict.
    let resp = http
        .post(format!("{base}/v1/snapshot"))
        .json(&json!({ "destination": snap_dir.to_str().unwrap() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    server.abort();
}
