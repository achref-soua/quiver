// SPDX-License-Identifier: AGPL-3.0-only
//! Replication engine seam (ADR-0030): a follower bootstraps from a leader's
//! logical snapshot and follows its committed-op tail (captured through the
//! commit observer), then serves the same reads. Hermetic — two local stores, no
//! network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams, WalOp};
use serde_json::json;

// Install a commit observer that records each committed op into `tail`.
fn capture(db: &mut Database) -> Arc<Mutex<Vec<WalOp>>> {
    let tail: Arc<Mutex<Vec<WalOp>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&tail);
    db.set_commit_observer(Arc::new(move |entry| {
        sink.lock().unwrap().push(entry.op.clone());
    }));
    tail
}

#[test]
fn follower_bootstraps_from_snapshot_then_follows_the_tail() {
    let leader_dir = tempfile::tempdir().unwrap();
    let mut leader = Database::open(leader_dir.path()).unwrap();
    let tail = capture(&mut leader);

    // State the follower will bootstrap from a snapshot.
    leader
        .create_collection(
            "places",
            Descriptor::new(3, Dtype::F32, DistanceMetric::Cosine),
        )
        .unwrap();
    leader
        .upsert("places", "a", &[1.0, 0.0, 0.0], &json!({"city": "paris"}))
        .unwrap();
    leader
        .upsert("places", "b", &[0.0, 1.0, 0.0], &json!({"city": "rome"}))
        .unwrap();

    // Bootstrap a fresh follower from the leader's logical snapshot.
    let follower_dir = tempfile::tempdir().unwrap();
    let mut follower = Database::open(follower_dir.path()).unwrap();
    let bootstrapped = tail.lock().unwrap().len();
    for op in leader.replication_snapshot().unwrap() {
        follower.apply_replicated(op).unwrap();
    }
    assert_eq!(follower.len("places").unwrap(), 2);

    // The follower serves the same nearest neighbour as the leader.
    let q = [1.0, 0.0, 0.0];
    let lead = leader
        .search("places", &q, &SearchParams::default())
        .unwrap();
    let foll = follower
        .search("places", &q, &SearchParams::default())
        .unwrap();
    assert_eq!(foll[0].id, lead[0].id);
    assert_eq!(foll[0].id, "a");

    // New leader writes flow to the follower through the captured tail (only ops
    // committed after the snapshot — the real stream never re-creates a snapshot
    // collection).
    leader
        .upsert("places", "c", &[0.0, 0.0, 1.0], &json!({"city": "oslo"}))
        .unwrap();
    leader.delete("places", "b").unwrap();
    let new_ops: Vec<WalOp> = tail.lock().unwrap().split_off(bootstrapped);
    for op in new_ops {
        follower.apply_replicated(op).unwrap();
    }

    assert_eq!(follower.len("places").unwrap(), 2); // a, c — b was deleted
    let oslo = follower
        .search("places", &[0.0, 0.0, 1.0], &SearchParams::default())
        .unwrap();
    assert_eq!(oslo[0].id, "c");
    assert!(follower.get("places", "b").unwrap().is_none());
}

#[test]
fn follower_survives_its_own_restart() {
    let leader_dir = tempfile::tempdir().unwrap();
    let mut leader = Database::open(leader_dir.path()).unwrap();
    leader
        .create_collection("c", Descriptor::new(2, Dtype::F32, DistanceMetric::L2))
        .unwrap();
    leader.upsert("c", "x", &[1.0, 2.0], &json!({})).unwrap();

    // Bootstrap a follower, then drop it and reopen the same directory: the
    // replicated ops were persisted to the follower's own WAL, so its state
    // recovers.
    let follower_dir = tempfile::tempdir().unwrap();
    {
        let mut follower = Database::open(follower_dir.path()).unwrap();
        for op in leader.replication_snapshot().unwrap() {
            follower.apply_replicated(op).unwrap();
        }
        assert_eq!(follower.len("c").unwrap(), 1);
    }
    let reopened = Database::open(follower_dir.path()).unwrap();
    assert_eq!(reopened.len("c").unwrap(), 1);
    assert!(reopened.get("c", "x").unwrap().is_some());
}
