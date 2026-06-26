// SPDX-License-Identifier: AGPL-3.0-only
//! Online slice migration data plane (ADR-0066 increment 3c): when a shard joins,
//! the router dual-writes the migrating slice to its donor, serves searches from the
//! donor (excluding the joining shard) and gets from the donor, then — after the
//! coordinator flips ownership (`promote`) — routes the slice to the new shard while
//! a dedup gather absorbs the brief donor/owner overlap. The invariants under test:
//! **the slice is queryable throughout** and **no acknowledged write is lost** across
//! the flip. The automated copy loop is increment 3c-ii; here the migration is driven
//! step by step (the copy = re-sending the data through the migration-aware router).
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

// The router and baseline must agree on every query (the slice is queryable +
// correct). Polled, because an upsert's index rebuild runs off-lock (ADR-0062): a
// search may serve the prior snapshot until the rebuild lands, so router and baseline
// converge rather than matching at a single instant — the same eventual-consistency
// the cluster's replicas/migration both rely on.
async fn assert_router_matches_baseline(http: &reqwest::Client, router: &str, baseline: &str) {
    for _ in 0..400 {
        let mut all = true;
        for qi in [3u32, 41, 88, 130] {
            let q = vec_for(qi);
            let got = top_scores(http, router, &q, 10).await;
            let want = top_scores(http, baseline, &q, 10).await;
            if !close(&got, &want) {
                all = false;
                break;
            }
        }
        if all {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router never converged to the single-node baseline");
}

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

async fn wait_router_version(http: &reqwest::Client, router: &str, version: u64) {
    for _ in 0..400 {
        if router_map_version(http, router).await >= version {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router did not refresh to map version {version}");
}

// Every id `0..n` must be retrievable through the router (no acked write lost).
async fn assert_all_present(http: &reqwest::Client, router: &str, n: u32) {
    for i in 0..n {
        let resp = http
            .get(format!("{router}/v1/collections/c/points/p{i}"))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "point p{i} lost (status {})",
            resp.status()
        );
    }
}

#[tokio::test]
async fn online_join_migration_loses_no_writes_and_stays_queryable() {
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let state = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();

    // Two donors, one recipient, a coordinator (seed {0,1}), a router, a baseline.
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
    let coordinator = boot(Config {
        data_dir: dirs[4].path().into(),
        coordinator: true,
        coordinator_state: Some(state.path().join("coord.json")),
        autoscale: Default::default(),
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

    for b in [&s0, &s1, &s2, &baseline] {
        create(&http, b).await;
    }
    // Initial data lands on the two donors.
    upsert_range(&http, &router, 0, 90).await;
    upsert_range(&http, &baseline, 0, 90).await;
    assert_eq!(count(&http, &s2).await, 0);
    assert_router_matches_baseline(&http, &router, &baseline).await;

    // --- Begin migration: s2 joins (v1). Its slice is still owned/served by donors.
    http.post(format!("{coordinator}/cluster/shards/joining"))
        .json(&json!({ "primary_url": s2 }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_router_version(&http, &router, 1).await;
    // Queryable throughout: while s2 is joining, the router serves from the donors.
    assert_router_matches_baseline(&http, &router, &baseline).await;
    assert_all_present(&http, &router, 90).await;

    // The "copy": re-send the data through the migration-aware router. Slice points
    // are written to the joining owner (s2) and dual-written to the donor; non-slice
    // points are idempotent. (Increment 3c-ii automates this in the coordinator.)
    upsert_range(&http, &router, 0, 90).await;
    assert!(
        count(&http, &s2).await > 0,
        "slice did not copy to the joining shard"
    );

    // A write *during* the migration: it must survive the flip.
    upsert_range(&http, &router, 90, 150).await;
    upsert_range(&http, &baseline, 90, 150).await;
    assert_router_matches_baseline(&http, &router, &baseline).await;

    // --- Flip: promote s2 (v2). Ownership of the slice transfers atomically.
    http.post(format!("{coordinator}/cluster/shards/2/promote"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_router_version(&http, &router, 2).await;

    // After the flip the slice is served by s2; the donor may still hold a copy, which
    // the dedup gather absorbs. No acknowledged write is lost and search still matches.
    assert_router_matches_baseline(&http, &router, &baseline).await;
    assert_all_present(&http, &router, 150).await;
}

// Poll until the cluster holds exactly `expected` points total (the donors' slice
// copies have been dropped after the flip — no duplicates remain).
async fn wait_total(http: &reqwest::Client, shards: &[&str], expected: u64) {
    for _ in 0..400 {
        let mut total = 0;
        for s in shards {
            total += count(http, s).await;
        }
        if total == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("cluster never settled to {expected} points (drop incomplete?)");
}

#[tokio::test]
async fn auto_grow_migrates_the_slice_with_no_loss() {
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let state = tempfile::tempdir().unwrap();
    let http = reqwest::Client::new();

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
    let coordinator = boot(Config {
        data_dir: dirs[4].path().into(),
        coordinator: true,
        coordinator_state: Some(state.path().join("coord.json")),
        autoscale: Default::default(),
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

    // The collection is created on the donors + baseline — NOT on s2; the coordinator
    // must provision it on the new shard during the migration.
    for b in [&s0, &s1, &baseline] {
        create(&http, b).await;
    }
    upsert_range(&http, &router, 0, 120).await;
    upsert_range(&http, &baseline, 0, 120).await;

    // One call grows the cluster; the coordinator copies, flips, and drops in the
    // background. The response is the joining map (v1).
    let resp: Value = http
        .post(format!("{coordinator}/cluster/shards/grow"))
        .json(&json!({ "primary_url": s2 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["version"].as_u64().unwrap(), 1);

    // A write *during* the background migration must survive (dual-write + copy).
    tokio::time::sleep(Duration::from_millis(500)).await;
    upsert_range(&http, &router, 120, 180).await;
    upsert_range(&http, &baseline, 120, 180).await;

    // The migration flips (v2) and then drops the donors' copies, settling the cluster
    // to exactly 180 points with the new shard owning its slice.
    wait_router_version(&http, &router, 2).await;
    wait_total(&http, &[&s0, &s1, &s2], 180).await;
    assert!(count(&http, &s2).await > 0, "the new shard owns no slice");

    // Queryable + correct throughout, and every acknowledged write survived.
    assert_router_matches_baseline(&http, &router, &baseline).await;
    assert_all_present(&http, &router, 180).await;
}
