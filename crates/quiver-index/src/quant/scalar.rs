// SPDX-License-Identifier: AGPL-3.0-only
//! Scalar quantization (SQ): per-dimension `f32 → u8`, ~4× smaller.
//!
//! Each dimension is mapped onto `[0, 255]` by its training min/max. Distances
//! are computed *asymmetrically* — the query stays full precision while database
//! codes are dequantized on the fly — which preserves more recall than a
//! symmetric code-to-code comparison (ADR-0008).

use serde::{Deserialize, Serialize};

use quiver_simd::Metric;

use super::{CodeScorer, Quantizer, prepare, uses_inner_product};

/// A trained per-dimension scalar quantizer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalarQuantizer {
    dim: usize,
    metric: Metric,
    /// Per-dimension lower bound.
    min: Vec<f32>,
    /// Per-dimension dequantization step, `(max - min) / 255`.
    step: Vec<f32>,
}

impl ScalarQuantizer {
    /// Train over a flat `n × dim` sample, learning each dimension's range.
    ///
    /// # Panics
    /// Panics if `sample.len() != n * dim` or `dim == 0`.
    #[must_use]
    pub fn train(sample: &[f32], n: usize, dim: usize, metric: Metric) -> Self {
        assert!(dim > 0, "dim > 0");
        assert_eq!(sample.len(), n * dim, "sample must be n*dim");
        let mut min = vec![f32::INFINITY; dim];
        let mut max = vec![f32::NEG_INFINITY; dim];
        for row in 0..n {
            let prepared = prepare(metric, &sample[row * dim..(row + 1) * dim]);
            for (d, &x) in prepared.iter().enumerate() {
                min[d] = min[d].min(x);
                max[d] = max[d].max(x);
            }
        }
        // A dimension with no spread (or no data) gets a zero step → all codes 0.
        let step: Vec<f32> = min
            .iter()
            .zip(&max)
            .map(|(&lo, &hi)| {
                let span = hi - lo;
                if span > 0.0 { span / 255.0 } else { 0.0 }
            })
            .collect();
        let min = min
            .iter()
            .map(|&m| if m.is_finite() { m } else { 0.0 })
            .collect();
        Self {
            dim,
            metric,
            min,
            step,
        }
    }

    fn quantize_dim(&self, d: usize, x: f32) -> u8 {
        if self.step[d] <= 0.0 {
            return 0;
        }
        let q = ((x - self.min[d]) / self.step[d]).round();
        q.clamp(0.0, 255.0) as u8
    }

    fn dequantize_dim(&self, d: usize, code: u8) -> f32 {
        self.min[d] + f32::from(code) * self.step[d]
    }
}

impl Quantizer for ScalarQuantizer {
    fn dim(&self) -> usize {
        self.dim
    }
    fn metric(&self) -> Metric {
        self.metric
    }
    fn code_len(&self) -> usize {
        self.dim
    }

    fn encode_into(&self, vector: &[f32], code: &mut [u8]) {
        assert_eq!(vector.len(), self.dim, "vector dim");
        assert_eq!(code.len(), self.dim, "code len");
        let prepared = prepare(self.metric, vector);
        for (d, slot) in code.iter_mut().enumerate() {
            *slot = self.quantize_dim(d, prepared[d]);
        }
    }

    fn scorer<'a>(&'a self, query: &[f32]) -> Box<dyn CodeScorer + 'a> {
        assert_eq!(query.len(), self.dim, "query dim");
        Box::new(ScalarScorer {
            quant: self,
            query: prepare(self.metric, query),
            inner_product: uses_inner_product(self.metric),
        })
    }
}

struct ScalarScorer<'a> {
    quant: &'a ScalarQuantizer,
    query: Vec<f32>,
    inner_product: bool,
}

impl CodeScorer for ScalarScorer<'_> {
    fn distance(&self, code: &[u8]) -> f32 {
        if self.inner_product {
            // Negated inner product → "smaller is closer".
            let mut ip = 0f32;
            for (d, &c) in code.iter().enumerate() {
                ip += self.query[d] * self.quant.dequantize_dim(d, c);
            }
            -ip
        } else {
            let mut l2 = 0f32;
            for (d, &c) in code.iter().enumerate() {
                let diff = self.query[d] - self.quant.dequantize_dim(d, c);
                l2 += diff * diff;
            }
            l2
        }
    }
}
