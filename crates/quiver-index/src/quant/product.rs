// SPDX-License-Identifier: AGPL-3.0-only
//! Product quantization (PQ): the RAM-resident workhorse (ADR-0008).
//!
//! The `dim`-dimensional space is split into `m` subspaces; k-means learns 256
//! centroids per subspace, so a vector compresses to `m` bytes (one centroid
//! index per subspace). Query distance is *asymmetric* (ADC, Jégou et al. 2011):
//! a per-subspace lookup table holds the query's distance to each centroid, and
//! a code's distance is the sum of `m` table reads — no per-candidate vector
//! math. Compression is set by `m` (e.g. 768-dim → m=96 is 32×).

use serde::{Deserialize, Serialize};

use quiver_simd::Metric;

use super::{CodeScorer, Quantizer, prepare, uses_inner_product};
use crate::IndexError;
use crate::kmeans::{kmeans, nearest_centroid};

/// Centroids per subspace (1 byte per code).
const KSUB: usize = 256;
/// Default k-means iterations during codebook training.
const TRAIN_ITERS: usize = 25;

/// A trained product quantizer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductQuantizer {
    dim: usize,
    /// Number of subspaces (= code length in bytes).
    m: usize,
    /// Dimensionality of each subspace, `dim / m`.
    dsub: usize,
    metric: Metric,
    /// Codebooks, laid out `[subspace][centroid][dsub]` in one flat buffer.
    centroids: Vec<f32>,
}

impl ProductQuantizer {
    /// Train `m` subspace codebooks over a flat `n × dim` sample.
    ///
    /// `seed` makes training reproducible. Returns an error if `dim` is not a
    /// multiple of `m`, or `m == 0`.
    ///
    /// # Panics
    /// Panics if `sample.len() != n * dim`.
    pub fn train(
        sample: &[f32],
        n: usize,
        dim: usize,
        m: usize,
        metric: Metric,
        seed: u64,
    ) -> Result<Self, IndexError> {
        assert_eq!(sample.len(), n * dim, "sample must be n*dim");
        if m == 0 || dim == 0 || dim % m != 0 {
            return Err(IndexError::InvalidConfig(
                "PQ requires m > 0 and dim divisible by m",
            ));
        }
        let dsub = dim / m;

        // Normalize the whole sample once (cosine) before splitting subspaces.
        let mut prepared = vec![0f32; n * dim];
        for row in 0..n {
            let p = prepare(metric, &sample[row * dim..(row + 1) * dim]);
            prepared[row * dim..(row + 1) * dim].copy_from_slice(&p);
        }

        let mut centroids = vec![0f32; m * KSUB * dsub];
        let mut sub = vec![0f32; n * dsub];
        for j in 0..m {
            // Gather subspace j of every sample row into a contiguous buffer.
            for row in 0..n {
                let src = &prepared[row * dim + j * dsub..row * dim + (j + 1) * dsub];
                sub[row * dsub..(row + 1) * dsub].copy_from_slice(src);
            }
            // A distinct seed per subspace keeps the runs independent.
            let book = kmeans(
                &sub,
                n,
                dsub,
                KSUB,
                TRAIN_ITERS,
                seed ^ (j as u64).wrapping_mul(0x9E37_79B9),
            );
            centroids[j * KSUB * dsub..(j + 1) * KSUB * dsub].copy_from_slice(&book);
        }

        Ok(Self {
            dim,
            m,
            dsub,
            metric,
            centroids,
        })
    }

    /// The number of subspaces (and the code length in bytes).
    #[must_use]
    pub fn subspaces(&self) -> usize {
        self.m
    }

    fn subspace_centroids(&self, j: usize) -> &[f32] {
        &self.centroids[j * KSUB * self.dsub..(j + 1) * KSUB * self.dsub]
    }
}

impl Quantizer for ProductQuantizer {
    fn dim(&self) -> usize {
        self.dim
    }
    fn metric(&self) -> Metric {
        self.metric
    }
    fn code_len(&self) -> usize {
        self.m
    }

    fn encode_into(&self, vector: &[f32], code: &mut [u8]) {
        assert_eq!(vector.len(), self.dim, "vector dim");
        assert_eq!(code.len(), self.m, "code len");
        let prepared = prepare(self.metric, vector);
        for (j, slot) in code.iter_mut().enumerate() {
            let sub = &prepared[j * self.dsub..(j + 1) * self.dsub];
            *slot = nearest_centroid(sub, self.subspace_centroids(j), self.dsub) as u8;
        }
    }

    fn scorer<'a>(&'a self, query: &[f32]) -> Box<dyn CodeScorer + 'a> {
        assert_eq!(query.len(), self.dim, "query dim");
        let prepared = prepare(self.metric, query);
        let inner_product = uses_inner_product(self.metric);
        // Precompute the asymmetric lookup table: lut[j*KSUB + c] is subspace j's
        // distance contribution from the query to centroid c, already in
        // "smaller is closer" orientation (negated for inner product).
        let mut lut = vec![0f32; self.m * KSUB];
        for j in 0..self.m {
            let q_sub = &prepared[j * self.dsub..(j + 1) * self.dsub];
            let book = self.subspace_centroids(j);
            for (c, centroid) in book.chunks_exact(self.dsub).enumerate() {
                let contribution = if inner_product {
                    -quiver_simd::dot_f32(q_sub, centroid)
                } else {
                    quiver_simd::l2_sq_f32(q_sub, centroid)
                };
                lut[j * KSUB + c] = contribution;
            }
        }
        Box::new(ProductScorer { lut, m: self.m })
    }
}

struct ProductScorer {
    lut: Vec<f32>,
    m: usize,
}

impl CodeScorer for ProductScorer {
    fn distance(&self, code: &[u8]) -> f32 {
        let mut sum = 0f32;
        for (j, &c) in code.iter().take(self.m).enumerate() {
            sum += self.lut[j * KSUB + c as usize];
        }
        sum
    }
}
