// SPDX-License-Identifier: AGPL-3.0-only
//! Concurrent reads (ADR-0057 / ADR-0062): the `&self` `*_snapshot` reads serve a
//! collection's current immutable snapshot — and keep serving the *prior* snapshot
//! when a write defers a rebuild, so a server runs many searches in parallel behind
//! a shared lock and never blocks them on a rebuild. These tests pin that
//! serve-prior contract and prove a built snapshot is safe to share across threads
//! with no lock at all.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams};
use serde_json::json;

// Seed a built, fresh HNSW collection of 64 points along the x-axis.
fn seed(db: &mut Database) {
    db.create_collection("c", Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine))
        .unwrap();
    for i in 0..64u32 {
        db.upsert(
            "c",
            &format!("p{i}"),
            &[i as f32, 1.0, 0.0, 0.0],
            &json!({ "i": i }),
        )
        .unwrap();
    }
    db.ensure_indexed("c").unwrap();
}

#[test]
fn snapshot_serves_prior_until_rebuilt() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(tmp.path()).unwrap();
    seed(&mut db);
    let q = [3.0, 1.0, 0.0, 0.0];

    // Fresh: a snapshot read works directly, no lock, no rebuild.
    let hits = db
        .search_snapshot("c", &q, &SearchParams::default())
        .unwrap();
    assert!(!hits.is_empty());
    assert!(!db.needs_rebuild("c").unwrap());

    // An HNSW in-place update can't be absorbed incrementally, so it defers the
    // rebuild and the index goes stale.
    db.upsert("c", "p3", &[100.0, 1.0, 0.0, 0.0], &json!({ "i": 3 }))
        .unwrap();

    // The `&self` snapshot read keeps serving the PRIOR snapshot (no error, no
    // mutation), so a reader holding only the shared lock never blocks on the
    // rebuild (ADR-0062); the deferral is reported separately via `needs_rebuild`.
    assert!(db.needs_rebuild("c").unwrap());
    assert!(
        !db.search_snapshot("c", &q, &SearchParams::default())
            .unwrap()
            .is_empty()
    );

    // The single writer resolves it; afterward the read is fresh and not stale.
    db.ensure_indexed("c").unwrap();
    assert!(!db.needs_rebuild("c").unwrap());
    assert!(
        !db.search_snapshot("c", &q, &SearchParams::default())
            .unwrap()
            .is_empty()
    );

    // The `&mut self` convenience wrappers give embedded callers read-your-writes:
    // they rebuild a deferred index before reading, for the dense and hybrid paths.
    db.upsert("c", "p4", &[200.0, 1.0, 0.0, 0.0], &json!({ "i": 4 }))
        .unwrap();
    assert!(
        !db.search("c", &q, &SearchParams::default())
            .unwrap()
            .is_empty()
    );
    assert!(!db.needs_rebuild("c").unwrap());
    db.upsert("c", "p5", &[300.0, 1.0, 0.0, 0.0], &json!({ "i": 5 }))
        .unwrap();
    assert!(
        !db.hybrid_search("c", Some(&q), None, None, &SearchParams::default(), 60.0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn many_readers_share_one_snapshot_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(tmp.path()).unwrap();
    seed(&mut db);
    let q = [3.0, 1.0, 0.0, 0.0];

    // Ground truth from a single-threaded read of the built snapshot.
    let want = db
        .search_snapshot("c", &q, &SearchParams::default())
        .unwrap()[0]
        .id
        .clone();

    // Share `&Database` across many threads. `search_snapshot` is `&self`, so the
    // readers traverse the same immutable snapshot with no lock and no data race —
    // the concurrency the server's read lock unlocks. (`Database: Send + Sync`.)
    let db = Arc::new(db);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = Arc::clone(&db);
        let want = want.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..250 {
                let hits = db
                    .search_snapshot("c", &q, &SearchParams::default())
                    .unwrap();
                assert_eq!(
                    hits[0].id, want,
                    "every concurrent reader sees the same result"
                );
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}
