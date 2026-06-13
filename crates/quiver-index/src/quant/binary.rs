// SPDX-License-Identifier: AGPL-3.0-only
//! Binary quantization (BQ): 1 bit per dimension, ~32× smaller (ADR-0008).
//!
//! Each dimension becomes a single bit (is the value above its learned
//! threshold?), packed into `u64` words. Candidate ranking is the Hamming
//! distance between packed codes — the SIMD `hamming_u64` kernel — used as a
//! fast **pre-filter** before an exact full-precision re-rank. Thresholding by
//! the per-dimension training mean (rather than a fixed zero) keeps the bits
//! balanced even on non-negative data such as SIFT.

use serde::{Deserialize, Serialize};

use quiver_simd::Metric;

use super::{CodeScorer, Quantizer, prepare};

/// Bits packed per word.
const BITS_PER_WORD: usize = 64;

/// A trained binary quantizer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryQuantizer {
    dim: usize,
    metric: Metric,
    /// Number of `u64` words per code, `ceil(dim / 64)`.
    words: usize,
    /// Per-dimension binarization threshold (the training mean).
    thresholds: Vec<f32>,
}

impl BinaryQuantizer {
    /// Train over a flat `n × dim` sample, learning each dimension's threshold.
    ///
    /// # Panics
    /// Panics if `sample.len() != n * dim` or `dim == 0`.
    #[must_use]
    pub fn train(sample: &[f32], n: usize, dim: usize, metric: Metric) -> Self {
        assert!(dim > 0, "dim > 0");
        assert_eq!(sample.len(), n * dim, "sample must be n*dim");
        let mut sums = vec![0f64; dim];
        for row in 0..n {
            let prepared = prepare(metric, &sample[row * dim..(row + 1) * dim]);
            for (d, &x) in prepared.iter().enumerate() {
                sums[d] += f64::from(x);
            }
        }
        let inv = if n > 0 { 1.0 / n as f64 } else { 0.0 };
        let thresholds = sums.iter().map(|&s| (s * inv) as f32).collect();
        let words = dim.div_ceil(BITS_PER_WORD);
        Self {
            dim,
            metric,
            words,
            thresholds,
        }
    }

    /// Number of `u64` words per code.
    #[must_use]
    pub fn words(&self) -> usize {
        self.words
    }

    /// Encode a vector directly into packed `u64` words (the form the SIMD
    /// Hamming kernel consumes for bulk scoring).
    #[must_use]
    pub fn encode_words(&self, vector: &[f32]) -> Vec<u64> {
        assert_eq!(vector.len(), self.dim, "vector dim");
        let prepared = prepare(self.metric, vector);
        let mut out = vec![0u64; self.words];
        for (d, &x) in prepared.iter().enumerate() {
            if x > self.thresholds[d] {
                out[d / BITS_PER_WORD] |= 1u64 << (d % BITS_PER_WORD);
            }
        }
        out
    }

    /// Hamming distance between two packed codes, via the SIMD kernel.
    #[must_use]
    pub fn hamming(&self, a: &[u64], b: &[u64]) -> u32 {
        quiver_simd::hamming_u64(a, b)
    }
}

impl Quantizer for BinaryQuantizer {
    fn dim(&self) -> usize {
        self.dim
    }
    fn metric(&self) -> Metric {
        self.metric
    }
    fn code_len(&self) -> usize {
        self.words * (BITS_PER_WORD / 8)
    }

    fn encode_into(&self, vector: &[f32], code: &mut [u8]) {
        assert_eq!(code.len(), self.code_len(), "code len");
        let words = self.encode_words(vector);
        for (w, word) in words.iter().enumerate() {
            code[w * 8..(w + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
    }

    fn scorer<'a>(&'a self, query: &[f32]) -> Box<dyn CodeScorer + 'a> {
        Box::new(BinaryScorer {
            query: self.encode_words(query),
        })
    }
}

struct BinaryScorer {
    query: Vec<u64>,
}

impl CodeScorer for BinaryScorer {
    fn distance(&self, code: &[u8]) -> f32 {
        // Reconstruct the packed words from the byte code (codes are 1-aligned
        // `Vec<u8>`, so we read each word with `from_le_bytes` rather than
        // reinterpreting the slice) and accumulate the hardware popcount.
        let mut h = 0u32;
        for (w, &qw) in self.query.iter().enumerate() {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&code[w * 8..(w + 1) * 8]);
            h += (u64::from_le_bytes(bytes) ^ qw).count_ones();
        }
        h as f32
    }
}
