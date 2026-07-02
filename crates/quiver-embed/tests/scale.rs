// SPDX-License-Identifier: AGPL-3.0-only
//! Memory-frugal scale harness (ignored by default). Ingests N synthetic vectors
//! through the bulk path into an IVF+PQ collection — the frugal config that keeps
//! only centroids + PQ codes resident — and reports the REAL measured ingest
//! rate, peak RSS, on-disk size, query latency, and (at feasible tiers) recall.
//!
//! It never fabricates a number: every figure printed is measured on the box it
//! ran on. Recall is only measured when the full set is cheap to brute-force
//! (`N <= QUIVER_SCALE_RECALL_CAP`, regenerating vectors deterministically);
//! above that tier recall is reported as "skipped (measure at a smaller tier)".
//!
//! Run (release is essential for a realistic rate):
//! ```text
//! QUIVER_SCALE_N=1000000  cargo test -p quiverdb-embed --release --test scale -- --ignored --nocapture
//! QUIVER_SCALE_N=100000000 QUIVER_SCALE_DIR=/data/scale \
//!     cargo test -p quiverdb-embed --release --test scale -- --ignored --nocapture
//! ```
//! Env: QUIVER_SCALE_N (count, default 1e6), QUIVER_SCALE_DIM (default 128),
//! QUIVER_SCALE_BATCH (bulk batch, default 20_000), QUIVER_SCALE_QUERIES
//! (default 200), QUIVER_SCALE_RECALL_CAP (default 2_000_000),
//! QUIVER_SCALE_DIR (data dir; default a tempdir on the system disk).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use quiver_embed::{
    Database, Descriptor, DistanceMetric, Dtype, IndexKind, IndexSpec, SearchParams,
};

// A SplitMix64 stream from a seed, mapping each draw to [-1, 1). Deterministic.
fn stream(seed: u64, dim: usize, scale: f32, out: &mut Vec<f32>, add: bool) {
    let mut z = seed;
    for d in 0..dim {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = z;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let v = ((x >> 40) as f32 / f32::from(1u16 << 11) - 1.0) * scale;
        if add {
            out[d] += v;
        } else {
            out.push(v);
        }
    }
}

// Number of latent clusters — gives the corpus realistic ANN structure (uniform
// random is the pathological near-equidistant case where recall is meaningless).
const CLUSTERS: u64 = 4096;

// Deterministic clustered synthetic vector: a per-cluster centre plus small
// per-point noise, so nearest neighbours are well-defined (same-cluster points)
// and recall is a meaningful measurement. Regenerable from `i` alone (no corpus
// held in RAM for the brute-force ground truth).
fn synth(i: u64, dim: usize, out: &mut Vec<f32>) {
    out.clear();
    // Deterministic cluster assignment for point i.
    let mut h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 29;
    let cluster = h % CLUSTERS;
    stream(
        cluster.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1,
        dim,
        1.0,
        out,
        false,
    );
    stream(i.wrapping_add(0xA0761D65) | 1, dim, 0.10, out, true);
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn peak_rss_kib() -> u64 {
    // VmHWM = peak resident set size (Linux).
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

fn dir_bytes(p: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let meta = e.metadata();
            if let Ok(m) = meta {
                total += if m.is_dir() {
                    dir_bytes(&e.path())
                } else {
                    m.len()
                };
            }
        }
    }
    total
}

#[test]
#[ignore = "scale/soak test — run explicitly with --ignored --release"]
fn scale_ingest_and_query() {
    let n = env_usize("QUIVER_SCALE_N", 1_000_000);
    let dim = env_usize("QUIVER_SCALE_DIM", 128);
    let batch = env_usize("QUIVER_SCALE_BATCH", 20_000);
    let queries = env_usize("QUIVER_SCALE_QUERIES", 200);
    let recall_cap = env_usize("QUIVER_SCALE_RECALL_CAP", 2_000_000);
    // Seal to disk this often so the active buffer (and RAM) stays bounded during
    // a large ingest; rounded to a whole number of batches.
    let checkpoint_every = (env_usize("QUIVER_SCALE_CHECKPOINT", 1_000_000) / batch).max(1) * batch;

    // Keep data on the real disk (a tempdir under a caller-chosen root; NOT /tmp,
    // which may be RAM-backed and would OOM at scale).
    let root = std::env::var("QUIVER_SCALE_DIR").unwrap_or_else(|_| ".scratch/scale-data".into());
    std::fs::create_dir_all(&root).unwrap();
    let data_dir = tempfile::Builder::new()
        .prefix("scale-")
        .tempdir_in(&root)
        .unwrap();

    eprintln!(
        "scale: N={n} dim={dim} batch={batch} queries={queries} dir={}",
        data_dir.path().display()
    );

    let mut db = Database::open(data_dir.path()).unwrap();
    // Frugal config: IVF + product quantization. PQ subspaces default to a
    // standard m=16 (each subspace 8-dim at dim=128); dim/2 trains too many
    // codebooks. QUIVER_SCALE_PQ=0 uses IVF-Flat (exact vectors, no PQ) — the
    // recall oracle that isolates IVF coverage from PQ compression loss.
    let pq_env = env_usize("QUIVER_SCALE_PQ", 16);
    let pq = if pq_env == 0 {
        0
    } else {
        pq_env.clamp(1, dim / 2) as u32
    };
    let quant = if pq == 0 { None } else { Some(pq) };
    db.create_collection(
        "scale",
        Descriptor::new(dim as u32, Dtype::F32, DistanceMetric::L2).with_index(IndexSpec {
            kind: IndexKind::Ivf,
            pq_subspaces: quant,
        }),
    )
    .unwrap();

    // --- Ingest (bulk path, ADR-0045) ---
    let t0 = Instant::now();
    let mut vecbuf = Vec::with_capacity(dim);
    let empty = serde_json::json!({});
    let mut i = 0u64;
    while (i as usize) < n {
        let this = batch.min(n - i as usize);
        let mut ids: Vec<String> = Vec::with_capacity(this);
        let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(this);
        for j in 0..this {
            let idx = i + j as u64;
            ids.push(format!("p{idx}"));
            synth(idx, dim, &mut vecbuf);
            vecs.push(vecbuf.clone());
        }
        let points: Vec<(&str, &[f32], &serde_json::Value)> = ids
            .iter()
            .zip(&vecs)
            .map(|(id, v)| (id.as_str(), v.as_slice(), &empty))
            .collect();
        db.upsert_bulk("scale", &points).unwrap();
        i += this as u64;
        // Seal the active buffer to disk periodically so ingest stays memory-frugal
        // — without this the whole active segment (vectors + primary index)
        // accumulates in RAM until the first checkpoint.
        if (i as usize).is_multiple_of(checkpoint_every) {
            db.checkpoint().unwrap();
        }
        if (i as usize).is_multiple_of(batch * 20) || i as usize == n {
            eprintln!(
                "  ingested {i}/{n}  ({:.0} vec/s, RSS {} MiB)",
                i as f64 / t0.elapsed().as_secs_f64(),
                peak_rss_kib() / 1024
            );
        }
    }
    let ingest_s = t0.elapsed().as_secs_f64();
    let rate = n as f64 / ingest_s;

    // Force the index build + flush so RSS/disk reflect a queryable state.
    let tb = Instant::now();
    let warm = {
        let mut v = Vec::new();
        synth(0, dim, &mut v);
        v
    };
    let params = SearchParams {
        k: 10,
        ef_search: 64,
        with_payload: false,
        with_vector: false,
        filter: None,
    };
    let _ = db.search("scale", &warm, &params).unwrap();
    let build_s = tb.elapsed().as_secs_f64();

    // --- Query latency ---
    let mut lat: Vec<f64> = Vec::with_capacity(queries);
    let mut qv = Vec::new();
    for q in 0..queries {
        synth((n as u64).wrapping_add(q as u64 * 7 + 1), dim, &mut qv);
        let t = Instant::now();
        let _ = db.search("scale", &qv, &params).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)];

    // --- Recall@10 (only when full-set brute force is cheap) ---
    let recall = if n <= recall_cap {
        let rq = queries.min(50);
        let mut hits = 0usize;
        let mut total = 0usize;
        let mut cand = Vec::new();
        for q in 0..rq {
            synth((n as u64).wrapping_add(q as u64 * 7 + 1), dim, &mut qv);
            // Brute-force ground truth by regenerating every vector.
            let mut best: Vec<(f32, u64)> = Vec::with_capacity(n);
            for idx in 0..n as u64 {
                synth(idx, dim, &mut cand);
                let d: f32 = qv.iter().zip(&cand).map(|(a, b)| (a - b) * (a - b)).sum();
                best.push((d, idx));
            }
            best.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let truth: std::collections::HashSet<u64> =
                best.iter().take(10).map(|(_, id)| *id).collect();
            let got = db.search("scale", &qv, &params).unwrap();
            for m in &got {
                if let Some(idx) = m.id.strip_prefix('p').and_then(|s| s.parse::<u64>().ok())
                    && truth.contains(&idx)
                {
                    hits += 1;
                }
            }
            total += 10;
        }
        Some(hits as f64 / total as f64)
    } else {
        None
    };

    let disk = dir_bytes(data_dir.path());

    eprintln!("\n================ SCALE RESULT (measured) ================");
    let idx_desc = if pq == 0 {
        "IVF-Flat (exact)".to_string()
    } else {
        format!("IVF+PQ m={pq}")
    };
    eprintln!("vectors ...... {n}  (dim {dim}, {idx_desc})");
    eprintln!("ingest ....... {ingest_s:.1}s  → {rate:.0} vec/s (bulk)");
    eprintln!("first-build .. {build_s:.1}s (lazy index build on first query)");
    eprintln!("peak RSS ..... {} MiB", peak_rss_kib() / 1024);
    eprintln!(
        "on-disk ...... {} MiB ({} bytes/vec)",
        disk / (1024 * 1024),
        disk / n as u64
    );
    eprintln!("query p50 .... {:.2} ms", p(0.50));
    eprintln!("query p95 .... {:.2} ms", p(0.95));
    match recall {
        Some(r) => eprintln!(
            "recall@10 .... {r:.3} (brute-force ground truth, {}q)",
            queries.min(50)
        ),
        None => eprintln!("recall@10 .... skipped (N > recall cap; measure at a smaller tier)"),
    }
    eprintln!("========================================================\n");

    // Functional assertions: it ingested everything and serves queries.
    assert_eq!(db.len("scale").unwrap(), n, "not all vectors ingested");
    let got = db.search("scale", &warm, &params).unwrap();
    assert!(!got.is_empty(), "query returned nothing at scale");
}
