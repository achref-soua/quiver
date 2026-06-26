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

/// Squared-L2 distance from `query` to each `dim`-length row of the contiguous
/// `batch` (so `batch.len() == n * dim`), returning the `n` distances in row order.
/// Uses the GPU when the `cuda` feature is enabled and a device is available, else
/// the CPU SIMD kernel.
#[must_use]
pub fn batch_l2_sq(query: &[f32], batch: &[f32], dim: usize) -> Vec<f32> {
    debug_assert!(dim > 0 && batch.len().is_multiple_of(dim) && query.len() == dim);
    #[cfg(feature = "cuda")]
    {
        if let Some(ctx) = cuda::context()
            && let Ok(out) = ctx.batch_l2_sq(query, batch, dim)
        {
            return out;
        }
    }
    cpu_batch_l2_sq(query, batch, dim)
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

    // One NVRTC-compiled batch-L2 kernel: out[r] = Σ_j (a[r*d+j] − q[j])².
    const KERNEL: &str = r#"
extern "C" __global__ void l2sq(const float* q, const float* a, float* out, int n, int d) {
  int r = blockIdx.x * blockDim.x + threadIdx.x;
  if (r >= n) return;
  float s = 0.0f;
  for (int j = 0; j < d; j++) { float diff = a[r * d + j] - q[j]; s += diff * diff; }
  out[r] = s;
}"#;

    pub(super) struct Context {
        dev: Arc<CudaDevice>,
    }

    impl Context {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let dev = CudaDevice::new(0)?;
            let ptx = compile_ptx(KERNEL)?;
            dev.load_ptx(ptx, "quiver_l2", &["l2sq"])?;
            Ok(Self { dev })
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

    // The process-wide GPU context, built once. `None` if no device is present or
    // init fails — callers fall back to the CPU kernel.
    pub(super) fn context() -> Option<&'static Context> {
        static CTX: OnceLock<Option<Context>> = OnceLock::new();
        CTX.get_or_init(|| match Context::new() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("cuda: GPU acceleration unavailable ({e}); using the CPU kernel");
                None
            }
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
    use super::{batch_l2_sq, cpu_batch_l2_sq};

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
