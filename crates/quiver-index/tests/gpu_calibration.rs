// SPDX-License-Identifier: AGPL-3.0-only
//! GPU calibration harness (ADR-0082, increment G4).
//!
//! Two `#[ignore]`d measurement runs, not CI gates — they need a real CUDA device
//! and take minutes. They exist so `GPU_MIN_BATCH` and the decision to ship a wiring
//! increment are **measured on the reference card**, never guessed:
//!
//! ```text
//! LD_LIBRARY_PATH=/usr/lib/wsl/lib \
//!   cargo test -p quiverdb-index --features cuda --release --test gpu_calibration \
//!   -- --ignored --nocapture
//! ```
//!
//! Without `LD_LIBRARY_PATH` the WSL driver is not found (`libcuda.so.1` exists but
//! `libcuda.so` is not in the loader cache), the device is silently invisible, and
//! the run measures the CPU against itself. Both harnesses **assert the GPU result
//! equals the CPU result before timing anything**, so a run that has quietly lost
//! the device fails loudly instead of reporting a flattering number.
#![cfg(feature = "cuda")]

use std::time::Instant;

use quiver_index::gpu;
use quiver_simd::Metric;

/// Deterministic pseudo-random floats — no rand dependency, and the same corpus
/// every run so two runs are comparable.
fn corpus(len: usize, seed: u64) -> Vec<f32> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 40) as f32 / 16_777_216.0 - 0.5
        })
        .collect()
}

/// Median of `reps` timings of `f`, in milliseconds.
fn median_ms(reps: usize, mut f: impl FnMut()) -> f64 {
    let mut ms: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[reps / 2]
}

fn require_device() {
    assert!(
        gpu::gpu_available(),
        "no CUDA device reachable — export LD_LIBRARY_PATH=/usr/lib/wsl/lib on WSL. \
         Refusing to publish a 'GPU' number measured on the CPU."
    );
}

/// Sweep the assign pass across batch sizes to find where the device starts to win.
/// The crossover is what `GPU_MIN_BATCH` should be; below it the CPU must win, which
/// is the property the constant exists to encode.
#[test]
#[ignore = "calibration harness — needs a CUDA device; see module docs to run"]
fn calibrate_gpu_min_batch_on_the_assign_pass() {
    require_device();
    let dim = 128usize;
    let k = 1024usize;
    let centroids = corpus(k * dim, 7);

    println!("\n=== assign pass: dim={dim}, nlist={k} (reference card) ===");
    println!(
        "{:>10}  {:>12}  {:>12}  {:>9}",
        "points", "cpu ms", "gpu ms", "speedup"
    );
    // The device path is measured with the threshold BYPASSED, so this sweep shows
    // where the crossover is rather than where the current constant says it is.
    for &n in &[
        1024usize, 2048, 4096, 6144, 8192, 12_288, 16_384, 65_536, 262_144, 1_048_576,
    ] {
        let points = corpus(n * dim, 11);
        // Correctness before timing: if the device silently did nothing, or produced
        // a materially worse assignment, the run stops here rather than reporting a
        // number. (The 1.0 A/B trap: a probe that does not check its output can
        // "measure" a path that never did the work.)
        let cpu_out = gpu::cpu_batch_assign(&points, &centroids, dim);
        let gpu_out = gpu::device_batch_assign(&points, &centroids, dim)
            .expect("device unreachable mid-sweep");
        assert_eq!(cpu_out.len(), n, "cpu assign returned the wrong row count");
        assert_eq!(gpu_out.len(), n, "gpu assign returned the wrong row count");
        // Not index equality: the SIMD kernel sums across lanes and the device sums
        // along `dim`, so two centroids within float epsilon of each other can
        // resolve the argmin differently. That is the per-backend determinism
        // ADR-0079 predicted and declined to paper over. What must hold is that the
        // device's choice is **just as good** — its centroid at (float-)equal
        // distance to the CPU's. A genuinely wrong assignment fails; a tie does not.
        let differing = disagreement(&points, &centroids, dim, &cpu_out, &gpu_out);

        let cpu_ms = median_ms(5, || {
            std::hint::black_box(gpu::cpu_batch_assign(&points, &centroids, dim));
        });
        let gpu_ms = median_ms(5, || {
            std::hint::black_box(gpu::device_batch_assign(&points, &centroids, dim));
        });
        println!(
            "{n:>10}  {cpu_ms:>12.2}  {gpu_ms:>12.2}  {:>8.2}x   (ties differing: {differing})",
            cpu_ms / gpu_ms
        );
    }
    println!(
        "GPU_MIN_BATCH is currently {} — it must be the smallest row count above \
         which the speedup stays > 1.0x.\n",
        gpu::GPU_MIN_BATCH
    );
}

/// The same sweep for the one-query-against-many-rows kernel that a flat posting
/// scan and a disk shortlist re-rank use (G3). Reported as the true metric so a
/// failure is visible, not just as a timing.
#[test]
#[ignore = "calibration harness — needs a CUDA device; see module docs to run"]
fn calibrate_the_scan_kernel() {
    require_device();
    let dim = 128usize;
    let query = corpus(dim, 3);

    println!("\n=== one query vs n rows: dim={dim} (reference card) ===");
    println!(
        "{:>10}  {:>12}  {:>12}  {:>9}",
        "rows", "cpu ms", "gpu ms", "speedup"
    );
    for &n in &[1024usize, 4096, 16_384, 65_536, 262_144, 1_048_576] {
        let batch = corpus(n * dim, 5);
        let cpu_out = gpu::cpu_batch_ordering_distance(Metric::L2, &query, &batch, dim);
        let gpu_out =
            gpu::device_batch_l2_sq(&query, &batch, dim).expect("device unreachable mid-sweep");
        assert_eq!(cpu_out.len(), n, "cpu scan returned the wrong row count");
        assert_eq!(gpu_out.len(), n, "gpu scan returned the wrong row count");
        // Float summation does not associate identically across backends, so the
        // *shortlist* is what must agree — which is the whole point of
        // narrow-then-rescore. Assert the ordering, not the bits.
        assert_eq!(
            argmin(&cpu_out),
            argmin(&gpu_out),
            "gpu and cpu disagree on the nearest row at n={n} — do not publish this run"
        );

        let cpu_ms = median_ms(5, || {
            std::hint::black_box(gpu::cpu_batch_ordering_distance(
                Metric::L2,
                &query,
                &batch,
                dim,
            ));
        });
        let gpu_ms = median_ms(5, || {
            std::hint::black_box(gpu::device_batch_l2_sq(&query, &batch, dim));
        });
        println!(
            "{n:>10}  {cpu_ms:>12.2}  {gpu_ms:>12.2}  {:>8.2}x",
            cpu_ms / gpu_ms
        );
    }
    println!();
}

/// End-to-end build wall-clock, which is the number that actually matters: a 4×
/// kernel is worth nothing if the assign pass is not where the build spends its time.
///
/// Run it twice, once per backend, because the device context is resolved once per
/// process and `QUIVER_GPU` cannot be flipped mid-run:
///
/// ```text
/// LD_LIBRARY_PATH=/usr/lib/wsl/lib cargo test -p quiverdb-index --features cuda \
///   --release --test gpu_calibration -- --ignored --nocapture measure_ivf_build_time
/// QUIVER_GPU=0 LD_LIBRARY_PATH=/usr/lib/wsl/lib cargo test ...same...
/// ```
#[test]
#[ignore = "measurement harness — run once per backend; see the doc comment"]
fn measure_ivf_build_time() {
    let dim = 128usize;
    let n = 200_000usize;
    let nlist = 1024usize;
    let vectors = corpus(n * dim, 31);
    let ids: Vec<u64> = (0..n as u64).collect();
    let config = quiver_index::IvfConfig {
        nlist,
        ..Default::default()
    };

    let t = Instant::now();
    let ivf = quiver_index::Ivf::build(&ids, &vectors, dim, Metric::L2, config).expect("build");
    let elapsed = t.elapsed().as_secs_f64();

    // The build must have actually happened — an empty or degenerate index would
    // "build" instantly and report a spectacular, meaningless speedup.
    let hits = ivf.search(&vectors[..dim], 10, 16).expect("search");
    assert_eq!(hits.len(), 10, "index answered {} of 10", hits.len());

    println!(
        "IVF build n={n} dim={dim} nlist={nlist}: {elapsed:.2}s  (backend: {})",
        if gpu::gpu_available() { "GPU" } else { "CPU" }
    );
}

/// How many points the two backends assigned differently — **asserting as it counts
/// that every difference is a genuine tie**, i.e. the device's centroid is at
/// float-equal distance to the CPU's. A device that assigns a materially worse
/// centroid fails here rather than being counted as a tie.
fn disagreement(points: &[f32], centroids: &[f32], dim: usize, cpu: &[u32], gpu: &[u32]) -> usize {
    let mut differing = 0usize;
    for (i, (&c, &g)) in cpu.iter().zip(gpu).enumerate() {
        if c == g {
            continue;
        }
        differing += 1;
        let p = &points[i * dim..(i + 1) * dim];
        let d = |idx: u32| {
            let o = idx as usize * dim;
            quiver_simd::l2_sq_f32(p, &centroids[o..o + dim])
        };
        let (dc, dg) = (d(c), d(g));
        assert!(
            (dc - dg).abs() <= 1e-4 * dc.abs().max(1.0),
            "point {i}: gpu picked centroid {g} (d={dg}) over cpu's {c} (d={dc}) — \
             that is not a tie, it is a wrong assignment"
        );
    }
    differing
}

/// End-to-end: a real IVF build large enough to cross `GPU_MIN_BATCH`, so the device
/// actually runs the assign pass, followed by a recall check. This is the test that
/// proves the *wiring* works, not just the kernel — a GPU-assisted build must produce
/// an index that answers queries as well as a CPU-built one.
///
/// Not `#[ignore]`d for want of a device: it self-skips without one, like the kernel
/// validation test, so it is a no-op on a GPU-less machine.
#[test]
fn a_gpu_assisted_ivf_build_produces_a_working_index() {
    if !gpu::gpu_available() {
        eprintln!("no CUDA device; skipping the GPU build validation");
        return;
    }
    let dim = 32usize;
    let n = 20_000usize; // > GPU_MIN_BATCH, so the assign pass reaches the device
    let vectors = corpus(n * dim, 23);
    let ids: Vec<u64> = (0..n as u64).collect();
    let config = quiver_index::IvfConfig {
        nlist: 64,
        ..Default::default()
    };
    let ivf = quiver_index::Ivf::build(&ids, &vectors, dim, Metric::L2, config)
        .expect("gpu-assisted ivf build");

    // Recall against brute force over the same corpus. The GPU only chose which
    // cell each point landed in; every distance reported here is still the CPU
    // SIMD kernel's, per the ADR-0079 contract.
    let k = 10usize;
    let mut hits = 0usize;
    let probes = 20usize;
    for q in 0..probes {
        let query = &vectors[q * dim..(q + 1) * dim];
        let got: Vec<u64> = ivf
            .search(query, k, 16)
            .expect("search")
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(got.len(), k, "query {q} returned {} of {k}", got.len());
        let mut exact: Vec<(f32, u64)> = (0..n)
            .map(|i| {
                (
                    quiver_simd::l2_sq_f32(query, &vectors[i * dim..(i + 1) * dim]),
                    i as u64,
                )
            })
            .collect();
        exact.sort_by(|a, b| a.0.total_cmp(&b.0));
        let want: Vec<u64> = exact[..k].iter().map(|&(_, id)| id).collect();
        hits += got.iter().filter(|id| want.contains(id)).count();
    }
    let recall = hits as f64 / (probes * k) as f64;
    assert!(
        recall >= 0.80,
        "gpu-assisted build recall@{k} = {recall:.3}, below the 0.80 floor"
    );
    println!("gpu-assisted IVF build: recall@{k} = {recall:.3} over {probes} queries");
}

fn argmin(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .fold((0usize, f32::INFINITY), |(bi, bd), (i, &d)| {
            if d < bd { (i, d) } else { (bi, bd) }
        })
        .0
}
