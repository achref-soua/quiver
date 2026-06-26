// SPDX-License-Identifier: AGPL-3.0-only
//! Coordinator autoscaling — automatic scale-out (ADR-0065 increment 5).
//!
//! A coordinator with the autoscale policy enabled, fronting two shards and holding
//! one standby, watches each shard's point count. When enough points are loaded
//! through the router that the busiest shard crosses the configured high-water mark,
//! the coordinator **grows the cluster into the standby on its own** — running the
//! same safe online migration as a manual `POST /cluster/shards/grow` — so the map
//! goes from two shards to three with no operator action and no lost data.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{AutoscaleConfig, Config, serve};
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

async fn boot(mut cfg: Config, dir: std::path::PathBuf) -> String {
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    cfg.rest_addr = rest.local_addr().unwrap();
    cfg.grpc_addr = grpc.local_addr().unwrap();
    cfg.insecure = true;
    cfg.data_dir = dir;
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

async fn shard_count(http: &reqwest::Client, coordinator: &str) -> usize {
    let resp: Value = http
        .get(format!("{coordinator}/cluster/map"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["shards"].as_array().map(|a| a.len()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coordinator_scales_out_when_a_shard_exceeds_high_water() {
    let dirs: Vec<_> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    let http = reqwest::Client::new();

    // Two shards + one standby (running, but not yet in the map).
    let shard0 = boot(Config::default(), dirs[0].path().into()).await;
    let shard1 = boot(Config::default(), dirs[1].path().into()).await;
    let standby = boot(Config::default(), dirs[2].path().into()).await;
    wait_ready(&http, &shard0).await;
    wait_ready(&http, &shard1).await;
    wait_ready(&http, &standby).await;

    // A coordinator with autoscale enabled: grow into the standby when any shard
    // exceeds 20 points. Short interval/cooldown so the test runs quickly.
    let coordinator = boot(
        Config {
            coordinator: true,
            cluster_shards: vec![shard0.clone(), shard1.clone()],
            autoscale: AutoscaleConfig {
                enabled: true,
                high_water_points: 20,
                standby_urls: vec![standby.clone()],
                interval_secs: 1,
                cooldown_secs: 1,
                max_shards: 3,
            },
            ..Default::default()
        },
        dirs[3].path().into(),
    )
    .await;
    let router = boot(
        Config {
            cluster_shards: vec![shard0.clone(), shard1.clone()],
            coordinator_url: Some(coordinator.clone()),
            ..Default::default()
        },
        dirs[4].path().into(),
    )
    .await;
    wait_ready(&http, &coordinator).await;
    wait_ready(&http, &router).await;
    assert_eq!(
        shard_count(&http, &coordinator).await,
        2,
        "starts at two shards"
    );

    // Load 60 points through the router (≈30 per shard, over the 20-point mark).
    http.post(format!("{router}/v1/collections"))
        .json(&json!({"name": "c", "dim": 8, "metric": "l2"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let points: Vec<Value> = (0..60)
        .map(|i| json!({"id": format!("p{i}"), "vector": vec_for(i), "payload": {"i": i}}))
        .collect();
    http.post(format!("{router}/v1/collections/c/points"))
        .json(&json!({ "points": points }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // The coordinator scales out on its own: the map grows to three shards.
    let mut grew = false;
    for _ in 0..600 {
        if shard_count(&http, &coordinator).await >= 3 {
            grew = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(grew, "coordinator never autoscaled to a third shard");

    // The standby received its migrated slice (the grow ran the safe migration).
    let mut got_slice = false;
    for _ in 0..400 {
        if standby_points(&http, &standby).await > 0 {
            got_slice = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        got_slice,
        "the autoscaled shard never received its migrated slice"
    );
}

// The standby's point count, or 0 if the collection isn't there yet (migration not
// complete) — never panics, so it can be polled.
async fn standby_points(http: &reqwest::Client, base: &str) -> u64 {
    let Ok(r) = http.get(format!("{base}/v1/collections/c")).send().await else {
        return 0;
    };
    if !r.status().is_success() {
        return 0;
    }
    r.json::<Value>()
        .await
        .ok()
        .and_then(|v| v["count"].as_u64())
        .unwrap_or(0)
}
