// SPDX-License-Identifier: AGPL-3.0-only
//! Leader replication over gRPC (ADR-0030): a `Replicate` stream first yields a
//! logical snapshot of current state, then the live commit tail. Hermetic — one
//! in-process leader, a loopback gRPC client; no follower process.

// A test harness; panics are the failure signal (ADR-0017 scopes the
// unwrap/expect ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
use quiver_server::{Config, serve};
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

fn point(id: &str, vector: Vec<f32>) -> v1::Point {
    v1::Point {
        id: id.to_owned(),
        vector,
        payload: b"{}".to_vec(),
    }
}

// The external id carried by a replicated upsert op.
fn upsert_id(op: &v1::ReplicationOp) -> Option<String> {
    match op.op.as_ref()? {
        v1::replication_op::Op::Upsert(u) => Some(u.external_id.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn replicate_streams_a_snapshot_then_the_live_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        api_keys: vec![],
        encryption_key: None,
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        // Insecure (no keys) admits any caller as an all-collections admin, which
        // the admin-scoped Replicate RPC requires.
        insecure: true,
    };
    tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    wait_ready(&http, &format!("http://{rest_addr}")).await;

    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();

    // Pre-replication state: one collection with two points, captured by the
    // bootstrap snapshot.
    client
        .create_collection(v1::CreateCollectionRequest {
            name: "places".to_owned(),
            dim: 2,
            metric: v1::Metric::Cosine as i32,
            index: v1::IndexKind::Hnsw as i32,
            pq_subspaces: None,
            filterable: vec![],
            multivector: false,
        })
        .await
        .unwrap();
    for (id, v) in [("a", vec![1.0, 0.0]), ("b", vec![0.0, 1.0])] {
        client
            .upsert(v1::UpsertRequest {
                collection: "places".to_owned(),
                points: vec![point(id, v)],
            })
            .await
            .unwrap();
    }

    // Open the replication stream (snapshot subscription is taken now).
    let mut stream = client
        .replicate(v1::ReplicateRequest {})
        .await
        .unwrap()
        .into_inner();

    // A post-snapshot write — it must arrive on the tail, after the snapshot.
    client
        .upsert(v1::UpsertRequest {
            collection: "places".to_owned(),
            points: vec![point("c", vec![1.0, 1.0])],
        })
        .await
        .unwrap();

    // First op is the collection creation; the next three are the upserts a, b
    // (snapshot) and c (tail).
    let first = stream.message().await.unwrap().unwrap();
    assert!(
        matches!(first.op, Some(v1::replication_op::Op::CreateCollection(_))),
        "the snapshot opens with the collection creation"
    );

    let mut ids = HashSet::new();
    for _ in 0..3 {
        let op = stream.message().await.unwrap().unwrap();
        ids.insert(upsert_id(&op).expect("an upsert op"));
    }
    assert_eq!(
        ids,
        HashSet::from(["a".to_owned(), "b".to_owned(), "c".to_owned()]),
        "snapshot upserts (a, b) then the live-tail upsert (c)"
    );
}
