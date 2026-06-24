// SPDX-License-Identifier: AGPL-3.0-only
//! Lock-free MVCC reads (ADR-0064, increment 1): the single writer publishes an
//! immutable [`CollectionSnapshot`] (base index + overlay of writes since) into an
//! `ArcSwap`; readers `load()` it without a lock and merge base ⊕ overlay. These
//! tests pin (1) the merge is exact for pure-vector reads — base hits, overlay
//! upserts, updates (supersede), and deletes (tombstone) — and (2) a reader holding
//! the snapshot cell sees consistent, never-torn results while the writer publishes
//! concurrently. The flag is default-off; these enable it via `set_mvcc_reads`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams};
use serde_json::json;

const DIM: usize = 8;

// A deterministic vector for point `i` (spread out so neighbors are well separated).
fn vec_for(i: u32) -> Vec<f32> {
    (0..DIM)
        .map(|j| (((i * 7 + j as u32 * 13) % 97) as f32) / 7.0)
        .collect()
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

// Exact top-k `(ext_id, l2_distance)` over a live set, closest first; ties broken
// by id so the result is deterministic (the index breaks equal-distance ties
// arbitrarily, so callers compare the score *sequence*, not the tied id order).
fn brute_force_topk(live: &[(String, Vec<f32>)], query: &[f32], k: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = live
        .iter()
        .map(|(id, v)| (id.clone(), l2(v, query)))
        .collect();
    scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    scored.truncate(k);
    scored
}

fn open_mvcc(dir: &std::path::Path) -> Database {
    let mut db = Database::open(dir).unwrap();
    db.set_mvcc_reads(true);
    assert!(db.mvcc_reads());
    db.create_collection(
        "c",
        Descriptor::new(DIM as u32, Dtype::F32, DistanceMetric::L2),
    )
    .unwrap();
    db
}

#[test]
fn snapshot_merges_base_overlay_updates_and_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = open_mvcc(tmp.path());

    // Base: bulk-load 64 points, then build & publish the snapshot base.
    let ids: Vec<String> = (0..64u32).map(|i| format!("p{i}")).collect();
    let vecs: Vec<Vec<f32>> = (0..64u32).map(vec_for).collect();
    let payloads: Vec<serde_json::Value> = (0..64u32).map(|i| json!({ "i": i })).collect();
    let refs: Vec<(&str, &[f32], &serde_json::Value)> = (0..64)
        .map(|i| (ids[i].as_str(), vecs[i].as_slice(), &payloads[i]))
        .collect();
    db.upsert_bulk("c", &refs).unwrap();
    db.ensure_indexed("c").unwrap(); // publishes the base snapshot, empty overlay

    // Overlay: 8 new points, one update (supersede), one delete (tombstone).
    for i in 100..108u32 {
        db.upsert("c", &format!("n{i}"), &vec_for(i), &json!({}))
            .unwrap();
    }
    db.upsert("c", "p5", &vec_for(900), &json!({ "updated": true }))
        .unwrap(); // supersede base p5
    db.delete("c", "p7").unwrap(); // tombstone base p7
    assert!(!db.delete("c", "ghost").unwrap()); // deleting an absent id is a no-op

    // Ground-truth live set: base minus p7, with p5 superseded, plus the 8 new.
    let mut live: Vec<(String, Vec<f32>)> = (0..64u32)
        .filter(|i| *i != 7)
        .map(|i| {
            (
                format!("p{i}"),
                if i == 5 { vec_for(900) } else { vec_for(i) },
            )
        })
        .collect();
    live.extend((100..108u32).map(|i| (format!("n{i}"), vec_for(i))));

    let cell = db.collection_snapshot("c").unwrap();
    for &qi in &[0u32, 5, 33, 900, 103] {
        let query = vec_for(qi);
        let want = brute_force_topk(&live, &query, 10);
        let got = cell.load().search(&query, 10, 64).unwrap();

        // The k closest *distances* must match the exact ground truth (HNSW is
        // recall-1.0 at this size; the overlay scan is exact). Tie order is arbitrary.
        let got_scores: Vec<f32> = got.iter().map(|m| m.score).collect();
        let want_scores: Vec<f32> = want.iter().map(|(_, s)| *s).collect();
        for (g, w) in got_scores.iter().zip(&want_scores) {
            assert!((g - w).abs() < 1e-4, "query {qi}: distance {g} != {w}");
        }
        // The returned ids are exactly the ground-truth set (tie order aside).
        let got_ids: std::collections::BTreeSet<&str> = got.iter().map(|m| m.id.as_str()).collect();
        let want_ids: std::collections::BTreeSet<&str> =
            want.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(got_ids, want_ids, "query {qi}: id set mismatch");
        // The deleted point and the stale copy of the updated point never resurface.
        assert!(!got_ids.contains("p7"));
    }

    // with_vector enrichment (via the snapshot's result ids) and the k==0 edge.
    let with_vec = db
        .search(
            "c",
            &vec_for(0),
            &SearchParams {
                with_vector: true,
                with_payload: false,
                ..SearchParams::default()
            },
        )
        .unwrap();
    assert!(with_vec[0].vector.is_some());
    assert!(cell.load().search(&vec_for(0), 0, 64).unwrap().is_empty());
}

#[test]
fn locked_search_routes_through_snapshot_with_payload() {
    // The locked `search` path, in MVCC mode, serves from the snapshot and enriches
    // payloads by a store fetch — so an ordinary (payload-bearing) read still works.
    let tmp = tempfile::tempdir().unwrap();
    let mut db = open_mvcc(tmp.path());
    for i in 0..32u32 {
        db.upsert("c", &format!("p{i}"), &vec_for(i), &json!({ "i": i }))
            .unwrap();
    }
    let hits = db
        .search("c", &vec_for(0), &SearchParams::default())
        .unwrap();
    assert_eq!(hits[0].id, "p0");
    assert_eq!(hits[0].payload, Some(json!({ "i": 0 }))); // payload enriched
    // A filter is the explicit increment-2 limitation: rejected, not silently wrong.
    let filtered = SearchParams {
        filter: Some(quiver_embed::Filter::Eq {
            field: "i".into(),
            value: json!(0),
        }),
        ..SearchParams::default()
    };
    assert!(db.search("c", &vec_for(0), &filtered).is_err());
}

#[test]
fn reader_during_write_sees_consistent_snapshots() {
    // The headline: readers holding the snapshot cell `load()` it lock-free while the
    // writer republishes on every upsert. A sentinel at the query (distance 0) must
    // be the top hit on *every* read — a torn/empty publish would drop it — and
    // results stay sorted and bounded throughout.
    let tmp = tempfile::tempdir().unwrap();
    let mut db = open_mvcc(tmp.path());

    let origin = vec![0.0f32; DIM]; // query and sentinel location
    // Base: the sentinel plus 50 far points (all distance >= ~14 from the origin).
    db.upsert("c", "S", &origin, &json!({})).unwrap();
    for i in 0..50u32 {
        db.upsert("c", &format!("b{i}"), &vec_for(i + 1), &json!({}))
            .unwrap();
    }
    db.ensure_indexed("c").unwrap();

    let cell = db.collection_snapshot("c").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..6)
        .map(|_| {
            let cell = cell.clone();
            let stop = stop.clone();
            let q = origin.clone();
            thread::spawn(move || {
                let mut iters = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let hits = cell.load().search(&q, 10, 64).unwrap();
                    assert!(!hits.is_empty(), "snapshot read returned no hits");
                    assert_eq!(hits[0].id, "S", "sentinel lost from a published snapshot");
                    assert!(hits.len() <= 10);
                    for w in hits.windows(2) {
                        assert!(
                            w[0].score <= w[1].score + 1e-3,
                            "results not sorted by distance"
                        );
                    }
                    iters += 1;
                }
                iters
            })
        })
        .collect();

    // Writer: 500 far-away upserts, each republishing the snapshot.
    for i in 0..500u32 {
        db.upsert("c", &format!("w{i}"), &vec_for(i + 1000), &json!({}))
            .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        assert!(r.join().unwrap() > 0, "a reader ran zero iterations");
    }

    // After the run the sentinel is still top-1 and the writes are visible.
    let hits = cell.load().search(&origin, 10, 64).unwrap();
    assert_eq!(hits[0].id, "S");
    assert_eq!(hits.len(), 10);
}

#[test]
fn off_lock_rebuild_publishes_the_snapshot() {
    // The server's off-lock rebuild path (ADR-0062) — capture → build (no lock) →
    // commit — publishes the base snapshot in MVCC mode.
    let tmp = tempfile::tempdir().unwrap();
    let mut db = open_mvcc(tmp.path());
    for i in 0..40u32 {
        db.upsert("c", &format!("p{i}"), &vec_for(i), &json!({}))
            .unwrap();
    }
    let inputs = db.snapshot_rebuild_inputs("c").unwrap();
    if let Some(inputs) = inputs {
        let built = inputs.build().unwrap();
        db.commit_rebuild(built).unwrap();
    }
    let hits = db
        .collection_snapshot("c")
        .unwrap()
        .load()
        .search(&vec_for(0), 5, 64)
        .unwrap();
    assert_eq!(hits[0].id, "p0");
    assert_eq!(hits.len(), 5);
}

#[test]
fn crowded_overlay_defers_a_consolidating_rebuild() {
    // Once the overlay passes the churn threshold a write marks the collection stale
    // so the next read rebuilds and folds the overlay into a fresh base.
    let tmp = tempfile::tempdir().unwrap();
    let mut db = open_mvcc(tmp.path());
    // Threshold floor is 1024 over an empty base; cross it.
    for i in 0..1100u32 {
        db.upsert("c", &format!("p{i}"), &vec_for(i % 97), &json!({}))
            .unwrap();
    }
    assert!(
        db.needs_rebuild("c").unwrap(),
        "overlay should defer a rebuild once crowded"
    );
    db.ensure_indexed("c").unwrap();
    assert!(!db.needs_rebuild("c").unwrap());
    // After consolidation the base serves and reads still work.
    let hits = db
        .collection_snapshot("c")
        .unwrap()
        .load()
        .search(&vec_for(0), 10, 64)
        .unwrap();
    assert_eq!(hits.len(), 10);
}

#[test]
fn reopen_with_mvcc_rebuilds_and_serves() {
    // Enabling MVCC after open (the server's `QUIVER_MVCC_READS` flow) forces a
    // rebuild on the next read, so a reopened collection serves from a fresh base.
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut db = open_mvcc(tmp.path());
        for i in 0..32u32 {
            db.upsert("c", &format!("p{i}"), &vec_for(i), &json!({ "i": i }))
                .unwrap();
        }
        db.ensure_indexed("c").unwrap();
    }
    // Reopen (default off), then enable — must rebuild + publish, not serve empty.
    let mut db = Database::open(tmp.path()).unwrap();
    db.set_mvcc_reads(true);
    let hits = db
        .search("c", &vec_for(0), &SearchParams::default())
        .unwrap();
    assert_eq!(hits[0].id, "p0");
    assert_eq!(hits[0].payload, Some(json!({ "i": 0 })));
    // Toggling MVCC back off restores the locked path.
    db.set_mvcc_reads(false);
    assert!(!db.mvcc_reads());
    assert_eq!(
        db.search("c", &vec_for(0), &SearchParams::default())
            .unwrap()[0]
            .id,
        "p0"
    );
}

#[test]
fn flag_off_is_the_unchanged_locked_path() {
    // Default (MVCC off): the snapshot cell is never published (stays empty) and the
    // ordinary locked search serves results — zero behavior change.
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(tmp.path()).unwrap();
    assert!(!db.mvcc_reads());
    db.create_collection(
        "c",
        Descriptor::new(DIM as u32, Dtype::F32, DistanceMetric::L2),
    )
    .unwrap();
    for i in 0..16u32 {
        db.upsert("c", &format!("p{i}"), &vec_for(i), &json!({ "i": i }))
            .unwrap();
    }
    db.ensure_indexed("c").unwrap();
    // Locked path works as before.
    let hits = db
        .search("c", &vec_for(0), &SearchParams::default())
        .unwrap();
    assert_eq!(hits[0].id, "p0");
    // The snapshot cell was never published: empty (the in-place index holds the data).
    let snap_hits = db
        .collection_snapshot("c")
        .unwrap()
        .load()
        .search(&vec_for(0), 10, 64)
        .unwrap();
    assert!(
        snap_hits.is_empty(),
        "snapshot should be empty when MVCC is off"
    );
}
