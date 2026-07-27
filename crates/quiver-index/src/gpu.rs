// SPDX-License-Identifier: AGPL-3.0-only
//! GPU-accelerated batch distance (ADR-0052), behind the off-by-default `cuda`
//! feature.
//!
//! The hot kernel a brute-force / exact scan spends its time in is "distance from
//! one query to many vectors". [`batch_l2_sq`] computes it on the GPU when the
//! `cuda` feature is enabled **and** a CUDA device is present, and otherwise falls
//! back to the CPU SIMD kernel ([`quiver_simd::l2_sq_f32`]) — the always-compiled
//! default. Results are identical (the same arithmetic), so a build without the
//! feature, or a machine without a GPU, is unaffected: the GPU is a pure
//! accelerator behind this seam, never a correctness dependency, and the on-disk
//! format and crash gate are untouched.
//!
//! cudarc dynamically loads the CUDA driver and compiles the kernel with NVRTC at
//! runtime, so the `cuda` feature even *builds* without a CUDA toolchain — only
//! *running* the GPU path needs a device. Validated on real hardware: the
//! `cuda`-gated test asserts the GPU result equals the CPU kernel and is otherwise
//! skipped (so it is a no-op where no GPU is present).
//!
//! "Never a correctness dependency" has to survive the driver being *absent*, which
//! is the common case on a self-hosted box. That is less automatic than it looks:
//! cudarc loads the driver from a lazy static that **panics** when `dlopen` fails,
//! so a fallible-looking `CudaDevice::new(0)?` does not, on its own, degrade to the
//! CPU path — it unwinds out of the caller. `init_or_none` is where that is
//! contained: any failure to reach a device, `Err` or panic, becomes `None` and the
//! CPU kernel answers. (A driver present under a name the loader does not search —
//! WSL ships `libcuda.so.1` with no `libcuda.so` in the cache — looks exactly like
//! "no GPU" and takes the same path; export `LD_LIBRARY_PATH=/usr/lib/wsl/lib` to
//! let the GPU be found there.)
//!
//! # The seam ([ADR-0079], [ADR-0082])
//!
//! Every distance batch in the engine reduces to one primitive — *one vector against
//! a contiguous batch, argmin or top-k over the result* — so the seam is two free
//! functions rather than a backend trait:
//!
//! - [`batch_ordering_distance`] — the smaller-is-closer ordering key for all three
//!   metrics, so callers do not each re-derive the orientation.
//! - [`batch_assign`] — the argmin over centroids for every point, **fused**: the
//!   intermediate `points × centroids` matrix is 16 GB at 1M × 4096 and must never
//!   be returned to the host.
//!
//! Dispatch is governed by three rules, all of them here and nowhere else:
//!
//! 1. **The CPU is the default and the fallback**, always compiled.
//! 2. **Below [`GPU_MIN_BATCH`] rows the CPU wins** — transfer plus launch latency
//!    exceeds the arithmetic. The constant is a *hardware* number, calibrated
//!    against a real card, not a universal one.
//! 3. **`QUIVER_GPU=0` forces the CPU path** in a `cuda`-enabled build, so an A/B
//!    measurement and a CPU-backend reproduction stay possible on a GPU box.
//!
//! [ADR-0079]: https://github.com/achref-soua/quiver/blob/main/docs/adr/0079-gpu-build-search-wiring.md
//! [ADR-0082]: https://github.com/achref-soua/quiver/blob/main/docs/adr/0082-gpu-seam-and-dispatch-policy.md

use quiver_simd::Metric;

use crate::score::ordering_distance;

/// Row count below which the CPU kernel wins outright: PCIe transfer plus kernel
/// launch latency exceeds the arithmetic saved, so dispatching to the device is a
/// net loss on small batches.
///
/// This is a **hardware constant, measured, not assumed**: on the reference card
/// (RTX 3070 Laptop, WSL2) the assign pass at `dim = 128`, `nlist = 1024` runs
/// 0.82× the CPU's speed at 6144 points and 1.07× at 8192 — so 8192 is the smallest
/// batch above which the device stays ahead, reaching 4.08× at 1M. The sweep it came
/// from is in [ADR-0082] and is re-runnable (`tests/gpu_calibration.rs`). A different
/// GPU has a different crossover; it stays a named constant precisely so it can be
/// re-measured rather than guessed at forever.
///
/// [ADR-0082]: https://github.com/achref-soua/quiver/blob/main/docs/adr/0082-gpu-seam-and-dispatch-policy.md
pub const GPU_MIN_BATCH: usize = 8192;

/// Whether a batch of `n` rows is large enough to be worth a device dispatch.
/// Split out from the call sites so the policy is stated once and tested directly —
/// including on a default (GPU-less) build, which is where CI runs.
#[cfg(any(feature = "cuda", test))]
#[must_use]
fn worth_dispatching(n: usize) -> bool {
    n >= GPU_MIN_BATCH
}

/// Whether `QUIVER_GPU` asks for the CPU path. Takes the value rather than reading
/// the environment so the policy is testable without a process-global mutation.
#[cfg(any(feature = "cuda", test))]
#[must_use]
fn disabled_by_env(value: Option<&str>) -> bool {
    matches!(value, Some("0"))
}

/// Run `build`, turning **any** failure into `None`: an `Err`, or a panic from a
/// dependency that panics where it should return (the CUDA driver loader does).
///
/// Kept out of the `cuda` module, and so always compiled and always tested, because
/// it is the thing standing between "the GPU is an accelerator" and "the GPU is a
/// crash on machines that do not have one".
#[cfg(any(feature = "cuda", test))]
fn init_or_none<T>(
    what: &str,
    build: impl FnOnce() -> Result<T, Box<dyn std::error::Error>>,
) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)) {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            eprintln!("{what} unavailable ({e}); using the CPU kernel");
            None
        }
        Err(_) => {
            eprintln!("{what} unavailable (the driver could not be loaded); using the CPU kernel");
            None
        }
    }
}

/// Squared-L2 distance from `query` to each `dim`-length row of the contiguous
/// `batch` (so `batch.len() == n * dim`), returning the `n` distances in row order.
/// Uses the GPU when the `cuda` feature is enabled and a device is available, else
/// the CPU SIMD kernel.
#[must_use]
pub fn batch_l2_sq(query: &[f32], batch: &[f32], dim: usize) -> Vec<f32> {
    debug_assert!(dim > 0 && batch.len().is_multiple_of(dim) && query.len() == dim);
    #[cfg(feature = "cuda")]
    {
        if worth_dispatching(batch.len() / dim)
            && let Some(ctx) = cuda::context()
            && let Ok(out) = ctx.batch_l2_sq(query, batch, dim)
        {
            return out;
        }
    }
    cpu_batch_l2_sq(query, batch, dim)
}

/// The **ordering key** — smaller is closer — from `query` to each `dim`-length row
/// of the contiguous `batch`, under `metric`. Equivalent row-for-row to
/// [`crate::ordering_distance`], batched so the work can go to a device.
///
/// Only [`Metric::L2`] has a device kernel today; [`Metric::Dot`] and
/// [`Metric::Cosine`] fall back per-metric to the CPU kernel, which is exactly what
/// a caller that has not been ported yet already did.
#[must_use]
pub fn batch_ordering_distance(
    metric: Metric,
    query: &[f32],
    batch: &[f32],
    dim: usize,
) -> Vec<f32> {
    debug_assert!(dim > 0 && batch.len().is_multiple_of(dim) && query.len() == dim);
    match metric {
        Metric::L2 => batch_l2_sq(query, batch, dim),
        Metric::Dot | Metric::Cosine => cpu_batch_ordering_distance(metric, query, batch, dim),
    }
}

/// The always-compiled CPU path for [`batch_ordering_distance`].
#[must_use]
pub fn cpu_batch_ordering_distance(
    metric: Metric,
    query: &[f32],
    batch: &[f32],
    dim: usize,
) -> Vec<f32> {
    batch
        .chunks_exact(dim)
        .map(|row| ordering_distance(metric, query, row))
        .collect()
}

/// For each `dim`-length point in the contiguous `points`, the index of its nearest
/// centroid by squared L2 — the k-means assignment step and the IVF build's assign
/// pass, which together are the largest arithmetic block in a large build
/// (`O(n · nlist · dim)`).
///
/// The argmin is **fused into the kernel** rather than computed from a returned
/// distance matrix: that matrix is `points × centroids × 4 B` — 16 GB at 1M points
/// and 4096 centroids — so materialising it would cost more than the arithmetic it
/// is meant to accelerate. Only `n` `u32`s ever cross back.
///
/// Returns all-zero assignments when `centroids` is empty, mirroring the
/// single-point path.
#[must_use]
pub fn batch_assign(points: &[f32], centroids: &[f32], dim: usize) -> Vec<u32> {
    debug_assert!(
        dim > 0 && points.len().is_multiple_of(dim) && centroids.len().is_multiple_of(dim)
    );
    #[cfg(feature = "cuda")]
    {
        if worth_dispatching(points.len() / dim)
            && let Some(ctx) = cuda::context()
            && let Ok(out) = ctx.batch_assign(points, centroids, dim)
        {
            return out;
        }
    }
    cpu_batch_assign(points, centroids, dim)
}

/// The always-compiled CPU path for [`batch_assign`]: the SIMD L2 kernel per
/// (point, centroid) pair, taking the first minimum on a tie.
#[must_use]
pub fn cpu_batch_assign(points: &[f32], centroids: &[f32], dim: usize) -> Vec<u32> {
    points
        .chunks_exact(dim)
        .map(|p| {
            let mut best = 0u32;
            let mut best_d = f32::INFINITY;
            for (c, centroid) in centroids.chunks_exact(dim).enumerate() {
                let d = quiver_simd::l2_sq_f32(p, centroid);
                if d < best_d {
                    best_d = d;
                    best = c as u32;
                }
            }
            best
        })
        .collect()
}

/// The always-compiled CPU path (and the GPU fallback): the SIMD `l2_sq_f32` per row.
#[must_use]
pub fn cpu_batch_l2_sq(query: &[f32], batch: &[f32], dim: usize) -> Vec<f32> {
    batch
        .chunks_exact(dim)
        .map(|row| quiver_simd::l2_sq_f32(query, row))
        .collect()
}

/// Whether a GPU is actually available to accelerate distances (the `cuda` feature
/// is on and a device initialised). `false` on the default build, so a caller can
/// log which path it is using.
#[cfg(feature = "cuda")]
#[must_use]
pub fn gpu_available() -> bool {
    cuda::context().is_some()
}

/// The device assign path with the [`GPU_MIN_BATCH`] threshold **bypassed**, or
/// `None` if no device is reachable.
///
/// This exists so the calibration harness can measure *where* the CPU/GPU crossover
/// actually is, rather than only confirming whatever the constant currently says —
/// below the threshold `batch_assign` runs the CPU path, so a sweep through it would
/// otherwise be timing the CPU against itself and calling the result a speedup.
/// Not part of the supported surface: it does not exist on a default build.
#[cfg(feature = "cuda")]
#[doc(hidden)]
#[must_use]
pub fn device_batch_assign(points: &[f32], centroids: &[f32], dim: usize) -> Option<Vec<u32>> {
    cuda::context().and_then(|ctx| ctx.batch_assign(points, centroids, dim).ok())
}

/// The device scan path with the [`GPU_MIN_BATCH`] threshold bypassed. Same purpose
/// and same caveats as [`device_batch_assign`].
#[cfg(feature = "cuda")]
#[doc(hidden)]
#[must_use]
pub fn device_batch_l2_sq(query: &[f32], batch: &[f32], dim: usize) -> Option<Vec<f32>> {
    cuda::context().and_then(|ctx| ctx.batch_l2_sq(query, batch, dim).ok())
}

/// Whether a GPU is available — always `false` without the `cuda` feature.
#[cfg(not(feature = "cuda"))]
#[must_use]
pub fn gpu_available() -> bool {
    false
}

#[cfg(feature = "cuda")]
mod cuda {
    use std::sync::{Arc, OnceLock};

    use cudarc::driver::{CudaDevice, LaunchAsync, LaunchConfig};
    use cudarc::nvrtc::compile_ptx;

    /// Points per device tile, as a byte budget rather than a row count: a tile has
    /// to fit in VRAM alongside the centroids, and `dim` varies by three orders of
    /// magnitude across collections. 256 MiB is comfortably inside the reference
    /// card's 8 GiB with room for the centroid table and the output.
    ///
    /// This is what keeps ADR-0079's "no dataset residency in VRAM" non-goal true:
    /// the corpus streams, so a build is never capped at the card's memory.
    const TILE_BYTES: usize = 256 << 20;

    // Two NVRTC-compiled kernels:
    //   l2sq   — out[r] = Σ_j (a[r*d+j] − q[j])², one query against many rows.
    //   assign — the fused argmin: one u32 per point, never the n×k distance matrix
    //            (16 GB at 1M × 4096). Ties take the lowest centroid index, matching
    //            the CPU `nearest_centroid`, so the backends agree wherever float
    //            arithmetic lets them.
    const KERNEL: &str = r#"
extern "C" __global__ void l2sq(const float* q, const float* a, float* out, int n, int d) {
  int r = blockIdx.x * blockDim.x + threadIdx.x;
  if (r >= n) return;
  float s = 0.0f;
  for (int j = 0; j < d; j++) { float diff = a[r * d + j] - q[j]; s += diff * diff; }
  out[r] = s;
}

extern "C" __global__ void assign(const float* pts, const float* cents, unsigned int* out,
                                  int n, int k, int d) {
  int i = blockIdx.x * blockDim.x + threadIdx.x;
  if (i >= n) return;
  const float* p = pts + (long long)i * d;
  float best = 0.0f;
  for (int j = 0; j < d; j++) { float diff = p[j] - cents[j]; best += diff * diff; }
  unsigned int bi = 0;
  for (int c = 1; c < k; c++) {
    const float* q = cents + (long long)c * d;
    float s = 0.0f;
    for (int j = 0; j < d; j++) { float diff = p[j] - q[j]; s += diff * diff; }
    if (s < best) { best = s; bi = (unsigned int)c; }
  }
  out[i] = bi;
}"#;

    pub(super) struct Context {
        dev: Arc<CudaDevice>,
    }

    impl Context {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let dev = CudaDevice::new(0)?;
            let ptx = compile_ptx(KERNEL)?;
            dev.load_ptx(ptx, "quiver_l2", &["l2sq", "assign"])?;
            Ok(Self { dev })
        }

        /// The fused argmin over `centroids` for every `dim`-length point, tiled so
        /// the corpus streams through VRAM instead of having to fit in it. The
        /// centroid table is uploaded once and reused across tiles.
        pub(super) fn batch_assign(
            &self,
            points: &[f32],
            centroids: &[f32],
            dim: usize,
        ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
            let n = points.len() / dim;
            let k = centroids.len() / dim;
            if n == 0 || k == 0 {
                return Ok(vec![0u32; n]);
            }
            let cents = self.dev.htod_sync_copy(centroids)?;
            let f = self
                .dev
                .get_func("quiver_l2", "assign")
                .ok_or("assign kernel not loaded")?;
            let tile_rows = (TILE_BYTES / (dim * size_of::<f32>())).max(1);
            let mut out = Vec::with_capacity(n);
            for tile in points.chunks(tile_rows * dim) {
                let rows = tile.len() / dim;
                let pts = self.dev.htod_sync_copy(tile)?;
                let mut dst = self.dev.alloc_zeros::<u32>(rows)?;
                let cfg = LaunchConfig::for_num_elems(rows as u32);
                // Safety: the kernel reads `pts[0..rows*d]` and `cents[0..k*d]` and
                // writes `dst[0..rows]`, all sized exactly here.
                unsafe {
                    f.clone().launch(
                        cfg,
                        (&pts, &cents, &mut dst, rows as i32, k as i32, dim as i32),
                    )?;
                }
                out.extend_from_slice(&self.dev.dtoh_sync_copy(&dst)?);
            }
            Ok(out)
        }

        pub(super) fn batch_l2_sq(
            &self,
            query: &[f32],
            batch: &[f32],
            dim: usize,
        ) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
            let n = batch.len() / dim;
            if n == 0 {
                return Ok(Vec::new());
            }
            let q = self.dev.htod_sync_copy(query)?;
            let a = self.dev.htod_sync_copy(batch)?;
            let mut out = self.dev.alloc_zeros::<f32>(n)?;
            let f = self
                .dev
                .get_func("quiver_l2", "l2sq")
                .ok_or("l2sq kernel not loaded")?;
            let cfg = LaunchConfig::for_num_elems(n as u32);
            // Safety: the kernel reads `q[0..d]`, `a[0..n*d]` and writes `out[0..n]`,
            // all of which are sized exactly here.
            unsafe {
                f.launch(cfg, (&q, &a, &mut out, n as i32, dim as i32))?;
            }
            Ok(self.dev.dtoh_sync_copy(&out)?)
        }
    }

    // The process-wide GPU context, built once. `None` if no device is present, init
    // fails, or `QUIVER_GPU=0` asks for the CPU path. Init runs through
    // `init_or_none` because cudarc's driver loader panics rather than returning
    // `Err` when `dlopen` cannot find the CUDA library.
    //
    // Which backend the process settled on is logged exactly once, here — never per
    // batch, which would be one line per query.
    pub(super) fn context() -> Option<&'static Context> {
        static CTX: OnceLock<Option<Context>> = OnceLock::new();
        CTX.get_or_init(|| {
            if super::disabled_by_env(std::env::var("QUIVER_GPU").ok().as_deref()) {
                eprintln!("cuda: disabled by QUIVER_GPU=0; using the CPU kernel");
                return None;
            }
            let ctx = super::init_or_none("cuda: GPU acceleration", Context::new);
            if ctx.is_some() {
                eprintln!(
                    "cuda: GPU acceleration active (batches of ≥ {} rows)",
                    super::GPU_MIN_BATCH
                );
            }
            ctx
        })
        .as_ref()
    }

    #[cfg(test)]
    mod tests {
        use crate::gpu::{batch_l2_sq, cpu_batch_l2_sq};

        // Runs only where a GPU is present (validated locally on the RTX 3070);
        // a no-op where no device is available, so it is safe everywhere.
        #[test]
        fn gpu_batch_matches_the_cpu_kernel() {
            if super::context().is_none() {
                eprintln!("no CUDA device; skipping GPU validation");
                return;
            }
            let dim = 64usize;
            let n = 2000usize;
            let query: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.07).collect();
            let batch: Vec<f32> = (0..n * dim).map(|i| ((i % 211) as f32) * 0.013).collect();
            let gpu = batch_l2_sq(&query, &batch, dim);
            let cpu = cpu_batch_l2_sq(&query, &batch, dim);
            assert_eq!(gpu.len(), n);
            for (g, c) in gpu.iter().zip(&cpu) {
                assert!(
                    (g - c).abs() <= 1e-3 * c.max(1.0),
                    "gpu {g} != cpu {c} (relative)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use quiver_simd::Metric;

    use super::{
        GPU_MIN_BATCH, batch_assign, batch_l2_sq, batch_ordering_distance, cpu_batch_assign,
        cpu_batch_l2_sq, disabled_by_env, init_or_none, worth_dispatching,
    };

    #[test]
    fn init_or_none_passes_through_a_working_backend() {
        assert_eq!(init_or_none("test", || Ok(7u32)), Some(7));
    }

    #[test]
    fn init_or_none_turns_an_error_into_the_cpu_path() {
        let build = || -> Result<u32, Box<dyn std::error::Error>> { Err("no device".into()) };
        assert_eq!(init_or_none("test", build), None);
    }

    // The regression that matters: cudarc loads the CUDA driver from a lazy static
    // that *panics* when `dlopen` fails, so a backend that panics on init — the
    // ordinary state of any machine without CUDA installed — must still degrade to
    // the CPU kernel rather than unwinding into the caller.
    #[test]
    fn init_or_none_contains_a_panicking_backend() {
        let build = || -> Result<u32, Box<dyn std::error::Error>> {
            panic!("Unable to dynamically load the \"cuda\" shared library")
        };
        assert_eq!(init_or_none("test", build), None);
    }

    // --- Dispatch policy (ADR-0082): the three rules, each asserted directly. ---

    #[test]
    fn small_batches_stay_on_the_cpu() {
        assert!(!worth_dispatching(0));
        assert!(!worth_dispatching(GPU_MIN_BATCH - 1));
        assert!(worth_dispatching(GPU_MIN_BATCH));
        assert!(worth_dispatching(GPU_MIN_BATCH * 16));
    }

    #[test]
    fn quiver_gpu_zero_is_the_escape_hatch() {
        assert!(disabled_by_env(Some("0")));
        // Anything else — unset, or any other value — leaves the GPU enabled, so a
        // stray `QUIVER_GPU=1` cannot accidentally turn the accelerator off.
        assert!(!disabled_by_env(None));
        assert!(!disabled_by_env(Some("1")));
        assert!(!disabled_by_env(Some("")));
        assert!(!disabled_by_env(Some("false")));
    }

    // --- The batched seam agrees with the per-row functions it replaces. ---

    #[test]
    fn batch_ordering_distance_matches_the_per_row_kernel_for_every_metric() {
        let dim = 8usize;
        let n = 64usize;
        let query: Vec<f32> = (0..dim).map(|i| (i as f32).mul_add(0.31, 0.5)).collect();
        let batch: Vec<f32> = (0..n * dim)
            .map(|i| ((i % 37) as f32).mul_add(0.17, -2.0))
            .collect();
        for metric in [Metric::L2, Metric::Dot, Metric::Cosine] {
            let got = batch_ordering_distance(metric, &query, &batch, dim);
            let want: Vec<f32> = batch
                .chunks_exact(dim)
                .map(|row| crate::score::ordering_distance(metric, &query, row))
                .collect();
            assert_eq!(got, want, "batched != per-row for {metric:?}");
        }
    }

    #[test]
    fn batch_assign_is_the_argmin_over_centroids() {
        let dim = 3usize;
        // Three axis centroids; three points each nearest an obvious one, in order.
        let centroids = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let points = [0.0f32, 0.0, 0.9, 0.9, 0.1, 0.0, 0.0, 0.8, 0.1];
        assert_eq!(batch_assign(&points, &centroids, dim), vec![2, 0, 1]);
        assert_eq!(
            batch_assign(&points, &centroids, dim),
            cpu_batch_assign(&points, &centroids, dim)
        );
    }

    #[test]
    fn batch_assign_ties_take_the_first_centroid() {
        // Equidistant from two identical centroids: the argmin must be stable and
        // pick the lower index, or a build's output depends on iteration order.
        let dim = 2usize;
        let centroids = [1.0f32, 0.0, 1.0, 0.0];
        assert_eq!(batch_assign(&[0.0, 0.0], &centroids, dim), vec![0]);
    }

    #[test]
    fn batch_assign_without_centroids_assigns_zero() {
        assert_eq!(batch_assign(&[1.0, 2.0], &[], 2), vec![0]);
        assert!(batch_assign(&[], &[1.0, 2.0], 2).is_empty());
    }

    #[test]
    fn batch_l2_sq_is_correct_per_row() {
        let dim = 4;
        let query = [1.0f32, 0.0, 0.0, 0.0];
        // row0 = query (dist 0); row1 = e2 (dist 2); row2 = 2·e1 (dist 1).
        let batch = [
            1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0,
        ];
        let got = batch_l2_sq(&query, &batch, dim);
        assert_eq!(got, vec![0.0, 2.0, 1.0]);
        assert_eq!(got, cpu_batch_l2_sq(&query, &batch, dim));
    }
}
