// SPDX-License-Identifier: AGPL-3.0-only
//! gRPC methods that no test reached.
//!
//! `grpc.rs` was the least-covered file in the server (63.6% lines) for a plain
//! reason: six of its seventeen service methods — `DeleteCollection`,
//! `DeletePoints`, `GetPoints`, `Fetch`, `UpsertMultiVector` and `DeleteDocuments`
//! — had no test on any path. Their REST equivalents are well covered, but REST
//! coverage proves nothing about the gRPC translation layer, which is where the
//! conversions between proto messages and engine types actually live.
//!
//! That matters more at 1.0 than it did before: the compatibility promise covers
//! the gRPC wire surface, and a method with no test is a method whose behaviour is
//! not pinned.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
use quiver_server::{Config, serve};
use serde_json::json;
use tokio::net::TcpListener;

const KEY: &str = "grpc-coverage-key";

fn auth<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {KEY}").parse().expect("ascii metadata"),
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

async fn boot() -> (QuiverClient<tonic::transport::Channel>, tempfile::TempDir) {
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
        api_keys: vec![KEY.to_owned().into()],
        ..Default::default()
    };
    tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    wait_ready(&http, &format!("http://{rest_addr}")).await;
    let client = QuiverClient::connect(format!("http://{grpc_addr}"))
        .await
        .unwrap();
    (client, tmp)
}

fn point(id: &str, vector: Vec<f32>, payload: serde_json::Value) -> v1::Point {
    v1::Point {
        id: id.to_owned(),
        vector,
        payload: serde_json::to_vec(&payload).unwrap(),
    }
}

async fn create(client: &mut QuiverClient<tonic::transport::Channel>, name: &str, dim: u32) {
    client
        .create_collection(auth(v1::CreateCollectionRequest {
            name: name.to_owned(),
            dim,
            metric: v1::Metric::L2 as i32,
            index: v1::IndexKind::Hnsw as i32,
            pq_subspaces: None,
            filterable: vec![v1::FilterableField {
                path: "topic".to_owned(),
                field_type: v1::FieldType::Keyword as i32,
            }],
            multivector: false,
            vector_encryption: v1::VectorEncryption::None as i32,
            binary: false,
        }))
        .await
        .unwrap();
}

/// `GetPoints` and `DeletePoints`: point-level reads and erasure over gRPC.
#[tokio::test]
async fn grpc_gets_and_deletes_points_by_id() {
    let (mut client, _tmp) = boot().await;
    create(&mut client, "c", 4).await;

    let upserted = client
        .upsert(auth(v1::UpsertRequest {
            collection: "c".to_owned(),
            points: vec![
                point("a", vec![1.0, 0.0, 0.0, 0.0], json!({"topic": "search"})),
                point("b", vec![0.0, 1.0, 0.0, 0.0], json!({"topic": "storage"})),
                point("c", vec![0.0, 0.0, 1.0, 0.0], json!({"topic": "ops"})),
            ],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(upserted.upserted, 3);

    // GetPoints returns exactly the requested ids, with vectors when asked.
    let got = client
        .get_points(auth(v1::GetPointsRequest {
            collection: "c".to_owned(),
            ids: vec!["a".to_owned(), "c".to_owned()],
            with_vector: true,
        }))
        .await
        .unwrap()
        .into_inner();
    let mut ids: Vec<_> = got.points.iter().map(|p| p.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["a".to_owned(), "c".to_owned()]);
    let a = got.points.iter().find(|p| p.id == "a").unwrap();
    assert_eq!(a.vector, vec![1.0, 0.0, 0.0, 0.0], "the vector round-trips");
    let payload: serde_json::Value = serde_json::from_slice(&a.payload).unwrap();
    assert_eq!(payload["topic"], "search", "the payload round-trips");

    // An absent id is omitted, not an error and not a null entry.
    let sparse = client
        .get_points(auth(v1::GetPointsRequest {
            collection: "c".to_owned(),
            ids: vec!["a".to_owned(), "nope".to_owned()],
            with_vector: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        sparse.points.len(),
        1,
        "a missing id is simply not returned"
    );

    let deleted = client
        .delete_points(auth(v1::DeletePointsRequest {
            collection: "c".to_owned(),
            ids: vec!["b".to_owned(), "nope".to_owned()],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(deleted.deleted, 1, "only the point that existed is counted");

    let after = client
        .get_points(auth(v1::GetPointsRequest {
            collection: "c".to_owned(),
            ids: vec!["b".to_owned()],
            with_vector: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(after.points.is_empty(), "the deleted point is gone");
}

/// `Fetch`: filtered, limit-bounded scan over gRPC — the export / re-embed path.
#[tokio::test]
async fn grpc_fetches_by_filter_and_respects_the_limit() {
    let (mut client, _tmp) = boot().await;
    create(&mut client, "c", 4).await;

    let mut points = Vec::new();
    for i in 0..6 {
        let topic = if i % 2 == 0 { "search" } else { "storage" };
        points.push(point(
            &format!("p{i}"),
            vec![i as f32, 0.0, 0.0, 0.0],
            json!({ "topic": topic }),
        ));
    }
    client
        .upsert(auth(v1::UpsertRequest {
            collection: "c".to_owned(),
            points,
        }))
        .await
        .unwrap();

    // Unfiltered fetch, bounded by the limit.
    let bounded = client
        .fetch(auth(v1::FetchRequest {
            collection: "c".to_owned(),
            filter: Vec::new(),
            limit: 2,
            with_payload: true,
            with_vector: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(bounded.matches.len(), 2, "the limit bounds the page");

    // Filtered fetch returns only the matching points.
    let filter =
        serde_json::to_vec(&json!({"eq": {"field": "topic", "value": "storage"}})).unwrap();
    let filtered = client
        .fetch(auth(v1::FetchRequest {
            collection: "c".to_owned(),
            filter,
            limit: 100,
            with_payload: true,
            with_vector: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        filtered.matches.len(),
        3,
        "three points carry topic=storage"
    );
    for m in &filtered.matches {
        let payload: serde_json::Value = serde_json::from_slice(&m.payload).unwrap();
        assert_eq!(payload["topic"], "storage");
    }
}

/// `UpsertMultiVector` and `DeleteDocuments`: the late-interaction document path.
#[tokio::test]
async fn grpc_upserts_and_deletes_multivector_documents() {
    let (mut client, _tmp) = boot().await;
    client
        .create_collection(auth(v1::CreateCollectionRequest {
            name: "docs".to_owned(),
            dim: 4,
            metric: v1::Metric::Cosine as i32,
            index: v1::IndexKind::Hnsw as i32,
            pq_subspaces: None,
            filterable: Vec::new(),
            multivector: true,
            vector_encryption: v1::VectorEncryption::None as i32,
            binary: false,
        }))
        .await
        .unwrap();

    let document = |id: &str, rows: Vec<Vec<f32>>| v1::MultiVectorPoint {
        id: id.to_owned(),
        vectors: rows
            .into_iter()
            .map(|values| v1::Vector { values })
            .collect(),
        payload: serde_json::to_vec(&json!({"kind": "doc"})).unwrap(),
    };

    let upserted = client
        .upsert_multi_vector(auth(v1::UpsertMultiVectorRequest {
            collection: "docs".to_owned(),
            documents: vec![
                document(
                    "d-search",
                    vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.9, 0.1, 0.0, 0.0]],
                ),
                document(
                    "d-storage",
                    vec![vec![0.0, 1.0, 0.0, 0.0], vec![0.1, 0.9, 0.0, 0.0]],
                ),
            ],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(upserted.upserted, 2, "both documents are acknowledged");

    // MaxSim ranks the document whose tokens sit at the query.
    let hits = client
        .search_multi_vector(auth(v1::SearchMultiVectorRequest {
            collection: "docs".to_owned(),
            query: vec![v1::Vector {
                values: vec![1.0, 0.0, 0.0, 0.0],
            }],
            k: 2,
            filter: Vec::new(),
            ef_search: 64,
            with_payload: false,
            with_vector: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        hits.matches[0].id, "d-search",
        "late interaction ranks it first"
    );

    let deleted = client
        .delete_documents(auth(v1::DeleteDocumentsRequest {
            collection: "docs".to_owned(),
            ids: vec!["d-search".to_owned()],
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(deleted.deleted, 1, "the document is deleted as one unit");

    let after = client
        .search_multi_vector(auth(v1::SearchMultiVectorRequest {
            collection: "docs".to_owned(),
            query: vec![v1::Vector {
                values: vec![1.0, 0.0, 0.0, 0.0],
            }],
            k: 2,
            filter: Vec::new(),
            with_payload: false,
            with_vector: false,
            ef_search: 64,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(
        after.matches.iter().all(|m| m.id != "d-search"),
        "every token row of the deleted document is gone, not just its first"
    );
}

/// `DeleteCollection`: the drop path, including that dropping twice is honest.
#[tokio::test]
async fn grpc_deletes_a_collection_and_reports_whether_it_existed() {
    let (mut client, _tmp) = boot().await;
    create(&mut client, "doomed", 4).await;

    let first = client
        .delete_collection(auth(v1::DeleteCollectionRequest {
            name: "doomed".to_owned(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(
        first.existed,
        "the first drop reports the collection existed"
    );

    let second = client
        .delete_collection(auth(v1::DeleteCollectionRequest {
            name: "doomed".to_owned(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !second.existed,
        "dropping an absent collection is not an error, but it must not claim it existed"
    );

    // And it is really gone from the listing.
    let listed = client
        .list_collections(auth(v1::ListCollectionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.collections.iter().all(|c| c.name != "doomed"));
}

/// Error mapping is part of the wire contract: a gRPC client branches on the status
/// code, so the code a failure arrives as matters as much as the success path.
#[tokio::test]
async fn grpc_maps_failures_onto_the_right_status_codes() {
    let (mut client, _tmp) = boot().await;
    create(&mut client, "c", 4).await;

    // An operation on a collection that does not exist.
    let missing = client
        .get_points(auth(v1::GetPointsRequest {
            collection: "nope".to_owned(),
            ids: vec!["a".to_owned()],
            with_vector: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(
        missing.code(),
        tonic::Code::NotFound,
        "an absent collection is NOT_FOUND, not an internal error"
    );

    // A vector whose length disagrees with the collection's declared dimension.
    let wrong_dim = client
        .upsert(auth(v1::UpsertRequest {
            collection: "c".to_owned(),
            points: vec![point("bad", vec![1.0, 0.0], json!({}))],
        }))
        .await
        .unwrap_err();
    assert_eq!(
        wrong_dim.code(),
        tonic::Code::InvalidArgument,
        "a dimension mismatch is the caller's error, not the server's"
    );

    // Creating a collection that already exists.
    let duplicate = client
        .create_collection(auth(v1::CreateCollectionRequest {
            name: "c".to_owned(),
            dim: 4,
            metric: v1::Metric::L2 as i32,
            index: v1::IndexKind::Hnsw as i32,
            pq_subspaces: None,
            filterable: Vec::new(),
            multivector: false,
            vector_encryption: v1::VectorEncryption::None as i32,
            binary: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(
        duplicate.code(),
        tonic::Code::AlreadyExists,
        "a name collision is ALREADY_EXISTS"
    );

    // And an unauthenticated call is refused before any of that matters.
    let anonymous = client
        .fetch(tonic::Request::new(v1::FetchRequest {
            collection: "c".to_owned(),
            filter: Vec::new(),
            limit: 10,
            with_payload: false,
            with_vector: false,
        }))
        .await
        .unwrap_err();
    assert_eq!(anonymous.code(), tonic::Code::Unauthenticated);
}
