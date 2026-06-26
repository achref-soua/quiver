// SPDX-License-Identifier: AGPL-3.0-only
//! Per-shard Raft write-HA **correctness gate**, server level (ADR-0067, 4b-iv).
//!
//! **No acknowledged write is lost across a leader failover.** A writer drives
//! continuous upserts through the router while the leader's *process* is killed;
//! every upsert the router acknowledged (returned 2xx) must afterwards be served
//! by a surviving voter — checked against the exact set of acked ids (the
//! single-node ground truth: an acknowledged write is durable). The assertion is
//! about *acknowledged* writes only: a write the router did not ack (e.g. one
//! attempted mid-election that exhausted the retry budget) carries no durability
//! promise and is not required to survive — exactly the Raft contract.
//!
//! The complementary safety property — **no split-brain: a minority cannot commit
//! a write** — is proven at the consensus-adapter level in
//! `quiver-server/src/raft.rs` (`a_minority_cannot_commit_a_write`), where the
//! in-process `Switchboard` can *truly* isolate nodes. A whole-process kill here
//! (`JoinHandle::abort`) stops a node's HTTP/gRPC server but not openraft's
//! background core, so it cannot fully partition a node — adequate for killing the
//! leader (the two real survivors still form a quorum) but not for isolating a
//! minority of one.
#![cfg(feature = "raft")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

// A sentinel id used only to probe which node is the leader (an idempotent
// re-upsert the leader accepts and a follower 421s); kept far above the writer's
// ids so it never collides with a counted write.
const PROBE_ID: u32 = 1_000_000;

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

async fn boot_raft_group(dirs: &[std::path::PathBuf]) -> Vec<Node> {
    let mut rests = Vec::new();
    let mut grpcs = Vec::new();
    let mut members = Vec::new();
    for (i, _) in dirs.iter().enumerate() {
        let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        members.push(format!("{}=http://{}", i + 1, grpc.local_addr().unwrap()));
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

async fn boot_router(dir: std::path::PathBuf, voters: &[String]) -> String {
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config {
        data_dir: dir,
        insecure: true,
        cluster_shards: vec![voters[0].clone()],
        cluster_replicas: voters[1..].iter().map(|u| format!("0={u}")).collect(),
        ..Default::default()
    };
    config.rest_addr = rest.local_addr().unwrap();
    config.grpc_addr = grpc.local_addr().unwrap();
    let base = format!("http://{}", config.rest_addr);
    tokio::spawn(async move {
        let _ = serve(config, rest, grpc).await;
    });
    base
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

fn point(i: u32) -> Value {
    json!({"id": format!("p{i}"), "vector": vec_for(i), "payload": {"i": i}})
}

// Create the collection through the router, retrying through the initial election.
async fn create_collection(http: &reqwest::Client, router: &str) {
    for _ in 0..240 {
        if let Ok(r) = http
            .post(format!("{router}/v1/collections"))
            .json(&json!({"name": "c", "dim": 4, "metric": "l2"}))
            .send()
            .await
            && (r.status().is_success() || r.status() == reqwest::StatusCode::CONFLICT)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("collection create never succeeded");
}

// Find the current leader by probing each node directly (the leader accepts an
// idempotent sentinel upsert; a follower replies 421).
async fn leader_index(http: &reqwest::Client, nodes: &[Node]) -> usize {
    for _ in 0..240 {
        for (i, n) in nodes.iter().enumerate() {
            if let Ok(r) = http
                .post(format!("{}/v1/collections/c/points", n.base))
                .json(&json!({ "points": [point(PROBE_ID)] }))
                .send()
                .await
                && r.status().is_success()
            {
                return i;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no leader found");
}

// Poll until `base` serves point `id` (a store read — immediately consistent once
// the committed op is applied on this voter).
async fn await_present(http: &reqwest::Client, base: &str, id: u32) {
    for _ in 0..240 {
        if let Ok(r) = http
            .get(format!("{base}/v1/collections/c/points/p{id}"))
            .send()
            .await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{base} never served p{id} (an acknowledged write was lost)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raft_acked_writes_survive_a_leader_kill_under_continuous_load() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let router_dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = dirs.iter().map(|d| d.path().to_path_buf()).collect();
    let http = reqwest::Client::new();

    let nodes = boot_raft_group(&paths).await;
    for n in &nodes {
        wait_ready(&http, &n.base).await;
    }
    let voters: Vec<String> = nodes.iter().map(|n| n.base.clone()).collect();
    let router = boot_router(router_dir.path().to_path_buf(), &voters).await;
    wait_ready(&http, &router).await;
    create_collection(&http, &router).await;

    // A writer task drives continuous upserts through the router, recording the id
    // of every write the router acknowledged (2xx). Only acked ids carry a
    // durability promise.
    let acked = Arc::new(Mutex::new(Vec::<u32>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicU32::new(0));
    let writer = {
        let http = http.clone();
        let router = router.clone();
        let acked = acked.clone();
        let stop = stop.clone();
        let next = next.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let id = next.fetch_add(1, Ordering::Relaxed);
                let ok = http
                    .post(format!("{router}/v1/collections/c/points"))
                    .json(&json!({ "points": [point(id)] }))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                if ok {
                    acked.lock().await.push(id);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };

    // Let writes flow and the leader settle, then kill the leader's process mid-load
    // and let the writer continue across the failover.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let acked_before = acked.lock().await.len();
    let leader = leader_index(&http, &nodes).await;
    nodes[leader].server.abort();
    tokio::time::sleep(Duration::from_millis(2000)).await;
    stop.store(true, Ordering::Relaxed);
    writer.await.unwrap();

    // The kill landed mid-stream: writes were acked both before and after it.
    let acked = acked.lock().await.clone();
    assert!(
        acked.len() > acked_before && acked_before > 0,
        "kill was not mid-load: {acked_before} acked before, {} after",
        acked.len()
    );

    // No acknowledged write is lost: a surviving voter serves every acked id.
    let survivor = nodes
        .iter()
        .enumerate()
        .find(|(i, _)| *i != leader)
        .map(|(_, n)| n.base.clone())
        .unwrap();
    for id in &acked {
        await_present(&http, &survivor, *id).await;
    }

    for (i, n) in nodes.into_iter().enumerate() {
        if i != leader {
            n.server.abort();
        }
    }
}
