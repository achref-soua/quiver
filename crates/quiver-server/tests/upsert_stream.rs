// SPDX-License-Identifier: AGPL-3.0-only
//! Client-streaming bulk upsert over gRPC (ADR-0045 fast-follow): a client streams
//! chunks of points for one collection; the server buffers the stream and performs
//! a single bulk load (one fsync + one index build), and the points are then
//! searchable. A stream that mixes collections is rejected.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

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

fn chunk(collection: &str, ids: &[(&str, [f32; 4])]) -> v1::UpsertRequest {
    v1::UpsertRequest {
        collection: collection.to_owned(),
        points: ids
            .iter()
            .map(|(id, v)| v1::Point {
                id: (*id).to_owned(),
                vector: v.to_vec(),
                payload: Vec::new(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn grpc_upsert_stream_bulk_loads_then_is_searchable() {
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
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // The collection must exist before streaming points into it.
    http.post(format!("{base}/v1/collections"))
        .json(&serde_json::json!({"name": "kb", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap();

    let mut client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();

    // Stream three chunks for "kb" → one bulk load of four points.
    let stream = tokio_stream::iter(vec![
        chunk(
            "kb",
            &[("a", [1.0, 0.0, 0.0, 0.0]), ("b", [0.0, 1.0, 0.0, 0.0])],
        ),
        chunk("kb", &[("c", [0.0, 0.0, 1.0, 0.0])]),
        // A later chunk may leave `collection` empty; it inherits the first.
        chunk("", &[("d", [0.0, 0.0, 0.0, 1.0])]),
    ]);
    let resp = client.upsert_stream(stream).await.unwrap().into_inner();
    assert_eq!(resp.upserted, 4);

    // The bulk-loaded points are searchable.
    let found = client
        .search(tonic::Request::new(v1::SearchRequest {
            collection: "kb".to_owned(),
            vector: vec![1.0, 0.0, 0.0, 0.0],
            k: 4,
            ef_search: 0,
            filter: Vec::new(),
            with_payload: false,
            with_vector: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.matches.len(), 4);
    assert_eq!(found.matches[0].id, "a");

    // A stream whose chunks target different collections is rejected.
    let mixed = tokio_stream::iter(vec![
        chunk("kb", &[("x", [1.0, 0.0, 0.0, 0.0])]),
        chunk("other", &[("y", [0.0, 1.0, 0.0, 0.0])]),
    ]);
    let err = client.upsert_stream(mixed).await;
    assert!(err.is_err(), "mixed-collection stream must error");

    server.abort();
}
