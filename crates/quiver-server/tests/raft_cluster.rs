// SPDX-License-Identifier: AGPL-3.0-only
//! Per-shard Raft write HA wired into the server (ADR-0067, increment 4b-iii-b).
//!
//! Boots a real 3-node Raft group — each node a full `serve()` with the
//! `RaftService` riding its gRPC port — and drives writes through the HTTP API.
//! A write reaches the engine only after a **quorum commit**, then every voter
//! applies it, so:
//!
//! - a write accepted by the leader is served by all three nodes, and
//! - killing the leader's process triggers an automatic failover: a survivor
//!   takes over, post-failover writes are accepted and applied, and **no
//!   acknowledged write is lost**.
//!
//! The consensus protocol itself is proven in `raft.rs` (in-process) and
//! `raft/grpc.rs` (real gRPC); this test proves the **server wiring** — config,
//! the write-handler Raft branch, the "not the leader" 421, and the RaftService
//! mounted on the live gRPC server. The ground-truth oracle is a direct
//! **store fetch** of every written id (immediately consistent, unlike the HNSW
//! index whose off-lock rebuild is eventually consistent — ADR-0062).
//!
//! Per ADR-0067's owner-locked staging the 4b log is volatile, so the killed
//! leader does not rejoin; this covers leader failover among live members.
#![cfg(feature = "raft")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One booted Raft node: its REST base URL and the task running `serve()` (aborted
/// to model a process kill).
struct Node {
    base: String,
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

// Boot a 3-node Raft group. Every node's gRPC listener is bound first so the
// member set (id → gRPC URL) is known before any node starts, then each node is
// served with that member list and its own data dir.
async fn boot_raft_group(dirs: &[std::path::PathBuf]) -> Vec<Node> {
    let mut rests = Vec::new();
    let mut grpcs = Vec::new();
    let mut members = Vec::new();
    for (i, _) in dirs.iter().enumerate() {
        let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let id = (i + 1) as u64;
        members.push(format!("{id}=http://{}", grpc.local_addr().unwrap()));
        rests.push(rest);
        grpcs.push(grpc);
    }

    let mut nodes = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        let rest = rests.remove(0);
        let grpc = grpcs.remove(0);
        let mut config = Config {
            data_dir: dir.clone(),
            insecure: true,
            raft_node_id: Some((i + 1) as u64),
            raft_members: members.clone(),
            ..Default::default()
        };
        config.rest_addr = rest.local_addr().unwrap();
        config.grpc_addr = grpc.local_addr().unwrap();
        let base = format!("http://{}", config.rest_addr);
        let server = tokio::spawn(async move {
            let _ = serve(config, rest, grpc).await;
        });
        nodes.push(Node { base, server });
    }
    nodes
}

fn vec_for(i: u32) -> Vec<f32> {
    [
        (i % 7) as f32,
        ((i + 2) % 5) as f32,
        ((i + 1) % 3) as f32,
        (i % 2) as f32,
    ]
    .to_vec()
}

// Send a write to whichever live node is currently the leader, retrying across
// nodes and over an election window. A follower replies 421 (not the leader);
// `also_ok` lets `create` treat an already-exists 409 as done. Returns the base
// URL that accepted it (the leader).
async fn leader_write(
    http: &reqwest::Client,
    nodes: &[&Node],
    path: &str,
    body: &Value,
    also_ok: Option<reqwest::StatusCode>,
) -> String {
    for _ in 0..240 {
        for n in nodes {
            if let Ok(r) = http
                .post(format!("{}{path}", n.base))
                .json(body)
                .send()
                .await
            {
                let status = r.status();
                if status.is_success() || Some(status) == also_ok {
                    return n.base.clone();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no leader accepted the write to {path}");
}

// Delete points through the current leader (the DELETE write path), retrying
// across nodes and an election window. A follower replies 421.
async fn leader_delete(http: &reqwest::Client, nodes: &[&Node], ids: &[u32]) {
    let body = json!({"ids": ids.iter().map(|i| format!("p{i}")).collect::<Vec<_>>()});
    for _ in 0..240 {
        for n in nodes {
            if let Ok(r) = http
                .delete(format!("{}/v1/collections/c/points", n.base))
                .json(&body)
                .send()
                .await
                && r.status().is_success()
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no leader accepted the delete");
}

fn points_body(ids: &[u32]) -> Value {
    let points: Vec<Value> = ids
        .iter()
        .map(|&i| json!({"id": format!("p{i}"), "vector": vec_for(i), "payload": {"i": i}}))
        .collect();
    json!({ "points": points })
}

// Fetch one point's payload `i` field from `base` (a store read — immediately
// consistent after apply), or `None` if the point is absent.
async fn payload_i(http: &reqwest::Client, base: &str, i: u32) -> Option<u32> {
    let r = http
        .get(format!("{base}/v1/collections/c/points/p{i}"))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: Value = r.json().await.ok()?;
    v["payload"]["i"].as_u64().map(|n| n as u32)
}

// Poll until `base` serves every id in `ids` with the right payload — the
// no-lost-write oracle. Followers apply the committed op asynchronously, so this
// converges once the apply lands (a store read, no index-rebuild lag).
async fn await_all_present(http: &reqwest::Client, base: &str, ids: &[u32]) {
    for _ in 0..240 {
        let mut missing = None;
        for &i in ids {
            if payload_i(http, base, i).await != Some(i) {
                missing = Some(i);
                break;
            }
        }
        match missing {
            None => return,
            Some(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("{base} never served all of {ids:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raft_group_replicates_and_fails_over_through_the_server() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let paths: Vec<_> = dirs.iter().map(|d| d.path().to_path_buf()).collect();
    let http = reqwest::Client::new();

    let nodes = boot_raft_group(&paths).await;
    for n in &nodes {
        wait_ready(&http, &n.base).await;
    }
    let all: Vec<&Node> = nodes.iter().collect();

    // Create the collection + load the first batch via the leader.
    leader_write(
        &http,
        &all,
        "/v1/collections",
        &json!({"name": "c", "dim": 4, "metric": "l2"}),
        Some(reqwest::StatusCode::CONFLICT),
    )
    .await;
    let first: Vec<u32> = (0..6).collect();
    let leader = leader_write(
        &http,
        &all,
        "/v1/collections/c/points",
        &points_body(&first),
        None,
    )
    .await;

    // Quorum-committed: every voter — leader and both followers — serves all six.
    for n in &nodes {
        await_all_present(&http, &n.base, &first).await;
    }

    // A follower refuses the write with 421 "not the leader" (no split write path).
    let follower = nodes.iter().find(|n| n.base != leader).unwrap();
    let resp = http
        .post(format!("{}/v1/collections/c/points", follower.base))
        .json(&points_body(&[99]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::MISDIRECTED_REQUEST,
        "a follower must reject the write as not-the-leader"
    );

    // Kill the leader's process. The survivors elect a new leader automatically.
    let dead = nodes.iter().find(|n| n.base == leader).unwrap();
    dead.server.abort();
    let survivors: Vec<&Node> = nodes.iter().filter(|n| n.base != leader).collect();

    // Post-failover writes are accepted by the new leader and applied everywhere.
    let second: Vec<u32> = (6..10).collect();
    leader_write(
        &http,
        &survivors,
        "/v1/collections/c/points",
        &points_body(&second),
        None,
    )
    .await;

    // No acknowledged write is lost: each survivor serves all ten — the six from
    // before the failover and the four after it.
    let everything: Vec<u32> = (0..10).collect();
    for n in &survivors {
        await_all_present(&http, &n.base, &everything).await;
    }

    // The Raft delete path also commits through consensus: deleting two points on
    // the new leader removes them from every survivor.
    leader_delete(&http, &survivors, &[0, 9]).await;
    for n in &survivors {
        for _ in 0..240 {
            if payload_i(&http, &n.base, 0).await.is_none()
                && payload_i(&http, &n.base, 9).await.is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            payload_i(&http, &n.base, 0).await,
            None,
            "{} still has p0",
            n.base
        );
        assert_eq!(
            payload_i(&http, &n.base, 9).await,
            None,
            "{} still has p9",
            n.base
        );
        // A surviving point is untouched.
        assert_eq!(payload_i(&http, &n.base, 5).await, Some(5));
    }

    for n in survivors {
        n.server.abort();
    }
}
