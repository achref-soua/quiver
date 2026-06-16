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

async fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

// An insecure (admin-any) config; a set `leader_url` makes the node a follower.
fn base_config(
    data_dir: std::path::PathBuf,
    rest_addr: std::net::SocketAddr,
    grpc_addr: std::net::SocketAddr,
    leader_url: Option<String>,
) -> Config {
    Config {
        data_dir,
        rest_addr,
        grpc_addr,
        api_keys: vec![],
        encryption_key: None,
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        leader_url,
        leader_api_key: None,
        insecure: true,
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
        leader_url: None,
        leader_api_key: None,
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
            vector_encryption: v1::VectorEncryption::None as i32,
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

#[tokio::test]
async fn a_follower_mirrors_the_leader_and_refuses_writes() {
    let http = reqwest::Client::new();

    // --- A leader holding two points ---
    let leader_tmp = tempfile::tempdir().unwrap();
    let (l_rest, l_grpc) = (bind().await, bind().await);
    let (l_rest_addr, l_grpc_addr) = (l_rest.local_addr().unwrap(), l_grpc.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = serve(
            base_config(
                leader_tmp.path().to_path_buf(),
                l_rest_addr,
                l_grpc_addr,
                None,
            ),
            l_rest,
            l_grpc,
        )
        .await;
        drop(leader_tmp);
    });
    wait_ready(&http, &format!("http://{l_rest_addr}")).await;
    let mut leader = QuiverClient::connect(format!("http://{l_grpc_addr}"))
        .await
        .unwrap();
    leader
        .create_collection(v1::CreateCollectionRequest {
            name: "places".to_owned(),
            dim: 2,
            metric: v1::Metric::Cosine as i32,
            index: v1::IndexKind::Hnsw as i32,
            pq_subspaces: None,
            filterable: vec![],
            multivector: false,
            vector_encryption: v1::VectorEncryption::None as i32,
        })
        .await
        .unwrap();
    for (id, v) in [("a", vec![1.0, 0.0]), ("b", vec![0.0, 1.0])] {
        leader
            .upsert(v1::UpsertRequest {
                collection: "places".to_owned(),
                points: vec![point(id, v)],
            })
            .await
            .unwrap();
    }

    // --- A follower pointed at that leader ---
    let follower_tmp = tempfile::tempdir().unwrap();
    let (f_rest, f_grpc) = (bind().await, bind().await);
    let (f_rest_addr, f_grpc_addr) = (f_rest.local_addr().unwrap(), f_grpc.local_addr().unwrap());
    let leader_url = format!("http://{l_grpc_addr}");
    tokio::spawn(async move {
        let _ = serve(
            base_config(
                follower_tmp.path().to_path_buf(),
                f_rest_addr,
                f_grpc_addr,
                Some(leader_url),
            ),
            f_rest,
            f_grpc,
        )
        .await;
        drop(follower_tmp);
    });
    wait_ready(&http, &format!("http://{f_rest_addr}")).await;
    let mut follower = QuiverClient::connect(format!("http://{f_grpc_addr}"))
        .await
        .unwrap();

    // The follower catches up to the leader's two points.
    let mut count = 0;
    for _ in 0..250 {
        if let Ok(resp) = follower
            .get_collection(v1::GetCollectionRequest {
                name: "places".to_owned(),
            })
            .await
        {
            count = resp.into_inner().count;
            if count == 2 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(count, 2, "follower replicated both points");

    // It serves the same nearest neighbour as the leader.
    let hits = follower
        .search(v1::SearchRequest {
            collection: "places".to_owned(),
            vector: vec![1.0, 0.0],
            k: 1,
            filter: vec![],
            ef_search: 0,
            with_payload: false,
            with_vector: false,
        })
        .await
        .unwrap()
        .into_inner()
        .matches;
    assert_eq!(hits[0].id, "a");

    // And it refuses a direct write (read-only follower).
    let denied = follower
        .upsert(v1::UpsertRequest {
            collection: "places".to_owned(),
            points: vec![point("z", vec![0.5, 0.5])],
        })
        .await;
    assert_eq!(
        denied.unwrap_err().code(),
        tonic::Code::PermissionDenied,
        "a follower rejects writes"
    );
}
