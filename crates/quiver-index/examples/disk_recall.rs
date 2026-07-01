// SPDX-License-Identifier: AGPL-3.0-only
//! Benchmark the disk-resident DiskANN path on a SIFT-style dataset, in two
//! phases so the frugal **serve-time** memory can be measured cleanly.
//!
//! - `build` reads the base vectors (RAM-heavy, one-time), constructs the Vamana
//!   graph + product-quantizer codebook, and writes the **encrypted
//!   page-structured disk index**. It reports build time, the on-disk index size,
//!   and the RAM-resident PQ-code footprint vs full-precision vectors.
//! - `serve` opens the index through `mmap` (so only the PQ codes are resident)
//!   and runs the recall@10 / QPS sweep. Measure *this* process's resident set to
//!   get the memory-frugality headline (see
//!   `docs/benchmarks/reference-hardware-runbook.md`).
//!
//! Recall and the byte footprints are host-independent; QPS is indicative on a
//! shared box. We never fabricate results.
//!
//! ```text
//! cargo run --release --example disk_recall -- build base.fvecs index.qvx
//! cargo run --release --example disk_recall -- serve index.qvx query.fvecs gt.ivecs
//! ```

use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Instant;

use quiver_core::page::PlainCodec;
use quiver_index::{
    DiskSearchParams, DiskVamana, Metric, ProductQuantizer, Vamana, VamanaConfig, disk,
};

const K: usize = 10;
const L_SWEEP: [usize; 4] = [16, 32, 64, 128];

type BoxErr = Box<dyn Error>;

fn main() -> Result<(), BoxErr> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("build") => match args.as_slice() {
            [_, _, base, out, rest @ ..] => build(Path::new(base), Path::new(out), rest.first()),
            _ => Err("usage: disk_recall build <base.fvecs> <index.qvx> [pq_subspaces]".into()),
        },
        Some("serve") => match args.as_slice() {
            [_, _, index, query, gt] => serve(Path::new(index), Path::new(query), Path::new(gt)),
            _ => Err("usage: disk_recall serve <index.qvx> <query.fvecs> <gt.ivecs>".into()),
        },
        _ => Err("usage: disk_recall <build|serve> ...".into()),
    }
}

// --- build phase ---

fn build(base_path: &Path, out: &Path, pq_arg: Option<&String>) -> Result<(), BoxErr> {
    let (base, dim) = read_fvecs(base_path)?;
    let n = base.len();
    let flat: Vec<f32> = base.iter().flatten().copied().collect();
    let ids: Vec<u64> = (0..n as u64).collect();
    let m = match pq_arg {
        Some(s) => s.parse()?,
        None => pq_subspaces(dim),
    };
    println!("build: n={n} dim={dim} pq_subspaces={m}");

    let timer = Instant::now();
    let graph = Vamana::build(&ids, &flat, dim, Metric::L2, VamanaConfig::default())?;
    let pq = ProductQuantizer::train(&flat, n, dim, m, Metric::L2, 0x5176_5044_5141_5453)?;
    disk::write(out, &graph, &pq, &PlainCodec)?;
    println!("  built in {:.1}s", timer.elapsed().as_secs_f64());

    let pq_ram = n * m;
    let full_ram = n * dim * 4;
    let on_disk = fs::metadata(out)?.len();
    println!(
        "  RAM-resident codes: {:.1} MB  vs full-precision: {:.1} MB  ({:.0}x smaller)",
        pq_ram as f64 / 1e6,
        full_ram as f64 / 1e6,
        full_ram as f64 / pq_ram as f64,
    );
    println!(
        "  on-disk index: {:.1} MB → {}",
        on_disk as f64 / 1e6,
        out.display()
    );
    Ok(())
}

// --- serve phase (measure this process's RSS for the frugal footprint) ---

fn serve(index_path: &Path, query_path: &Path, gt_path: &Path) -> Result<(), BoxErr> {
    let index = DiskVamana::open(index_path, Box::new(PlainCodec))?;
    let (queries, _) = read_fvecs(query_path)?;
    let truth = read_ivecs(gt_path)?;
    if queries.len() != truth.len() {
        return Err("query count does not match ground-truth rows".into());
    }
    println!("serve: index={} queries={}", index.len(), queries.len());
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
    // Hold at steady state (only PQ codes resident) so the resident-set memory
    // can be sampled. `QUIVER_DISK_HOLD_SECS` makes the hold deterministic for a
    // scripted sampler (no TTY needed — e.g. scripts/bench-disk-frugality.ps1);
    // otherwise an interactive run waits for Enter.
    // NOTE: env hold over a CLI flag — the example's arg parsing is positional.
    if let Some(secs) = env::var("QUIVER_DISK_HOLD_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        println!("\nindex loaded; holding {secs}s for RSS sampling");
        std::io::stdout().flush()?;
        std::thread::sleep(std::time::Duration::from_secs(secs));
    } else if std::io::stdin().is_terminal() {
        println!("\nindex loaded; sample this process's RSS now, then press Enter to exit");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
    }
    Ok(())
}

// --- dataset readers (standard SIFT .fvecs / .ivecs) ---

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
