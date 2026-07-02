// SPDX-License-Identifier: AGPL-3.0-only
//! Sparse vectors and Reciprocal Rank Fusion for hybrid search (ADR-0043).
//!
//! A [`SparseVector`] is a learned-sparse (SPLADE/BGE-M3) or lexical term-weight
//! vector — parallel `indices` (dimension ids) and `values` (weights). It rides
//! in the point payload under [`SPARSE_KEY`] (no on-disk format change); the
//! embeddable engine builds a derived inverted index from it. [`rrf_fuse`] merges
//! the dense and sparse result lists by rank, the standard hybrid fuser.

use serde::{Deserialize, Serialize};

/// The reserved payload key carrying a point's sparse vector (ADR-0043).
pub const SPARSE_KEY: &str = "__quiver_sparse__";

/// The conventional RRF rank-bias constant (Cormack et al., 2009).
pub const DEFAULT_RRF_K0: f32 = 60.0;

/// A sparse vector: parallel `indices` and `values`. Indices are dimension ids
/// into a (possibly very large) sparse vocabulary; values are their weights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SparseVector {
    /// Dimension ids. After [`SparseVector::normalized`] they are sorted and unique.
    pub indices: Vec<u32>,
    /// Per-index weights, parallel to `indices`.
    pub values: Vec<f32>,
}

impl SparseVector {
    /// Validate shape: equal-length, and no duplicate index after sorting.
    pub fn validate(&self) -> Result<(), String> {
        if self.indices.len() != self.values.len() {
            return Err(format!(
                "sparse vector indices ({}) and values ({}) length mismatch",
                self.indices.len(),
                self.values.len()
            ));
        }
        let mut seen = self.indices.clone();
        seen.sort_unstable();
        if seen.windows(2).any(|w| w[0] == w[1]) {
            return Err("sparse vector has duplicate indices".to_owned());
        }
        Ok(())
    }

    /// Number of non-zero terms.
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    /// Whether the vector has no terms.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Return a copy with indices sorted ascending (values kept parallel). The
    /// canonical form the inverted index and `dot` assume.
    pub fn normalized(&self) -> SparseVector {
        let mut pairs: Vec<(u32, f32)> = self
            .indices
            .iter()
            .copied()
            .zip(self.values.iter().copied())
            .collect();
        pairs.sort_by_key(|&(i, _)| i);
        SparseVector {
            indices: pairs.iter().map(|&(i, _)| i).collect(),
            values: pairs.iter().map(|&(_, v)| v).collect(),
        }
    }

    /// Dot product with another sparse vector. Order-independent (builds a small
    /// lookup over `self`), so callers need not pre-sort.
    pub fn dot(&self, other: &SparseVector) -> f32 {
        use std::collections::HashMap;
        let lhs: HashMap<u32, f32> = self
            .indices
            .iter()
            .copied()
            .zip(self.values.iter().copied())
            .collect();
        let mut sum = 0.0f32;
        for (i, v) in other.indices.iter().zip(other.values.iter()) {
            if let Some(w) = lhs.get(i) {
                sum += w * v;
            }
        }
        sum
    }
}

/// Fuse several ranked id lists by Reciprocal Rank Fusion and return the top
/// `top_k` ids with their fused scores, highest first.
///
/// For each list, a document at 0-based `rank` contributes `1 / (k0 + rank + 1)`;
/// the contributions sum across lists. RRF is rank-based, so the (incomparable)
/// dense-distance and sparse-dot scales need no normalisation — the property that
/// makes it the standard, robust hybrid fuser. Ties break by id for determinism.
pub fn rrf_fuse(rankings: &[Vec<String>], k0: f32, top_k: usize) -> Vec<(String, f32)> {
    use std::collections::HashMap;
    // The rank-bias constant is defined for k0 >= 0. Floor it so the denominator
    // `k0 + rank + 1` is always >= 1: a caller-supplied negative k0 (e.g. -1.0)
    // would otherwise divide by zero (+inf) at rank 0 or, fractional, invert the
    // fusion by giving early ranks negative weight.
    let k0 = k0.max(0.0);
    let mut scores: HashMap<String, f32> = HashMap::new();
    for ranking in rankings {
        for (rank, id) in ranking.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (k0 + rank as f32 + 1.0);
        }
    }
    let mut fused: Vec<(String, f32)> = scores.into_iter().collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    fused.truncate(top_k);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_catches_length_mismatch_and_dupes() {
        assert!(
            SparseVector {
                indices: vec![1, 2],
                values: vec![1.0]
            }
            .validate()
            .is_err()
        );
        assert!(
            SparseVector {
                indices: vec![1, 1],
                values: vec![1.0, 2.0]
            }
            .validate()
            .is_err()
        );
        assert!(
            SparseVector {
                indices: vec![3, 1, 2],
                values: vec![1.0, 2.0, 3.0]
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn dot_is_order_independent_and_uses_shared_dims() {
        let a = SparseVector {
            indices: vec![1, 5, 9],
            values: vec![1.0, 2.0, 3.0],
        };
        let b = SparseVector {
            indices: vec![9, 1, 7],
            values: vec![10.0, 4.0, 1.0],
        };
        // shared dims: 1 (1*4) + 9 (3*10) = 34
        assert_eq!(a.dot(&b), 34.0);
        assert_eq!(a.dot(&b), b.dot(&a));
    }

    #[test]
    fn normalized_sorts_indices_keeping_values_parallel() {
        let n = SparseVector {
            indices: vec![5, 1, 3],
            values: vec![50.0, 10.0, 30.0],
        }
        .normalized();
        assert_eq!(n.indices, vec![1, 3, 5]);
        assert_eq!(n.values, vec![10.0, 30.0, 50.0]);
    }

    #[test]
    fn rrf_rewards_agreement_across_lists() {
        let dense = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let sparse = vec!["b".to_owned(), "a".to_owned(), "d".to_owned()];
        let fused = rrf_fuse(&[dense, sparse], DEFAULT_RRF_K0, 10);
        // "a" (ranks 0,1) and "b" (ranks 1,0) appear in both → top two; both equal.
        let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(&ids[..2], &["a", "b"]);
        // a: 1/61 + 1/62 ; b: 1/62 + 1/61 → equal, so id order breaks the tie.
        assert!((fused[0].1 - fused[1].1).abs() < 1e-9);
        // c and d (single-list) score below.
        assert!(fused[2].1 < fused[0].1);
    }

    #[test]
    fn rrf_truncates_to_top_k() {
        let r = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(rrf_fuse(&[r], DEFAULT_RRF_K0, 2).len(), 2);
    }

    #[test]
    fn rrf_tolerates_non_positive_k0() {
        // A negative k0 must not divide by zero (+inf) or invert the ranking:
        // the first-ranked doc must still score highest and all scores finite.
        let r = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let fused = rrf_fuse(&[r], -1.0, 3);
        assert_eq!(fused.len(), 3);
        assert!(fused.iter().all(|(_, s)| s.is_finite()));
        assert_eq!(fused[0].0, "a", "first-ranked doc should score highest");
        assert!(fused[0].1 >= fused[1].1 && fused[1].1 >= fused[2].1);
    }
}
