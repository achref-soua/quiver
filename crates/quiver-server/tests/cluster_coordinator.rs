// SPDX-License-Identifier: AGPL-3.0-only
//! The cluster coordinator + dynamic router refresh (ADR-0066 increment 3b): a
//! coordinator owns the versioned shard map; a router refreshes it from the
//! coordinator and picks up an added or removed shard **with no restart**. No data
//! migration yet (that is 3c) — these tests exercise the membership/refresh path,
//! using a freshly added shard that owns only the keys that now hash to it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;

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

// Boot a server on ephemeral ports; return its REST base URL. `cfg` customises it
// (a shard, a router, or the coordinator).
async fn boot(mut cfg: Config) -> String {
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    cfg.rest_addr = rest.local_addr().unwrap();
    cfg.grpc_addr = grpc.local_addr().unwrap();
    cfg.insecure = true;
    let base = format!("http://{}", cfg.rest_addr);
    tokio::spawn(async move {
        let _ = serve(cfg, rest, grpc).await;
    });
    base
}

fn vec_for(i: u32) -> Vec<f32> {
    (0..8)
        .map(|j| (((i * 7 + j * 13) % 91) as f32) / 9.0)
        .collect()
}

async fn create(http: &reqwest::Client, base: &str) {
    http.post(format!("{base}/v1/collections"))
        .json(&json!({"name": "c", "dim": 8, "metric": "l2"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

// Upsert ids `lo..hi` (so successive batches use fresh ids — no cross-shard dups).
async fn upsert_range(http: &reqwest::Client, base: &str, lo: u32, hi: u32) {
    let points: Vec<Value> = (lo..hi)
        .map(|i| json!({"id": format!("p{i}"), "vector": vec_for(i), "payload": {"i": i}}))
        .collect();
    http.post(format!("{base}/v1/collections/c/points"))
        .json(&json!({ "points": points }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

async fn count(http: &reqwest::Client, base: &str) -> u64 {
    let resp: Value = http
        .get(format!("{base}/v1/collections/c"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["count"].as_u64().unwrap_or(0)
}

async fn top_scores(http: &reqwest::Client, base: &str, q: &[f32], k: usize) -> Vec<f32> {
    let resp: Value = http
        .post(format!("{base}/v1/collections/c/query"))
        .json(&json!({"vector": q, "k": k, "ef_search": 256, "with_payload": false, "with_vector": false}))
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
                .map(|m| m["score"].as_f64().unwrap() as f32)
                .collect()
        })
        .unwrap_or_default()
}

fn close(got: &[f32], want: &[f32]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-4)
}

// The map version the router has currently adopted (its `/cluster/map` endpoint).
async fn router_map_version(http: &reqwest::Client, router: &str) -> u64 {
    let resp: Value = http
        .get(format!("{router}/cluster/map"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["version"].as_u64().unwrap_or(0)
}

// Poll until the router has refreshed to at least `version` (or panic on timeout).
async fn wait_router_version(http: &reqwest::Client, router: &str, version: u64) {
    for _ in 0..400 {
        if router_map_version(http, router).await >= version {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router did not refresh to map version {version}");
}

#[tokio::test]
async fn router_refreshes_membership_from_the_coordinator() {
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let state = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();

    // Three shard servers (s2 added later) and a single-node baseline.
    let s0 = boot(Config {
        data_dir: dirs[0].path().into(),
        ..Default::default()
    })
    .await;
    let s1 = boot(Config {
        data_dir: dirs[1].path().into(),
        ..Default::default()
    })
    .await;
    let s2 = boot(Config {
        data_dir: dirs[2].path().into(),
        ..Default::default()
    })
    .await;
    let baseline = boot(Config {
        data_dir: dirs[3].path().into(),
        ..Default::default()
    })
    .await;
    for b in [&s0, &s1, &s2, &baseline] {
        wait_ready(&http, b).await;
    }

    // The coordinator, seeded with shards {0:s0, 1:s1}, and a router pointed at it
    // (also seeded with the same bootstrap set).
    let coordinator = boot(Config {
        data_dir: dirs[4].path().into(),
        coordinator: true,
        coordinator_state: Some(state.path().join("coord.json")),
        raft_node_id: None,
        raft_members: Vec::new(),
        cluster_shards: vec![s0.clone(), s1.clone()],
        ..Default::default()
    })
    .await;
    wait_ready(&http, &coordinator).await;
    let router = boot(Config {
        data_dir: dirs[5].path().into(),
        cluster_shards: vec![s0.clone(), s1.clone()],
        coordinator_url: Some(coordinator.clone()),
        ..Default::default()
    })
    .await;
    wait_ready(&http, &router).await;

    // The collection lives on every shard (s2 pre-provisioned for when it joins) and
    // the baseline. Write the first batch through the router; it shards to s0/s1.
    for b in [&s0, &s1, &s2, &baseline] {
        create(&http, b).await;
    }
    upsert_range(&http, &router, 0, 90).await;
    upsert_range(&http, &baseline, 0, 90).await;
    assert_eq!(count(&http, &s2).await, 0, "s2 is not in the cluster yet");
    assert_eq!(
        router_map_version(&http, &router).await,
        0,
        "seed map is v0"
    );

    // Add s2 through the coordinator — version bumps to 1.
    let added: Value = http
        .post(format!("{coordinator}/cluster/shards"))
        .json(&json!({ "primary_url": s2 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(added["version"].as_u64().unwrap(), 1);
    assert_eq!(added["shards"].as_array().unwrap().len(), 3);

    // The router refreshes to v1 with no restart, then routes part of a fresh batch
    // to the new shard.
    wait_router_version(&http, &router, 1).await;
    upsert_range(&http, &router, 90, 180).await;
    upsert_range(&http, &baseline, 90, 180).await;
    assert!(
        count(&http, &s2).await > 0,
        "router did not route to the added shard after refresh"
    );

    // The coordinator's health endpoint reports all three live shards by id.
    let health: Value = http
        .get(format!("{coordinator}/cluster/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for id in ["0", "1", "2"] {
        assert_eq!(health[id], json!(true), "shard {id} should be healthy");
    }
    // Removing a non-existent shard is a 400, not a silent success.
    let bad = http
        .request(
            reqwest::Method::DELETE,
            format!("{coordinator}/cluster/shards/99"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // With all three shards live, scatter-gather still equals the single-node truth.
    for qi in [5u32, 73, 150] {
        let q = vec_for(qi);
        let got = top_scores(&http, &router, &q, 10).await;
        let want = top_scores(&http, &baseline, &q, 10).await;
        assert!(close(&got, &want), "router != baseline after add for q{qi}");
    }

    // Remove s1 through the coordinator — version bumps to 2. (No migration in 3b;
    // s1's data is simply no longer routed to.)
    let removed: Value = http
        .request(
            reqwest::Method::DELETE,
            format!("{coordinator}/cluster/shards/1"),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(removed["version"].as_u64().unwrap(), 2);
    assert_eq!(removed["shards"].as_array().unwrap().len(), 2);

    // The router refreshes to v2 and stops routing to s1: a fresh batch lands only on
    // the surviving shards, leaving s1's count unchanged.
    wait_router_version(&http, &router, 2).await;
    let s1_before = count(&http, &s1).await;
    upsert_range(&http, &router, 180, 240).await;
    assert_eq!(
        count(&http, &s1).await,
        s1_before,
        "router still routed to the removed shard"
    );
}

#[tokio::test]
async fn coordinator_persists_membership_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("coord.json");
    let http = reqwest::Client::new();

    let s0 = boot(Config {
        data_dir: dir.path().join("s0"),
        ..Default::default()
    })
    .await;
    let s1 = boot(Config {
        data_dir: dir.path().join("s1"),
        ..Default::default()
    })
    .await;
    wait_ready(&http, &s0).await;
    wait_ready(&http, &s1).await;

    // A coordinator seeded with one shard; add a second, then "restart" it (a fresh
    // process pointed at the same state file) and confirm it recovers v1 + both
    // shards, and that the next id is not reused.
    let coord1 = boot(Config {
        data_dir: dir.path().join("c1"),
        coordinator: true,
        coordinator_state: Some(state_path.clone()),
        raft_node_id: None,
        raft_members: Vec::new(),
        cluster_shards: vec![s0.clone()],
        ..Default::default()
    })
    .await;
    wait_ready(&http, &coord1).await;
    http.post(format!("{coord1}/cluster/shards"))
        .json(&json!({ "primary_url": s1 }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let coord2 = boot(Config {
        data_dir: dir.path().join("c2"),
        coordinator: true,
        coordinator_state: Some(state_path.clone()),
        raft_node_id: None,
        raft_members: Vec::new(),
        // A different bootstrap set is ignored because the state file exists.
        cluster_shards: vec!["http://ignored:6333".into()],
        ..Default::default()
    })
    .await;
    wait_ready(&http, &coord2).await;
    let recovered: Value = http
        .get(format!("{coord2}/cluster/map"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        recovered["version"].as_u64().unwrap(),
        1,
        "recovered version"
    );
    let ids: Vec<u64> = recovered["shards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [0, 1], "recovered both shards with their ids");

    // The next added shard gets id 2 — a recovered coordinator never reuses an id.
    let next: Value = http
        .post(format!("{coord2}/cluster/shards"))
        .json(&json!({ "primary_url": s0 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new_id = next["shards"].as_array().unwrap().last().unwrap()["id"]
        .as_u64()
        .unwrap();
    assert_eq!(new_id, 2, "id counter persisted across restart");
}
