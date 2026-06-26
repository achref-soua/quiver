// SPDX-License-Identifier: AGPL-3.0-only
//! Cluster router in front of a Raft shard: leader-aware write routing + the
//! "not the leader" redirect (ADR-0067, increment 4b-iii-c).
//!
//! A single shard is a **3-node Raft group** (the three voters' REST endpoints are
//! the shard's `{primary} ∪ replicas`). A stateless router fronts it. Because any
//! voter can be the leader — and the leader changes on failover — a write the
//! router sends to the shard's `primary_url` may reach a follower, which replies
//! 421 "not the leader". The router then discovers the leader among the shard's
//! voter URLs, caches it, and routes there; on a leader change it re-discovers. So:
//!
//! - writes sent through the router land on the leader and are acknowledged, and
//! - after the leader's process is killed, the router rediscovers the new leader
//!   and post-failover writes still land — no acknowledged write is lost.
//!
//! Reads (the router's scatter-gather search) already fail over across a shard's
//! read targets, so a killed voter does not stop queries.
#![cfg(feature = "raft")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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

// Boot a 3-node Raft group (the shard's three voters). gRPC listeners are bound
// first so the member set is known before any node starts.
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

// Boot a stateless router whose single shard's voters are the three Raft nodes
// ({primary} ∪ replicas).
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

// Upsert through the router. The router resolves the shard's Raft leader and
// retries internally, so this succeeds whichever voter is currently leader.
async fn router_upsert(http: &reqwest::Client, router: &str, ids: &[u32]) {
    let points: Vec<Value> = ids.iter().map(|&i| point(i)).collect();
    http.post(format!("{router}/v1/collections/c/points"))
        .json(&json!({ "points": points }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

// The set of ids the router's scatter-gather search returns for a query near `qi`.
async fn router_search_ids(http: &reqwest::Client, router: &str, qi: u32, k: usize) -> Vec<String> {
    let resp: Value = http
        .post(format!("{router}/v1/collections/c/query"))
        .json(&json!({"vector": vec_for(qi), "k": k, "ef_search": 256}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["matches"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

// Poll until the router serves every expected id (followers apply asynchronously,
// and a just-written index rebuilds off-lock — ADR-0062 — so reads converge).
async fn await_router_has(http: &reqwest::Client, router: &str, ids: &[u32]) {
    for _ in 0..300 {
        // Sweep a few queries so the union of results covers every point.
        let mut found = std::collections::HashSet::new();
        for qi in ids.iter().copied() {
            for id in router_search_ids(http, router, qi, 20).await {
                found.insert(id);
            }
        }
        if ids.iter().all(|i| found.contains(&format!("p{i}"))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router never served all of {ids:?}");
}

// Find the current leader by probing each node directly with an idempotent
// re-upsert of an existing point: the leader accepts (2xx), a follower replies 421.
async fn leader_index(http: &reqwest::Client, nodes: &[Node], existing: u32) -> usize {
    for _ in 0..240 {
        for (i, n) in nodes.iter().enumerate() {
            if let Ok(r) = http
                .post(format!("{}/v1/collections/c/points", n.base))
                .json(&json!({ "points": [point(existing)] }))
                .send()
                .await
                && r.status().is_success()
            {
                return i;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no leader found among the Raft nodes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn router_routes_writes_to_the_raft_leader_across_a_failover() {
    let node_dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let router_dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = node_dirs.iter().map(|d| d.path().to_path_buf()).collect();
    let http = reqwest::Client::new();

    let nodes = boot_raft_group(&paths).await;
    for n in &nodes {
        wait_ready(&http, &n.base).await;
    }
    let voters: Vec<String> = nodes.iter().map(|n| n.base.clone()).collect();
    let router = boot_router(router_dir.path().to_path_buf(), &voters).await;
    wait_ready(&http, &router).await;

    // Create + load the first batch entirely through the router. The router lands
    // the create and each upsert on the shard's Raft leader (redirecting off any
    // follower), retrying through the initial election.
    for _ in 0..240 {
        let r = http
            .post(format!("{router}/v1/collections"))
            .json(&json!({"name": "c", "dim": 4, "metric": "l2"}))
            .send()
            .await;
        if let Ok(r) = r
            && (r.status().is_success() || r.status() == reqwest::StatusCode::CONFLICT)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first: Vec<u32> = (0..6).collect();
    router_upsert(&http, &router, &first).await;
    await_router_has(&http, &router, &first).await;

    // Kill the leader's process; the survivors elect a new one.
    let leader = leader_index(&http, &nodes, 0).await;
    nodes[leader].server.abort();

    // Post-failover writes still land: the router rediscovers the new leader.
    let second: Vec<u32> = (6..10).collect();
    router_upsert(&http, &router, &second).await;

    // No acknowledged write is lost: the router serves all ten points (reads fail
    // over across the surviving voters).
    let everything: Vec<u32> = (0..10).collect();
    await_router_has(&http, &router, &everything).await;

    for (i, n) in nodes.into_iter().enumerate() {
        if i != leader {
            n.server.abort();
        }
    }
}
