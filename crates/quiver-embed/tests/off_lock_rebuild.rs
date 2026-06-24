// SPDX-License-Identifier: AGPL-3.0-only
//! Off-lock rebuild lifecycle (ADR-0062): a deferred index rebuild is captured
//! under the shared read lock (`snapshot_rebuild_inputs`), built with no lock held
//! (`RebuildInputs::build`), and installed under a brief write lock
//! (`commit_rebuild`). These tests pin the three guarantees that make it correct:
//! readers keep seeing the prior snapshot until the commit, the commit refreshes
//! the snapshot, and a write that lands *during* a build is never lost (the write
//! generation makes the commit leave the index stale for the next rebuild).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use quiver_embed::{
    Database, Descriptor, DistanceMetric, Dtype, IndexKind, IndexSpec, SearchParams,
};
use serde_json::json;

const X: [f32; 4] = [0.0, 1.0, 0.0, 0.0]; // p0's seeded direction
const Z: [f32; 4] = [0.0, 0.0, 1.0, 0.0]; // a distinct direction p0 is moved to

fn desc(kind: IndexKind) -> Descriptor {
    Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine).with_index(IndexSpec {
        kind,
        pq_subspaces: None,
    })
}

// A built, fresh collection of 64 points spread along the x-axis; p0 sits at `X`.
fn seed(kind: IndexKind) -> (tempfile::TempDir, Database) {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(tmp.path()).unwrap();
    db.create_collection("c", desc(kind)).unwrap();
    for i in 0..64u32 {
        db.upsert(
            "c",
            &format!("p{i}"),
            &[i as f32 * 0.01, 1.0, 0.0, 0.0],
            &json!({ "i": i }),
        )
        .unwrap();
    }
    // p0 exactly on `X`.
    db.upsert("c", "p0", &X, &json!({ "i": 0 })).unwrap();
    db.ensure_indexed("c").unwrap();
    (tmp, db)
}

fn top(db: &Database, q: &[f32]) -> String {
    db.search_snapshot("c", q, &SearchParams::default())
        .unwrap()[0]
        .id
        .clone()
}

// Move p0 to `Z` via a bulk write, which defers a rebuild uniformly across index
// kinds (a single in-place update is absorbed by IVF and the graph indexes, but a
// bulk write always marks the index stale). The new vector is durable in the store.
fn defer_move_p0_to_z(db: &mut Database) {
    let payload = json!({ "i": 0 });
    db.upsert_bulk("c", &[("p0", &Z[..], &payload)]).unwrap();
    assert!(
        db.needs_rebuild("c").unwrap(),
        "the move must defer a rebuild"
    );
}

#[test]
fn prepare_build_commit_refreshes_the_snapshot() {
    let (_tmp, mut db) = seed(IndexKind::Hnsw);
    assert_eq!(top(&db, &X), "p0");

    defer_move_p0_to_z(&mut db);

    // Build off-lock from the captured inputs, then install.
    let inputs = db.snapshot_rebuild_inputs("c").unwrap().expect("stale");
    let rebuilt = inputs.build().unwrap();
    let still_stale = db.commit_rebuild(rebuilt).unwrap();
    assert!(!still_stale, "no write landed during the build");
    assert!(!db.needs_rebuild("c").unwrap());

    // The snapshot now reflects the move: p0 answers a query at its new direction.
    assert_eq!(top(&db, &Z), "p0");
}

#[test]
fn snapshot_serves_prior_until_commit() {
    let (_tmp, mut db) = seed(IndexKind::Hnsw);
    defer_move_p0_to_z(&mut db);

    // Capture and build, but do NOT commit yet. The prior snapshot is still served:
    // p0 still answers a query at its OLD direction, because the live index has not
    // changed (only the store has).
    let inputs = db.snapshot_rebuild_inputs("c").unwrap().expect("stale");
    let rebuilt = inputs.build().unwrap();
    assert_eq!(top(&db, &X), "p0", "prior snapshot still served pre-commit");

    db.commit_rebuild(rebuilt).unwrap();
    // After the swap, the old direction no longer resolves to p0; the new one does.
    assert_ne!(top(&db, &X), "p0", "snapshot refreshed at commit");
    assert_eq!(top(&db, &Z), "p0");
}

#[test]
fn write_during_build_is_not_lost() {
    let (_tmp, mut db) = seed(IndexKind::Hnsw);
    defer_move_p0_to_z(&mut db);

    // Capture inputs at generation G (sees p0 at Z, p1 at its seed direction).
    let inputs = db.snapshot_rebuild_inputs("c").unwrap().expect("stale");

    // A write lands "during" the build: move p1 to a third direction. This bumps the
    // write generation past what the inputs captured.
    let w = [0.0, 0.0, 0.0, 1.0];
    db.upsert("c", "p1", &w, &json!({ "i": 1 })).unwrap();

    // Committing the (now-behind) build installs it but leaves the collection stale,
    // so the p1 move cannot be silently lost.
    let rebuilt = inputs.build().unwrap();
    let still_stale = db.commit_rebuild(rebuilt).unwrap();
    assert!(
        still_stale,
        "a write during the build keeps the index stale"
    );
    assert!(db.needs_rebuild("c").unwrap());

    // A second cycle catches the newer write up; only now is p1's move visible.
    let inputs = db
        .snapshot_rebuild_inputs("c")
        .unwrap()
        .expect("still stale");
    let rebuilt = inputs.build().unwrap();
    assert!(!db.commit_rebuild(rebuilt).unwrap());
    assert!(!db.needs_rebuild("c").unwrap());
    assert_eq!(top(&db, &w), "p1");
}

#[test]
fn commit_after_drop_is_a_noop() {
    let (_tmp, mut db) = seed(IndexKind::Hnsw);
    defer_move_p0_to_z(&mut db);

    let inputs = db.snapshot_rebuild_inputs("c").unwrap().expect("stale");
    let rebuilt = inputs.build().unwrap();
    db.drop_collection("c").unwrap();
    // The collection vanished while the build ran: the commit is discarded cleanly.
    assert!(!db.commit_rebuild(rebuilt).unwrap());
}

#[test]
fn fresh_index_has_nothing_to_rebuild() {
    let (_tmp, db) = seed(IndexKind::Hnsw);
    assert!(!db.needs_rebuild("c").unwrap());
    assert!(db.snapshot_rebuild_inputs("c").unwrap().is_none());
}

#[test]
fn disk_index_rebuilds_off_lock() {
    // Exercises the `RebuiltKind::Disk` commit arm: the graph + PQ are built off-lock
    // and the encrypted artifact is sealed (`write_disk_index`) under the write lock.
    let (_tmp, mut db) = seed(IndexKind::DiskVamana);
    assert_eq!(top(&db, &X), "p0");
    defer_move_p0_to_z(&mut db);

    let inputs = db.snapshot_rebuild_inputs("c").unwrap().expect("stale");
    let rebuilt = inputs.build().unwrap();
    assert!(!db.commit_rebuild(rebuilt).unwrap());
    assert!(!db.needs_rebuild("c").unwrap());
    assert_eq!(top(&db, &Z), "p0");
}

#[test]
fn ivf_index_rebuilds_off_lock() {
    // A non-HNSW in-memory kind through the `RebuiltKind::Ready` commit arm.
    let (_tmp, mut db) = seed(IndexKind::Ivf);
    defer_move_p0_to_z(&mut db);
    let inputs = db.snapshot_rebuild_inputs("c").unwrap().expect("stale");
    let rebuilt = inputs.build().unwrap();
    assert!(!db.commit_rebuild(rebuilt).unwrap());
    assert_eq!(top(&db, &Z), "p0");
}
