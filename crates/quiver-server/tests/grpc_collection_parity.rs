// SPDX-License-Identifier: AGPL-3.0-only
//! gRPC collection-surface parity with REST.
//!
//! Binary quantization for the disk graph (ADR-0074) was reachable over REST and the
//! MCP server but not gRPC, which passed a hard-coded `false`: a gRPC client could
//! not create the collection it wanted, and `GetCollection` could not report what it
//! had got. Index readiness (ADR-0081) is the same shape — reported over REST, so it
//! belongs on the gRPC `Collection` message too. A 1.0 wire surface should not have
//! one protocol quietly less capable than the other.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
use quiver_server::{Config, serve};
use tokio::net::TcpListener;

fn auth_request<T>(key: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {key}").parse().expect("ascii metadata"),
    );
    request
}

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

/// A gRPC client can create a binary-quantized disk collection and read the flag
/// back — the round trip that a hard-coded `false` made impossible.
#[tokio::test]
async fn grpc_round_trips_binary_quantization_and_reports_index_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();
    let key = "test-key";

    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        insecure: true,
        api_keys: vec![key.to_owned().into()],
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    wait_ready(&http, &format!("http://{rest_addr}")).await;

    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();

    // Create a disk-graph collection that navigates by binary codes.
    let created = client
        .create_collection(auth_request(
            key,
            v1::CreateCollectionRequest {
                name: "bq".to_owned(),
                dim: 32,
                metric: v1::Metric::L2 as i32,
                index: v1::IndexKind::DiskVamana as i32,
                pq_subspaces: Some(8),
                filterable: Vec::new(),
                multivector: false,
                vector_encryption: v1::VectorEncryption::None as i32,
                binary: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        created.binary,
        "the create response must echo the binary flag it was asked for"
    );

    // And it survives a round trip through the descriptor, not just the response.
    let fetched = client
        .get_collection(auth_request(
            key,
            v1::GetCollectionRequest {
                name: "bq".to_owned(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        fetched.binary,
        "GetCollection must report the stored binary flag"
    );
    assert_eq!(fetched.index, v1::IndexKind::DiskVamana as i32);
    assert!(
        fetched.index_ready,
        "an empty collection is ready: there is nothing to build (ADR-0081)"
    );

    // The default stays false, so this is opt-in and the flag is not merely echoed.
    client
        .create_collection(auth_request(
            key,
            v1::CreateCollectionRequest {
                name: "pq".to_owned(),
                dim: 32,
                metric: v1::Metric::L2 as i32,
                index: v1::IndexKind::DiskVamana as i32,
                pq_subspaces: Some(8),
                filterable: Vec::new(),
                multivector: false,
                vector_encryption: v1::VectorEncryption::None as i32,
                binary: false,
            },
        ))
        .await
        .unwrap();
    let plain = client
        .get_collection(auth_request(
            key,
            v1::GetCollectionRequest {
                name: "pq".to_owned(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!plain.binary, "product quantization stays the default");

    server.abort();
}
