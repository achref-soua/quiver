// SPDX-License-Identifier: AGPL-3.0-only
//! Vector quantizers — the memory-frugality core (ADR-0008).
//!
//! Three quantizers share one **approximate-then-exact-re-rank** flow: compress
//! vectors into compact codes, rank candidates by an approximate distance over
//! those codes, then re-rank the shortlist with full-precision distances. The
//! candidate multiplier (`rerank_factor`) is the recall ↔ latency/memory knob.
//!
//! | Quantizer | Code size | Compression | Distance |
//! |---|---|---|---|
//! | [`ScalarQuantizer`] | `dim` bytes | ~4× | dequantize + exact |
//! | [`ProductQuantizer`] | `m` bytes | up to 32× | asymmetric LUT (ADC) |
//! | [`BinaryQuantizer`] | `dim/8` bytes | ~32× | Hamming pre-filter |
//!
//! All distances are reported in a **"smaller is closer"** orientation, matching
//! the index search heaps: squared-L2 for [`Metric::L2`], and *negated* inner
//! product for [`Metric::Dot`] / [`Metric::Cosine`] (cosine vectors are unit
//! normalized first, reducing cosine to inner product).

mod binary;
mod kmeans;
mod product;
mod scalar;

pub use binary::BinaryQuantizer;
pub use product::ProductQuantizer;
pub use scalar::ScalarQuantizer;

use quiver_simd::Metric;

/// A trained quantizer: encodes full-precision vectors into compact codes and
/// builds a per-query [`CodeScorer`] for approximate distances.
pub trait Quantizer: Send + Sync {
    /// Dimensionality of the vectors this quantizer was trained on.
    fn dim(&self) -> usize;
    /// The metric the approximate distances approximate.
    fn metric(&self) -> Metric;
    /// Length in bytes of one encoded code.
    fn code_len(&self) -> usize;
    /// Encode a vector into `code` (which must be [`Quantizer::code_len`] long).
    fn encode_into(&self, vector: &[f32], code: &mut [u8]);
    /// Build a scorer that yields approximate distances from `query` to codes.
    fn scorer<'a>(&'a self, query: &[f32]) -> Box<dyn CodeScorer + 'a>;

    /// Encode a vector into a freshly allocated code.
    fn encode(&self, vector: &[f32]) -> Vec<u8> {
        let mut code = vec![0u8; self.code_len()];
        self.encode_into(vector, &mut code);
        code
    }
}

/// A per-query scorer over encoded codes. Built by [`Quantizer::scorer`], it
/// holds whatever the query needs precomputed (a dequantized query, a PQ lookup
/// table, packed query bits) so scoring each code is cheap.
pub trait CodeScorer {
    /// Approximate distance from the prepared query to `code`, "smaller closer".
    fn distance(&self, code: &[u8]) -> f32;
}

// Unit-normalize a vector; a zero vector is returned unchanged.
fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

// A copy of `v` prepared for `metric`: unit-normalized for cosine (so cosine
// reduces to inner product), otherwise a plain copy.
fn prepare(metric: Metric, v: &[f32]) -> Vec<f32> {
    match metric {
        Metric::Cosine => normalize(v),
        Metric::Dot | Metric::L2 => v.to_vec(),
    }
}

// Whether the metric ranks by inner product (and so negates for "smaller closer").
fn uses_inner_product(metric: Metric) -> bool {
    matches!(metric, Metric::Dot | Metric::Cosine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IndexError;
    use crate::rng::SplitMix64;
    use std::collections::HashSet;

    // Exact full-precision distance in "smaller is closer" orientation — the
    // re-rank oracle the quantizers are measured against.
    fn exact_distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
        match metric {
            Metric::L2 => quiver_simd::l2_sq_f32(a, b),
            Metric::Dot => -quiver_simd::dot_f32(a, b),
            Metric::Cosine => -quiver_simd::cosine_f32(a, b),
        }
    }

    // Clustered points: blobs around random centers, so quantizers have
    // structure to exploit (uniform noise is adversarial for PQ at 32×).
    fn clustered_data(rng: &mut SplitMix64, n: usize, dim: usize, clusters: usize) -> Vec<f32> {
        let centers: Vec<Vec<f32>> = (0..clusters)
            .map(|_| {
                (0..dim)
                    .map(|_| (rng.next_f64() as f32) * 10.0 - 5.0)
                    .collect()
            })
            .collect();
        let mut data = vec![0f32; n * dim];
        for row in 0..n {
            let c = &centers[rng.below(clusters)];
            for d in 0..dim {
                data[row * dim + d] = c[d] + (rng.next_f64() as f32 - 0.5);
            }
        }
        data
    }

    // A test dataset and the metric it is searched under.
    struct Dataset {
        data: Vec<f32>,
        n: usize,
        dim: usize,
        metric: Metric,
    }

    impl Dataset {
        fn clustered(
            rng: &mut SplitMix64,
            n: usize,
            dim: usize,
            clusters: usize,
            metric: Metric,
        ) -> Self {
            Self {
                data: clustered_data(rng, n, dim, clusters),
                n,
                dim,
                metric,
            }
        }

        fn row(&self, i: usize) -> &[f32] {
            &self.data[i * self.dim..(i + 1) * self.dim]
        }

        fn exact_topk(&self, q: &[f32], k: usize) -> Vec<usize> {
            let mut scored: Vec<(f32, usize)> = (0..self.n)
                .map(|i| (exact_distance(self.metric, self.row(i), q), i))
                .collect();
            scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            scored.into_iter().take(k).map(|(_, i)| i).collect()
        }
    }

    fn encode_all<Q: Quantizer>(quant: &Q, ds: &Dataset) -> Vec<u8> {
        let code_len = quant.code_len();
        let mut codes = vec![0u8; ds.n * code_len];
        for i in 0..ds.n {
            quant.encode_into(ds.row(i), &mut codes[i * code_len..(i + 1) * code_len]);
        }
        codes
    }

    // The full approximate→re-rank flow over a flat code store. The re-rank pool
    // is a prefix of the approximate ranking, so a deeper pool can only add true
    // positives — recall is monotonic in `rerank_factor`.
    fn quantized_topk<Q: Quantizer>(
        quant: &Q,
        codes: &[u8],
        ds: &Dataset,
        q: &[f32],
        k: usize,
        rerank_factor: usize,
    ) -> Vec<usize> {
        let code_len = quant.code_len();
        let scorer = quant.scorer(q);
        let mut approx: Vec<(f32, usize)> = (0..ds.n)
            .map(|i| (scorer.distance(&codes[i * code_len..(i + 1) * code_len]), i))
            .collect();
        approx.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let pool = (k * rerank_factor).min(ds.n);
        let mut exact: Vec<(f32, usize)> = approx[..pool]
            .iter()
            .map(|&(_, i)| (exact_distance(ds.metric, ds.row(i), q), i))
            .collect();
        exact.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        exact.into_iter().take(k).map(|(_, i)| i).collect()
    }

    fn recall<Q: Quantizer>(
        quant: &Q,
        ds: &Dataset,
        k: usize,
        rerank_factor: usize,
        queries: usize,
        rng: &mut SplitMix64,
    ) -> f64 {
        let codes = encode_all(quant, ds);
        let mut hits = 0usize;
        for _ in 0..queries {
            let q = clustered_data(rng, 1, ds.dim, 1);
            let truth: HashSet<usize> = ds.exact_topk(&q, k).into_iter().collect();
            let got = quantized_topk(quant, &codes, ds, &q, k, rerank_factor);
            hits += got.iter().filter(|i| truth.contains(i)).count();
        }
        hits as f64 / (queries * k) as f64
    }

    #[test]
    fn scalar_quantizer_recall_with_rerank() {
        let (n, dim) = (2000, 32);
        let mut rng = SplitMix64::new(0x5CA1);
        let ds = Dataset::clustered(&mut rng, n, dim, 16, Metric::L2);
        let sq = ScalarQuantizer::train(&ds.data, n, dim, Metric::L2);
        assert_eq!(sq.code_len(), dim); // 4× smaller than f32
        let r = recall(&sq, &ds, 10, 4, 50, &mut rng);
        assert!(r >= 0.95, "SQ recall@10 with re-rank was {r:.3}");
    }

    #[test]
    fn product_quantizer_recall_with_rerank() {
        let (n, dim, m) = (2000, 32, 8); // 8 bytes/code = 16× smaller than f32
        let mut rng = SplitMix64::new(0xC0DE);
        let ds = Dataset::clustered(&mut rng, n, dim, 16, Metric::L2);
        let pq = ProductQuantizer::train(&ds.data, n, dim, m, Metric::L2, 1).unwrap();
        assert_eq!(pq.code_len(), m);
        let r = recall(&pq, &ds, 10, 8, 50, &mut rng);
        assert!(r >= 0.90, "PQ recall@10 with re-rank was {r:.3}");
    }

    #[test]
    fn rerank_recovers_recall_lost_to_compression() {
        // PQ with a shallow pool vs a deep pool — the knob must not hurt recall.
        let (n, dim, m) = (2000, 32, 8);
        let mut rng = SplitMix64::new(0xBEAD);
        let ds = Dataset::clustered(&mut rng, n, dim, 16, Metric::L2);
        let pq = ProductQuantizer::train(&ds.data, n, dim, m, Metric::L2, 1).unwrap();
        let mut rng_a = SplitMix64::new(0x11);
        let mut rng_b = SplitMix64::new(0x11);
        let shallow = recall(&pq, &ds, 10, 1, 40, &mut rng_a);
        let deep = recall(&pq, &ds, 10, 16, 40, &mut rng_b);
        assert!(
            deep >= shallow,
            "deeper re-rank should not hurt: {deep:.3} vs {shallow:.3}"
        );
    }

    #[test]
    fn binary_quantizer_prefilter_then_rerank() {
        // Binary is the coarsest quantizer and is designed for high-dimensional
        // vectors (ADR-0008): at low dim, sign-pattern collisions crowd the
        // candidate pool. Exercised in its regime — 128-dim with a deep re-rank
        // pool — the Hamming pre-filter plus exact re-rank recovers high recall.
        let (n, dim) = (1500, 128);
        let mut rng = SplitMix64::new(0xB17);
        let ds = Dataset::clustered(&mut rng, n, dim, 12, Metric::L2);
        let bq = BinaryQuantizer::train(&ds.data, n, dim, Metric::L2);
        assert_eq!(bq.code_len(), dim / 8); // 32× smaller
        let r = recall(&bq, &ds, 10, 64, 50, &mut rng);
        assert!(r >= 0.85, "BQ recall@10 with re-rank was {r:.3}");
    }

    #[test]
    fn binary_encode_words_match_simd_hamming() {
        let (n, dim) = (100, 128);
        let mut rng = SplitMix64::new(0xF00);
        let ds = Dataset::clustered(&mut rng, n, dim, 4, Metric::L2);
        let bq = BinaryQuantizer::train(&ds.data, n, dim, Metric::L2);
        let a = bq.encode_words(ds.row(0));
        let b = bq.encode_words(ds.row(1));
        assert_eq!(a.len(), dim / 64);
        // The convenience hamming and the kernel agree, and a vector matches itself.
        assert_eq!(bq.hamming(&a, &b), quiver_simd::hamming_u64(&a, &b));
        assert_eq!(bq.hamming(&a, &a), 0);
    }

    #[test]
    fn cosine_quantization_normalizes() {
        // Two vectors with the same direction but different magnitudes should
        // encode identically under cosine (PQ on the unit sphere).
        let (n, dim, m) = (500, 16, 4);
        let mut rng = SplitMix64::new(0xC051);
        let ds = Dataset::clustered(&mut rng, n, dim, 8, Metric::Cosine);
        let pq = ProductQuantizer::train(&ds.data, n, dim, m, Metric::Cosine, 2).unwrap();
        let v: Vec<f32> = (0..dim).map(|i| i as f32 + 1.0).collect();
        let scaled: Vec<f32> = v.iter().map(|x| x * 7.5).collect();
        assert_eq!(pq.encode(&v), pq.encode(&scaled));
    }

    #[test]
    fn product_training_is_deterministic() {
        let (n, dim, m) = (800, 24, 6);
        let mut rng = SplitMix64::new(0xD17);
        let ds = Dataset::clustered(&mut rng, n, dim, 10, Metric::L2);
        let a = ProductQuantizer::train(&ds.data, n, dim, m, Metric::L2, 99).unwrap();
        let b = ProductQuantizer::train(&ds.data, n, dim, m, Metric::L2, 99).unwrap();
        assert_eq!(a.encode(ds.row(0)), b.encode(ds.row(0)));
    }

    #[test]
    fn product_rejects_indivisible_dim() {
        let data = vec![0f32; 10 * 7];
        assert!(matches!(
            ProductQuantizer::train(&data, 10, 7, 2, Metric::L2, 0),
            Err(IndexError::InvalidConfig(_))
        ));
    }

    #[test]
    fn quantizers_serialize_round_trip() {
        let (n, dim, m) = (300, 16, 4);
        let mut rng = SplitMix64::new(0x5E11);
        let ds = Dataset::clustered(&mut rng, n, dim, 6, Metric::L2);
        let pq = ProductQuantizer::train(&ds.data, n, dim, m, Metric::L2, 3).unwrap();
        let bytes = serde_json::to_vec(&pq).unwrap();
        let back: ProductQuantizer = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(pq.encode(ds.row(0)), back.encode(ds.row(0)));
    }
}
