// SPDX-License-Identifier: AGPL-3.0-only
//! Measure HNSW recall@10 on a SIFT-style dataset, in memory.
//!
//! Recall is a property of the index and the data — it does not depend on the
//! storage engine or the network transport — so this measures it directly,
//! sidestepping the durable write path's per-point `fsync` (which makes loading
//! millions of vectors through the server impractical for a quick measurement).
//! It complements the end-to-end `ann-benchmarks`-style harness in `bench/`.
//!
//! Inputs are the standard `.fvecs` / `.ivecs` files (a little-endian `int32`
//! length followed by that many `float32`/`int32`). Run, e.g.:
//!
//! ```text
//! cargo run --release --example sift_recall -- \
//!   bench/datasets/sift1m/sift_base.fvecs \
//!   bench/datasets/sift1m/sift_query.fvecs \
//!   bench/datasets/sift1m/sift_groundtruth.ivecs
//! ```

use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use quiver_index::{Hnsw, HnswConfig, Index, Metric};

const K: usize = 10;
const EF_SWEEP: [usize; 5] = [16, 32, 64, 128, 256];

type BoxErr = Box<dyn Error>;
type Vectors = Vec<Vec<f32>>;

// Read a `.fvecs` file into rows of f32, returning the rows and the dimension.
fn read_fvecs(path: &Path) -> Result<(Vectors, usize), BoxErr> {
    let bytes = fs::read(path)?;
    let mut rows = Vec::new();
    let mut dim = 0usize;
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let d = read_u32(&bytes, offset) as usize;
        dim = d;
        offset += 4;
        let end = offset + d * 4;
        if end > bytes.len() {
            return Err("truncated .fvecs record".into());
        }
        let row = bytes[offset..end]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        rows.push(row);
        offset = end;
    }
    Ok((rows, dim))
}

// Read an `.ivecs` file into rows of u32 (e.g. ground-truth neighbour indices).
fn read_ivecs(path: &Path) -> Result<Vec<Vec<u32>>, BoxErr> {
    let bytes = fs::read(path)?;
    let mut rows = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let d = read_u32(&bytes, offset) as usize;
        offset += 4;
        let end = offset + d * 4;
        if end > bytes.len() {
            return Err("truncated .ivecs record".into());
        }
        let row = bytes[offset..end]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        rows.push(row);
        offset = end;
    }
    Ok(rows)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn main() -> Result<(), BoxErr> {
    let args: Vec<String> = env::args().collect();
    let [_, base_path, query_path, gt_path] = args.as_slice() else {
        return Err("usage: sift_recall <base.fvecs> <query.fvecs> <groundtruth.ivecs>".into());
    };

    let (base, dim) = read_fvecs(Path::new(base_path))?;
    let (queries, _) = read_fvecs(Path::new(query_path))?;
    let truth = read_ivecs(Path::new(gt_path))?;
    if queries.len() != truth.len() {
        return Err("query count does not match ground-truth rows".into());
    }
    println!("base={} queries={} dim={dim}", base.len(), queries.len());

    let mut index = Hnsw::new(dim, Metric::L2, HnswConfig::default());
    let build = Instant::now();
    for (id, vector) in base.iter().enumerate() {
        index.insert(id as u64, vector)?;
    }
    println!(
        "build: {:.1}s for {} vectors\n",
        build.elapsed().as_secs_f64(),
        base.len()
    );
    std::io::stdout().flush()?;

    println!("{:>9}  {:>9}  {:>10}", "ef_search", "recall@10", "qps_1t");
    for ef in EF_SWEEP {
        let mut hits = 0usize;
        let timer = Instant::now();
        for (qi, query) in queries.iter().enumerate() {
            let found = index.search(query, K, ef)?;
            let want: HashSet<u32> = truth[qi].iter().take(K).copied().collect();
            hits += found
                .iter()
                .take(K)
                .filter(|n| want.contains(&(n.id as u32)))
                .count();
        }
        let elapsed = timer.elapsed().as_secs_f64();
        let recall = hits as f64 / (queries.len() * K) as f64;
        let qps = queries.len() as f64 / elapsed;
        println!("{ef:>9}  {recall:>9.4}  {qps:>10.0}");
        std::io::stdout().flush()?;
    }
    Ok(())
}
