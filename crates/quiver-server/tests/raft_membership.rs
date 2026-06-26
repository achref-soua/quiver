// SPDX-License-Identifier: AGPL-3.0-only
//! Dynamic Raft voter membership through the server (ADR-0067, increment 4c).
//!
//! A shard's Raft group starts with a fixed voter set, but an operator (or, later,
//! the coordinator's grow/shrink) can change it at runtime via the admin endpoint
//! `POST/DELETE /cluster/raft/voters`. Here a single-member leader is grown to two
//! voters online: the new node, booted but not bootstrapped (so it waits), is added
//! via the endpoint — openraft adds it as a learner, it catches up, and it is
//! promoted to a voter — after which it serves the leader's data. Then it is
//! removed again.
#![cfg(feature = "raft")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

struct Node {
    base: String,
    grpc_url: String,
    server: JoinHandle<()>,
}

async fn wait_ready(http: &reqwest::Client, base: &str) {
    for _ in 0..300 {
        if let Ok(r) = http.get(format!("{base}/healthz")).send().await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server {base} did not become ready");
}

// Boot one Raft node with an explicit member list (so the caller controls which
// node bootstraps: only the lowest-id member in its own list does).
async fn boot(
    id: u64,
    dir: std::path::PathBuf,
    grpc: TcpListener,
    raft_members: Vec<String>,
) -> Node {
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_url = format!("http://{}", grpc.local_addr().unwrap());
    let mut config = Config {
        data_dir: dir,
        insecure: true,
        raft_node_id: Some(id),
        raft_members,
        ..Default::default()
    };
    config.rest_addr = rest.local_addr().unwrap();
    config.grpc_addr = grpc.local_addr().unwrap();
    let base = format!("http://{}", config.rest_addr);
    let server = tokio::spawn(async move {
        let _ = serve(config, rest, grpc).await;
    });
    Node {
        base,
        grpc_url,
        server,
    }
}

fn point(i: u32) -> Value {
    json!({"id": format!("p{i}"), "vector": [(i % 5) as f32, 1.0, 0.0, 0.0], "payload": {"i": i}})
}

async fn present(http: &reqwest::Client, base: &str, i: u32) -> bool {
    http.get(format!("{base}/v1/collections/c/points/p{i}"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn await_present(http: &reqwest::Client, base: &str, ids: &[u32]) {
    for _ in 0..300 {
        let mut all = true;
        for &i in ids {
            if !present(http, base, i).await {
                all = false;
                break;
            }
        }
        if all {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{base} never served {ids:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raft_voter_can_be_added_and_removed_at_runtime() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();

    // Bind both gRPC listeners first so each node's member URL is known up front.
    let g1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let g2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let m1 = format!("1=http://{}", g1.local_addr().unwrap());
    let m2 = format!("2=http://{}", g2.local_addr().unwrap());

    // Node 1 bootstraps a single-member group {1}; node 2 lists a lower id, so it
    // does not bootstrap — it waits to be added.
    let n1 = boot(1, dir1.path().into(), g1, vec![m1.clone()]).await;
    let n2 = boot(2, dir2.path().into(), g2, vec![m1.clone(), m2.clone()]).await;
    wait_ready(&http, &n1.base).await;
    wait_ready(&http, &n2.base).await;

    // Create + load via the leader (node 1). Retry through its initial election.
    for _ in 0..240 {
        if let Ok(r) = http
            .post(format!("{}/v1/collections", n1.base))
            .json(&json!({"name": "c", "dim": 4, "metric": "l2"}))
            .send()
            .await
            && r.status().is_success()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let points: Vec<Value> = (0..5).map(point).collect();
    http.post(format!("{}/v1/collections/c/points", n1.base))
        .json(&json!({ "points": points }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    await_present(&http, &n1.base, &[0, 1, 2, 3, 4]).await;

    // Add node 2 as a voter at runtime: it catches up and then serves the data.
    let resp = http
        .post(format!("{}/cluster/raft/voters", n1.base))
        .json(&json!({"id": 2, "url": n2.grpc_url}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "add voter failed: {}",
        resp.status()
    );
    await_present(&http, &n2.base, &[0, 1, 2, 3, 4]).await;

    // A write through the leader now commits across the two voters and reaches node 2.
    http.post(format!("{}/v1/collections/c/points", n1.base))
        .json(&json!({"points": [point(5)]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    await_present(&http, &n2.base, &[5]).await;

    // Remove node 2 from the voter set again.
    let resp = http
        .delete(format!("{}/cluster/raft/voters/2", n1.base))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "remove voter failed: {}",
        resp.status()
    );
    // The leader keeps serving on its own after the shrink.
    http.post(format!("{}/v1/collections/c/points", n1.base))
        .json(&json!({"points": [point(6)]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    await_present(&http, &n1.base, &[6]).await;

    n1.server.abort();
    n2.server.abort();
}
