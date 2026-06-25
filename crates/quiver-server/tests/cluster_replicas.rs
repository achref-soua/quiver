// SPDX-License-Identifier: AGPL-3.0-only
//! Per-shard read replicas end to end (ADR-0065 increment 2): each shard is a
//! single-writer **primary** plus an ordinary **follower replica** (ADR-0030), and
//! the router fans searches across `{primary} ∪ replicas`. The correctness oracle
//! is the same single-node baseline as increment 1 — a search served from replicas
//! must return the baseline's top-k. The dataset is small (ef_search above the
//! point count) so every HNSW is exact and the distance sequence is deterministic.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

// A booted server: its REST and gRPC base URLs and the task handle (so a test can
// `abort()` it to simulate the node going down).
struct Node {
    rest: String,
    grpc: String,
    handle: JoinHandle<()>,
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

// Boot a server on ephemeral ports. `leader_grpc` makes it a read-replica follower
// of that leader (ADR-0030); `shards`/`replicas` make it a cluster router.
async fn boot(
    data_dir: std::path::PathBuf,
    leader_grpc: Option<String>,
    shards: Vec<String>,
    replicas: Vec<String>,
) -> Node {
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest.local_addr().unwrap();
    let grpc_addr = grpc.local_addr().unwrap();
    let config = Config {
        data_dir,
        rest_addr,
        grpc_addr,
        insecure: true,
        leader_url: leader_grpc,
        cluster_shards: shards,
        cluster_replicas: replicas,
        ..Default::default()
    };
    let handle = tokio::spawn(async move {
        let _ = serve(config, rest, grpc).await;
    });
    Node {
        rest: format!("http://{rest_addr}"),
        grpc: format!("http://{grpc_addr}"),
        handle,
    }
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

// The k smallest distances for `q`, sorted — compared as a sequence (not by id), so
// equal-distance ties at the k-boundary can break either way without a false fail.
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

fn close(got: &[f32], want: &[f32]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-4)
}

// A few representative queries the router and baseline must agree on.
const QUERIES: [u32; 4] = [0, 17, 63, 119];

// Poll a replica's REST search until it mirrors its **primary** for every query (a
// replica holds only its shard's slice, not the whole dataset, so the primary — not
// the cluster-wide baseline — is its oracle). A follower defers and runs its index
// rebuild off-lock (ADR-0062), so a search may serve the prior snapshot until the
// rebuild lands — eventual consistency, in keeping with async replication.
async fn wait_caught_up(http: &reqwest::Client, replica: &str, primary: &str) {
    for _ in 0..300 {
        let mut ok = true;
        for qi in QUERIES {
            let q = vec_for(qi);
            let got = top_scores(http, replica, &q, 10).await;
            let want = top_scores(http, primary, &q, 10).await;
            // The primary holds part of the data, so its own top-k may be fewer than
            // 10; the replica must reproduce exactly that sequence.
            if got.is_empty() || !close(&got, &want) {
                ok = false;
                break;
            }
        }
        if ok {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("replica {replica} did not catch up to its primary {primary}");
}

#[tokio::test]
async fn cluster_replicas_serve_reads_matching_single_node() {
    // data dirs: 2 primaries, 2 replicas, 1 router, 1 baseline.
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let http = reqwest::Client::new();

    // Two primary shards.
    let p0 = boot(dirs[0].path().into(), None, vec![], vec![]).await;
    let p1 = boot(dirs[1].path().into(), None, vec![], vec![]).await;
    wait_ready(&http, &p0.rest).await;
    wait_ready(&http, &p1.rest).await;
    // One follower replica per shard, each pointed at its primary's gRPC.
    let r0 = boot(dirs[2].path().into(), Some(p0.grpc.clone()), vec![], vec![]).await;
    let r1 = boot(dirs[3].path().into(), Some(p1.grpc.clone()), vec![], vec![]).await;
    wait_ready(&http, &r0.rest).await;
    wait_ready(&http, &r1.rest).await;
    // The router: primaries as shards, the followers declared as their replicas.
    let router = boot(
        dirs[4].path().into(),
        None,
        vec![p0.rest.clone(), p1.rest.clone()],
        vec![format!("0={}", r0.rest), format!("1={}", r1.rest)],
    )
    .await;
    let baseline = boot(dirs[5].path().into(), None, vec![], vec![]).await;
    wait_ready(&http, &router.rest).await;
    wait_ready(&http, &baseline.rest).await;

    // Create + write through the router (broadcast create, sharded write) and to the
    // baseline. Replicas pick up both via replication.
    create(&http, &router.rest).await;
    create(&http, &baseline.rest).await;
    upsert_all(&http, &router.rest, 120).await;
    upsert_all(&http, &baseline.rest, 120).await;

    // (1) Writes went to the primaries, sharded — never to the replicas directly.
    let (c0, c1) = (count(&http, &p0.rest).await, count(&http, &p1.rest).await);
    assert!(c0 > 0 && c1 > 0, "write did not shard: p0={c0} p1={c1}");
    assert_eq!(c0 + c1, 120, "points lost or duplicated across shards");

    // Wait until each replica mirrors its primary and is queryable.
    wait_caught_up(&http, &r0.rest, &p0.rest).await;
    wait_caught_up(&http, &r1.rest, &p1.rest).await;

    // The router agrees with the single-node baseline while everything is up
    // (round-robin already routes some shard reads to replicas).
    for qi in QUERIES {
        let q = vec_for(qi);
        let got = top_scores(&http, &router.rest, &q, 10).await;
        let want = top_scores(&http, &baseline.rest, &q, 10).await;
        assert_eq!(got.len(), 10, "router returned {} hits", got.len());
        assert!(close(&got, &want), "router != baseline for q{qi}");
    }

    // (3) A replica refuses a direct external write (read-only follower): a mis-route
    // can never corrupt a replica.
    let denied = http
        .post(format!("{}/v1/collections/c/points", r0.rest))
        .json(&json!({"points": [{"id": "x", "vector": vec_for(1)}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);

    // (2) Stop BOTH primaries: the router's only live read targets are the replicas,
    // so every shard's slice is now served from a replica. The top-k must still equal
    // the baseline — a deterministic proof the replicas serve correct reads.
    p0.handle.abort();
    p1.handle.abort();
    for qi in QUERIES {
        let q = vec_for(qi);
        let got = top_scores(&http, &router.rest, &q, 10).await;
        assert_eq!(
            got.len(),
            10,
            "replica-only router returned {} hits",
            got.len()
        );
        let want = top_scores(&http, &baseline.rest, &q, 10).await;
        assert!(
            close(&got, &want),
            "replica-served top-k != baseline for q{qi}"
        );
    }
}

#[tokio::test]
async fn router_tolerates_a_down_replica() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let http = reqwest::Client::new();

    // A single shard with one replica, a router, and a baseline.
    let primary = boot(dirs[0].path().into(), None, vec![], vec![]).await;
    wait_ready(&http, &primary.rest).await;
    let replica = boot(
        dirs[1].path().into(),
        Some(primary.grpc.clone()),
        vec![],
        vec![],
    )
    .await;
    wait_ready(&http, &replica.rest).await;
    let router = boot(
        dirs[2].path().into(),
        None,
        vec![primary.rest.clone()],
        vec![format!("0={}", replica.rest)],
    )
    .await;
    let baseline = boot(dirs[3].path().into(), None, vec![], vec![]).await;
    wait_ready(&http, &router.rest).await;
    wait_ready(&http, &baseline.rest).await;

    create(&http, &router.rest).await;
    create(&http, &baseline.rest).await;
    upsert_all(&http, &router.rest, 80).await;
    upsert_all(&http, &baseline.rest, 80).await;
    wait_caught_up(&http, &replica.rest, &primary.rest).await;

    // (4) Kill the replica. Round-robin reads that would have hit it now fall through
    // to the primary, so the router keeps answering correctly.
    replica.handle.abort();
    for qi in QUERIES {
        let q = vec_for(qi);
        // Several reads so the round-robin definitely lands on the (dead) replica
        // slot at least once and must fall back to the primary.
        for _ in 0..4 {
            let got = top_scores(&http, &router.rest, &q, 10).await;
            let want = top_scores(&http, &baseline.rest, &q, 10).await;
            assert_eq!(
                got.len(),
                10,
                "router returned {} hits after replica down",
                got.len()
            );
            assert!(
                close(&got, &want),
                "router != baseline after replica down for q{qi}"
            );
        }
    }
}
