// SPDX-License-Identifier: AGPL-3.0-only
//! Cluster router end to end (ADR-0065 increment 1): two shard servers behind a
//! router shard writes and scatter-gather searches. The correctness oracle is a
//! **single-node baseline** holding the same data — the router's top-k must equal
//! it. The dataset is small so HNSW is exact and the comparison is deterministic.
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

// Boot a server with the given config on ephemeral ports; return its REST base URL.
async fn boot(mut config: Config) -> String {
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.rest_addr = rest.local_addr().unwrap();
    config.grpc_addr = grpc.local_addr().unwrap();
    config.insecure = true;
    let base = format!("http://{}", config.rest_addr);
    tokio::spawn(async move {
        let _ = serve(config, rest, grpc).await;
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

async fn upsert_all(http: &reqwest::Client, base: &str, n: u32) {
    let points: Vec<Value> = (0..n)
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

// The k smallest distances for `q`, sorted. ef_search is above the point count so
// every HNSW (per-shard and the single-node baseline) is exact, and we compare the
// distance *sequence* rather than ids — equal-distance ties at the k-boundary are
// broken arbitrarily, so the ids can legitimately differ while the distances match.
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
        .unwrap()
        .iter()
        .map(|m| m["score"].as_f64().unwrap() as f32)
        .collect()
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
    resp["count"].as_u64().unwrap()
}

#[tokio::test]
async fn cluster_router_matches_single_node_ground_truth() {
    let _tmp = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let http = reqwest::Client::new();

    // Two shard servers, a router in front of them, and a single-node baseline.
    let shard0 = boot(Config {
        data_dir: _tmp.0.path().into(),
        ..Default::default()
    })
    .await;
    let shard1 = boot(Config {
        data_dir: _tmp.1.path().into(),
        ..Default::default()
    })
    .await;
    wait_ready(&http, &shard0).await;
    wait_ready(&http, &shard1).await;
    let router = boot(Config {
        data_dir: _tmp.2.path().into(),
        cluster_shards: vec![shard0.clone(), shard1.clone()],
        ..Default::default()
    })
    .await;
    let baseline = boot(Config {
        data_dir: _tmp.3.path().into(),
        ..Default::default()
    })
    .await;
    wait_ready(&http, &router).await;
    wait_ready(&http, &baseline).await;

    // Create the collection (router broadcasts to both shards) + the baseline.
    create(&http, &router).await;
    create(&http, &baseline).await;

    // Write 120 points through the router (split across shards) and to the baseline.
    upsert_all(&http, &router, 120).await;
    upsert_all(&http, &baseline, 120).await;

    // The write actually sharded: each shard holds part of the data, summing to all.
    let (c0, c1) = (count(&http, &shard0).await, count(&http, &shard1).await);
    assert!(
        c0 > 0 && c1 > 0,
        "data did not shard: shard0={c0} shard1={c1}"
    );
    assert_eq!(c0 + c1, 120, "points lost or duplicated across shards");

    // Scatter-gather top-k must equal the single-node ground truth (the k smallest
    // distances), for several queries.
    for qi in [0u32, 17, 63, 119] {
        let q = vec_for(qi);
        let got = top_scores(&http, &router, &q, 10).await;
        let want = top_scores(&http, &baseline, &q, 10).await;
        assert_eq!(
            got.len(),
            10,
            "query {qi}: router returned {} hits",
            got.len()
        );
        for (g, w) in got.iter().zip(&want) {
            assert!(
                (g - w).abs() < 1e-4,
                "router distance {g} != single-node {w} for query {qi}"
            );
        }
    }

    // A routed get returns the point; a routed delete removes it from its shard.
    let before = count(&http, &shard0).await + count(&http, &shard1).await;
    let resp = http
        .request(
            reqwest::Method::DELETE,
            format!("{router}/v1/collections/c/points"),
        )
        .json(&json!({"ids": ["p0", "p1", "p2"]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let after = count(&http, &shard0).await + count(&http, &shard1).await;
    assert_eq!(
        after,
        before - 3,
        "routed delete did not remove across shards"
    );
}
