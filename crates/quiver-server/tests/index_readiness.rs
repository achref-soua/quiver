// SPDX-License-Identifier: AGPL-3.0-only
//! Index readiness (ADR-0081): a search is never answered from an index that has
//! never been built.
//!
//! Before ADR-0081 the two decisions composed into a silent wrong answer. A bulk
//! load defers its index work and marks the collection stale (ADR-0045), and a
//! stale collection's read serves the *prior* snapshot rather than blocking
//! (ADR-0062) — but a collection whose index has never been built has no prior
//! snapshot, so the read was answered from nothing. `GET` reported the full point
//! count while `query` returned `200 OK` with zero matches.
//!
//! Only HNSW is live from creation; IVF, Vamana, DiskVamana and ColBERT are all
//! built on first use, so four of the five index kinds were exposed. These tests
//! drive REST.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use serde_json::{Value, json};
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

// Boot a server on ephemeral ports and return (base_url, http, tempdir).
async fn boot() -> (String, reqwest::Client, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
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
    tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;
    (base, http, tmp)
}

// Deterministic points spread over `n` positions in a `dim`-d space.
fn points(n: usize, dim: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let v: Vec<f64> = (0..dim).map(|j| ((i + j) % 17) as f64 * 0.1).collect();
            json!({"id": format!("p-{i:05}"), "vector": v})
        })
        .collect()
}

/// The regression: the query immediately after a load must find the data, for every
/// index kind whose index is built on first use rather than maintained live.
#[tokio::test]
async fn the_first_query_after_a_load_is_answered_from_a_built_index() {
    let (base, http, _tmp) = boot().await;

    // `ivf` and `disk_vamana` are the kinds that were broken; `hnsw` is the one that
    // always worked, and is here so a regression that "fixes" things by making
    // everything slow would still be visible as a behaviour change.
    for (name, index, dim) in [
        ("c_ivf", "ivf", 32usize),
        ("c_disk", "disk_vamana", 32),
        ("c_hnsw", "hnsw", 32),
    ] {
        let mut body = json!({"name": name, "dim": dim, "metric": "l2", "index": index});
        if index == "disk_vamana" {
            body["pq_subspaces"] = json!(8);
        }
        http.post(format!("{base}/v1/collections"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        http.post(format!("{base}/v1/collections/{name}/points:bulk"))
            .json(&json!({ "points": points(1500, dim) }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        // No sleep, no retry, no warm-up: the very next request after the write.
        let hits: Value = http
            .post(format!("{base}/v1/collections/{name}/query"))
            .json(&json!({"vector": vec![0.0f64; dim], "k": 10}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let matches = hits["matches"].as_array().unwrap();
        assert_eq!(
            matches.len(),
            10,
            "{index}: first query after the load returned {} matches; \
             an acknowledged write must never be silently invisible",
            matches.len()
        );
    }
}

/// Readiness is reported, so a caller can poll instead of blocking — and the count
/// and the readiness flag never disagree about whether data is searchable.
#[tokio::test]
async fn readiness_is_reported_and_agrees_with_what_a_query_returns() {
    let (base, http, _tmp) = boot().await;

    http.post(format!("{base}/v1/collections"))
        .json(&json!({"name": "c", "dim": 16, "metric": "l2", "index": "ivf"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // An empty collection is ready: returning no matches is the correct answer, and
    // there is nothing to build.
    let info: Value = http
        .get(format!("{base}/v1/collections/c"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["count"], 0);
    assert_eq!(
        info["index_ready"], true,
        "an empty collection has nothing to build and answers correctly"
    );

    http.post(format!("{base}/v1/collections/c/points:bulk"))
        .json(&json!({ "points": points(800, 16) }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // After a query has forced the build, the reported count, the readiness flag and
    // the searchable data all agree.
    let hits: Value = http
        .post(format!("{base}/v1/collections/c/query"))
        .json(&json!({"vector": vec![0.0f64; 16], "k": 5}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hits["matches"].as_array().unwrap().len(), 5);

    let info: Value = http
        .get(format!("{base}/v1/collections/c"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["count"], 800);
    assert_eq!(info["index_ready"], true);
}

/// Concurrent first readers share one build and all get correct results — the gate
/// must not let some of them through to the empty snapshot.
#[tokio::test]
async fn concurrent_first_readers_all_see_the_data() {
    let (base, http, _tmp) = boot().await;

    http.post(format!("{base}/v1/collections"))
        .json(&json!({"name": "c", "dim": 24, "metric": "l2", "index": "ivf"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    http.post(format!("{base}/v1/collections/c/points:bulk"))
        .json(&json!({ "points": points(1200, 24) }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // Fire the first reads together, so they race the same unbuilt index.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let http = http.clone();
        let url = format!("{base}/v1/collections/c/query");
        tasks.push(tokio::spawn(async move {
            let hits: Value = http
                .post(url)
                .json(&json!({"vector": vec![0.0f64; 24], "k": 10}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            hits["matches"].as_array().unwrap().len()
        }));
    }
    for task in tasks {
        assert_eq!(
            task.await.unwrap(),
            10,
            "every concurrent first reader must wait for the build, not race past it"
        );
    }
}

/// Querying a collection *before* loading it must not poison the readiness cache.
///
/// An empty collection is legitimately "ready" — an empty answer is correct — but
/// that is a transient truth, unlike "an index has been built", which is permanent.
/// Caching the wrong one would let a search that ran before the load skip the gate
/// for every search after it, silently restoring exactly the bug ADR-0081 fixes.
#[tokio::test]
async fn a_query_before_the_load_does_not_poison_the_readiness_cache() {
    let (base, http, _tmp) = boot().await;

    http.post(format!("{base}/v1/collections"))
        .json(&json!({"name": "c", "dim": 16, "metric": "l2", "index": "ivf"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // Query while genuinely empty: correct, and returns nothing.
    let empty: Value = http
        .post(format!("{base}/v1/collections/c/query"))
        .json(&json!({"vector": vec![0.0f64; 16], "k": 5}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["matches"].as_array().unwrap().is_empty());

    // Now load it. The index has still never been built.
    http.post(format!("{base}/v1/collections/c/points:bulk"))
        .json(&json!({ "points": points(900, 16) }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // The very next query must wait for the first build, not sail through on a
    // cached "ready" left over from when the collection was empty.
    let loaded: Value = http
        .post(format!("{base}/v1/collections/c/query"))
        .json(&json!({"vector": vec![0.0f64; 16], "k": 10}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        loaded["matches"].as_array().unwrap().len(),
        10,
        "a search that ran while the collection was empty must not have cached readiness"
    );
}
