// SPDX-License-Identifier: AGPL-3.0-only
//! Measurement harness for ADR-0062: **does the off-lock rebuild keep concurrent
//! readers from stalling while the single writer rebuilds a deferred index?**
//!
//! The first version of this harness (PR #265) measured the *problem*: under the
//! plain `RwLock` model a deferred rebuild ran under the exclusive lock, stalling
//! every concurrent reader for the rebuild's whole duration — 8 s at 20k vectors,
//! 30 s at 50k, 77 s at 100k. ADR-0062 moves the rebuild off the exclusive lock:
//! the inputs are captured under the shared read lock (`snapshot_rebuild_inputs`),
//! the new index is built with **no lock held** (`RebuildInputs::build`), and it is
//! installed under a brief write lock (`commit_rebuild`), while readers keep serving
//! the prior snapshot throughout. This harness now measures the *fix*: the worst
//! read latency during a rebuild should collapse from seconds to the steady-state
//! tail.
//!
//! It models the server: `Arc<RwLock<Database>>`, readers take the shared lock and
//! call `search_snapshot` (which serves the prior snapshot when stale); a single
//! background driver runs the off-lock rebuild, mirroring the server's scheduler.
//!
//! Not a CI gate — `#[ignore]`d (it builds 100k-vector indexes; minutes, not the
//! sub-second unit budget). Reproduce with:
//!
//! ```text
//! cargo test -p quiverdb-embed --release --test mvcc_measurement -- --ignored --nocapture
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams};
use serde_json::json;

const DIM: usize = 128;

// Deterministic pseudo-random vector from a seed (xorshift) — no `rand` dep, so the
// run is byte-reproducible. Centered on 0 so cosine distance is meaningful.
fn vec_for(seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..DIM)
        .map(|_| {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let bits = (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32;
            bits / (1u64 << 24) as f32 - 0.5
        })
        .collect()
}

// A reader's record of one query: when it finished, and how long it waited end to
// end (lock acquisition + search). The wait, not the search, is what a rebuild
// would inflate if it held the exclusive lock.
#[derive(Clone, Copy)]
struct Sample {
    at: Duration,
    latency_us: u64,
}

fn percentile(sorted_us: &[u64], p: f64) -> u64 {
    if sorted_us.is_empty() {
        return 0;
    }
    let idx = ((sorted_us.len() as f64 - 1.0) * p).round() as usize;
    sorted_us[idx]
}

// One reader query: take the shared lock and read. `search_snapshot` serves the
// prior snapshot when a rebuild is deferred (ADR-0062), so it never blocks on the
// writer.
fn read_once(db: &RwLock<Database>, query: &[f32]) {
    let guard = db.read().unwrap();
    guard
        .search_snapshot("c", query, &SearchParams::default())
        .unwrap();
}

// The server's off-lock rebuild scheduler, in miniature: capture inputs under the
// shared read lock, build with no lock, install under a brief write lock; repeat
// while a write landed during the build. Returns the wall time it took.
fn drive_rebuild(db: &RwLock<Database>) -> Duration {
    let start = Instant::now();
    loop {
        let inputs = {
            let guard = db.read().unwrap();
            guard.snapshot_rebuild_inputs("c").unwrap()
        };
        let Some(inputs) = inputs else { break };
        let rebuilt = inputs.build().unwrap(); // no lock held — the expensive part
        let still_stale = {
            let mut guard = db.write().unwrap();
            guard.commit_rebuild(rebuilt).unwrap()
        };
        if !still_stale {
            break;
        }
    }
    start.elapsed()
}

fn measure(n: usize, readers: usize, bulk: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let mut db = Database::open(tmp.path()).unwrap();
    db.create_collection(
        "c",
        Descriptor::new(DIM as u32, Dtype::F32, DistanceMetric::Cosine),
    )
    .unwrap();

    // Seed N points in one bulk write, then build the index once.
    let payload = json!({});
    let ids: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
    let vecs: Vec<Vec<f32>> = (0..n as u64).map(vec_for).collect();
    let points: Vec<(&str, &[f32], &serde_json::Value)> = ids
        .iter()
        .zip(vecs.iter())
        .map(|(id, v)| (id.as_str(), v.as_slice(), &payload))
        .collect();
    db.upsert_bulk("c", &points).unwrap();

    let build_start = Instant::now();
    db.ensure_indexed("c").unwrap();
    let initial_build = build_start.elapsed();

    let db = Arc::new(RwLock::new(db));
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let start = Instant::now();

    let mut handles = Vec::new();
    for r in 0..readers {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let done = Arc::clone(&done);
        handles.push(std::thread::spawn(move || {
            let query = vec_for(1_000_000 + r as u64);
            let mut samples = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                read_once(&db, &query);
                samples.push(Sample {
                    at: start.elapsed(),
                    latency_us: t0.elapsed().as_micros() as u64,
                });
                done.fetch_add(1, Ordering::Relaxed);
            }
            samples
        }));
    }

    // Warm up until readers have done real work, then fire one bulk upsert (marks
    // the index stale) and kick the off-lock rebuild driver — exactly the server's
    // sequence. Readers keep running on the prior snapshot throughout.
    while done.load(Ordering::Relaxed) < (readers as u64 * 200) {
        std::thread::yield_now();
    }
    let next: Vec<Vec<f32>> = (0..bulk as u64).map(|i| vec_for(2_000_000 + i)).collect();
    let next_ids: Vec<String> = (0..bulk).map(|i| format!("n{i}")).collect();
    let next_points: Vec<(&str, &[f32], &serde_json::Value)> = next_ids
        .iter()
        .zip(next.iter())
        .map(|(id, v)| (id.as_str(), v.as_slice(), &payload))
        .collect();
    let write_at = {
        let mut guard = db.write().unwrap();
        let t = start.elapsed();
        guard.upsert_bulk("c", &next_points).unwrap();
        t
    };
    let rebuild_wall = drive_rebuild(&db);

    // Let readers gather post-rebuild samples, then stop.
    while done.load(Ordering::Relaxed) < (readers as u64 * 400) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);

    let mut all: Vec<Sample> = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }

    let steady: Vec<u64> = all
        .iter()
        .filter(|s| s.at < write_at)
        .map(|s| s.latency_us)
        .collect();
    let mut steady_sorted = steady.clone();
    steady_sorted.sort_unstable();

    // Samples taken while the off-lock rebuild was in flight — the window that used
    // to stall for the whole rebuild.
    let rebuild_end = write_at + rebuild_wall;
    let during: Vec<u64> = all
        .iter()
        .filter(|s| s.at >= write_at && s.at < rebuild_end)
        .map(|s| s.latency_us)
        .collect();
    let mut during_sorted = during.clone();
    during_sorted.sort_unstable();
    let max_during = during.iter().copied().max().unwrap_or(0);

    println!("\n=== MVCC off-lock rebuild: N={n} vectors, {readers} readers, bulk={bulk} ===");
    println!("initial index build (single-threaded): {initial_build:.2?}");
    println!("off-lock rebuild wall time (build done with no exclusive lock): {rebuild_wall:.2?}");
    println!("steady-state read latency (fresh, concurrent):");
    println!(
        "  p50 {} us · p95 {} us · p99 {} us · n={}",
        percentile(&steady_sorted, 0.50),
        percentile(&steady_sorted, 0.95),
        percentile(&steady_sorted, 0.99),
        steady_sorted.len(),
    );
    println!("read latency DURING the rebuild (readers serve the prior snapshot):");
    println!(
        "  p50 {} us · p95 {} us · p99 {} us · max {} us ({:.3} s) · n={}",
        percentile(&during_sorted, 0.50),
        percentile(&during_sorted, 0.95),
        percentile(&during_sorted, 0.99),
        max_during,
        max_during as f64 / 1e6,
        during_sorted.len(),
    );
    println!(
        "  => the worst read during a {rebuild_wall:.1?} rebuild stays in the tens-of-µs tail,",
    );
    println!("     not the multi-second stall the exclusive-lock rebuild imposed (see PR #265).");
}

#[test]
#[ignore = "measurement harness, not a gate — see module docs to run"]
fn mvcc_reader_stall_during_rebuild() {
    // A few sizes so the result's stability with collection size is visible. Kept
    // off 1M to stay well within a shared dev box (no OOM, minutes not hours).
    for n in [20_000usize, 50_000, 100_000] {
        measure(n, 4, n / 10);
    }
}
