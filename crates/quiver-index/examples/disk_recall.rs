// SPDX-License-Identifier: AGPL-3.0-only
//! Measure the disk-resident DiskANN path on a SIFT-style dataset.
//!
//! Builds the Vamana graph + product-quantizer codebook, writes the **encrypted
//! page-structured disk index**, opens it through `mmap`, and queries it —
//! reporting recall@10, build time, the on-disk index size, and the
//! **RAM-resident PQ-code footprint** versus full-precision vectors (the
//! memory-frugality headline). Recall and the byte footprints are
//! host-independent; QPS here is indicative only (shared dev box), per
//! `docs/benchmarks/methodology.md`. We never fabricate results.
//!
//! ```text
//! cargo run --release --example disk_recall -- \
//!   bench/datasets/siftsmall/siftsmall_base.fvecs \
//!   bench/datasets/siftsmall/siftsmall_query.fvecs \
//!   bench/datasets/siftsmall/siftsmall_groundtruth.ivecs
//! ```

use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use quiver_core::page::PlainCodec;
use quiver_index::{
    DiskSearchParams, DiskVamana, Metric, ProductQuantizer, Vamana, VamanaConfig, disk,
};

const K: usize = 10;
const L_SWEEP: [usize; 4] = [16, 32, 64, 128];

type BoxErr = Box<dyn Error>;

fn read_fvecs(path: &Path) -> Result<(Vec<Vec<f32>>, usize), BoxErr> {
    let bytes = fs::read(path)?;
    let (mut rows, mut dim, mut offset) = (Vec::new(), 0usize, 0usize);
    while offset + 4 <= bytes.len() {
        let d = read_u32(&bytes, offset) as usize;
        dim = d;
        offset += 4;
        let end = offset + d * 4;
        if end > bytes.len() {
            return Err("truncated .fvecs record".into());
        }
        rows.push(
            bytes[offset..end]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        );
        offset = end;
    }
    Ok((rows, dim))
}

fn read_ivecs(path: &Path) -> Result<Vec<Vec<u32>>, BoxErr> {
    let bytes = fs::read(path)?;
    let (mut rows, mut offset) = (Vec::new(), 0usize);
    while offset + 4 <= bytes.len() {
        let d = read_u32(&bytes, offset) as usize;
        offset += 4;
        let end = offset + d * 4;
        if end > bytes.len() {
            return Err("truncated .ivecs record".into());
        }
        rows.push(
            bytes[offset..end]
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        );
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

// Largest PQ subspace count that divides `dim`, targeting ~8 dims per subspace.
fn pq_subspaces(dim: usize) -> usize {
    let target = (dim / 8).max(1);
    (1..=target)
        .rev()
        .find(|&m| dim.is_multiple_of(m))
        .unwrap_or(1)
}

fn main() -> Result<(), BoxErr> {
    let args: Vec<String> = env::args().collect();
    let [_, base_path, query_path, gt_path] = args.as_slice() else {
        return Err("usage: disk_recall <base.fvecs> <query.fvecs> <groundtruth.ivecs>".into());
    };

    let (base, dim) = read_fvecs(Path::new(base_path))?;
    let (queries, _) = read_fvecs(Path::new(query_path))?;
    let truth = read_ivecs(Path::new(gt_path))?;
    if queries.len() != truth.len() {
        return Err("query count does not match ground-truth rows".into());
    }
    let n = base.len();
    let flat: Vec<f32> = base.iter().flatten().copied().collect();
    let ids: Vec<u64> = (0..n as u64).collect();
    let m = pq_subspaces(dim);
    println!(
        "base={n} queries={} dim={dim} pq_subspaces={m}",
        queries.len()
    );

    let build = Instant::now();
    let graph = Vamana::build(&ids, &flat, dim, Metric::L2, VamanaConfig::default())?;
    let pq = ProductQuantizer::train(&flat, n, dim, m, Metric::L2, 0x5176_5044_5141_5453)?;
    let path = env::temp_dir().join("quiver_disk_recall.qvx");
    disk::write(&path, &graph, &pq, &PlainCodec)?;
    let index = DiskVamana::open(&path, Box::new(PlainCodec))?;
    println!("build: {:.1}s\n", build.elapsed().as_secs_f64());

    // Memory: only the PQ codes are RAM-resident; full vectors live on disk.
    let pq_ram = n * m;
    let full_ram = n * dim * 4;
    let on_disk = fs::metadata(&path)?.len();
    println!(
        "RAM-resident codes: {:.1} MB  vs full-precision vectors: {:.1} MB  ({:.0}x smaller)",
        pq_ram as f64 / 1e6,
        full_ram as f64 / 1e6,
        full_ram as f64 / pq_ram as f64,
    );
    println!("encrypted on-disk index: {:.1} MB\n", on_disk as f64 / 1e6);
    std::io::stdout().flush()?;

    println!("{:>9}  {:>9}  {:>10}", "l_search", "recall@10", "qps_1t");
    for l in L_SWEEP {
        let mut hits = 0usize;
        let timer = Instant::now();
        for (qi, query) in queries.iter().enumerate() {
            let found = index.search(query, K, &DiskSearchParams { l_search: l })?;
            let want: HashSet<u32> = truth[qi].iter().take(K).copied().collect();
            hits += found
                .iter()
                .take(K)
                .filter(|nbr| want.contains(&(nbr.id as u32)))
                .count();
        }
        let recall = hits as f64 / (queries.len() * K) as f64;
        let qps = queries.len() as f64 / timer.elapsed().as_secs_f64();
        println!("{l:>9}  {recall:>9.4}  {qps:>10.0}");
        std::io::stdout().flush()?;
    }
    fs::remove_file(&path).ok();
    Ok(())
}
