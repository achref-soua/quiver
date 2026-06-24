// SPDX-License-Identifier: AGPL-3.0-only
//! Measurement harness for ADR-0062: **does the shipped `RwLock` model (ADR-0057)
//! stall concurrent readers while the single writer rebuilds a deferred index?**
//!
//! Slice 5 already proved RwLock reads scale with cores when the index is *fresh*
//! (1.76× at ef=256). The only marginal win a lock-free arc-swap MVCC path
//! (ADR-0057 phase 2) would add is: readers keep serving the *previous* snapshot
//! during a rebuild instead of blocking on the writer's exclusive lock. This
//! harness isolates exactly that window and measures the stall, so the decision to
//! ship `unsafe` lock-free code (or not) rests on a real number, not faith.
//!
//! It faithfully models the server: `Arc<RwLock<Database>>`, a reader takes the
//! shared lock and calls `search_snapshot`; on `IndexStale` it takes the exclusive
//! lock once, `ensure_indexed`, and searches under it (the `search_blocking` cold
//! path, `crates/quiver-server/src/lib.rs`). A bulk upsert marks the index stale
//! (fast), so the *next read* pays the rebuild under the exclusive lock — stalling
//! every other reader for the rebuild's duration.
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

use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, Error, SearchParams};
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
// inflates.
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

// One reader loop: snapshot read under the shared lock; on `IndexStale`, the cold
// path takes the exclusive lock once to rebuild and search (mirrors the server).
fn read_once(db: &RwLock<Database>, query: &[f32]) {
    let params = SearchParams::default();
    {
        let guard = db.read().unwrap();
        match guard.search_snapshot("c", query, &params) {
            Err(Error::IndexStale) => {}
            other => {
                other.unwrap();
                return;
            }
        }
    }
    // Cold path: rebuild under the exclusive lock, then search while holding it.
    let mut guard = db.write().unwrap();
    guard.ensure_indexed("c").unwrap();
    guard.search_snapshot("c", query, &params).unwrap();
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
    // Counts reads completed, so the writer fires only once readers are warm.
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
                let latency_us = t0.elapsed().as_micros() as u64;
                samples.push(Sample {
                    at: start.elapsed(),
                    latency_us,
                });
                done.fetch_add(1, Ordering::Relaxed);
            }
            samples
        }));
    }

    // Warm up until readers have done real work, then fire one bulk upsert. It
    // marks the index stale and returns fast; the *next* reader pays the rebuild
    // under the exclusive lock — the stall we are here to measure.
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

    // Let the rebuild happen and readers recover, then stop.
    while done.load(Ordering::Relaxed) < (readers as u64 * 200) + (readers as u64 * 200) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);

    let mut all: Vec<Sample> = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }

    // Steady state = samples that finished before the write. Stall window = samples
    // that finished after it but within the rebuild-and-recover span.
    let steady: Vec<u64> = all
        .iter()
        .filter(|s| s.at < write_at)
        .map(|s| s.latency_us)
        .collect();
    let mut steady_sorted = steady.clone();
    steady_sorted.sort_unstable();

    let after: Vec<&Sample> = all.iter().filter(|s| s.at >= write_at).collect();
    let max_after = after.iter().map(|s| s.latency_us).max().unwrap_or(0);
    // The single worst post-write read ≈ the time one reader held the exclusive lock
    // rebuilding; concurrent readers that arrived during it are blocked for ~the same
    // span. That whole span is what a lock-free snapshot read would eliminate.
    let stalled = after
        .iter()
        .filter(|s| s.latency_us > percentile(&steady_sorted, 0.99).max(1) * 5)
        .count();

    println!("\n=== MVCC stall measurement: N={n} vectors, {readers} readers, bulk={bulk} ===");
    println!("initial index build (single-threaded): {initial_build:.2?}");
    println!("steady-state read latency (fresh, concurrent):");
    println!(
        "  p50 {} us · p95 {} us · p99 {} us · n={}",
        percentile(&steady_sorted, 0.50),
        percentile(&steady_sorted, 0.95),
        percentile(&steady_sorted, 0.99),
        steady_sorted.len(),
    );
    println!("rebuild window (RwLock: every reader blocks on the writer):");
    println!(
        "  worst read latency after write: {max_after} us ({:.3} s)",
        max_after as f64 / 1e6
    );
    println!("  reads stalled >5×p99: {stalled}");
    println!(
        "  => with RwLock, that worst-case stall is borne by EVERY read arriving during the rebuild;",
    );
    println!(
        "     a lock-free arc-swap snapshot read would serve the prior snapshot and not block (~p99 latency)."
    );
}

#[test]
#[ignore = "measurement harness, not a gate — see module docs to run"]
fn mvcc_reader_stall_during_rebuild() {
    // A few sizes so the stall's growth with collection size is visible. Kept off
    // 1M to stay well within a shared dev box (no OOM, minutes not hours).
    for n in [20_000usize, 50_000, 100_000] {
        measure(n, 4, n / 10);
    }
}
