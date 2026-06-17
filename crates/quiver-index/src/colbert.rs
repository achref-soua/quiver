// SPDX-License-Identifier: AGPL-3.0-only
//! ColBERTv2 / PLAID token-pool index for late interaction (ADR-0034; Santhanam
//! et al., NAACL 2022 / CIKM 2022).
//!
//! A late-interaction collection stores a large pool of low-dimensional token
//! vectors (ADR-0028). This index makes that pool **memory-frugal** and prunes
//! candidate generation, the way ColBERTv2 + PLAID do:
//!
//! - **Residual compression.** Coarse centroids are trained over the token pool
//!   (the shared [`crate::kmeans`]); each token is assigned to its nearest
//!   centroid and only its *residual* (token − centroid) is product-quantized.
//!   RAM holds the centroids plus, per token, a centroid id and a short PQ code —
//!   far smaller than the full vector — and an approximate token is reconstructed
//!   as `centroid + decoded residual`. The exact token vectors stay in the
//!   encrypted store for the MaxSim re-rank (the ADR-0019 pattern, applied to
//!   tokens).
//! - **PLAID centroid pruning.** A query token scores the centroids first and
//!   only the inverted lists under the closest `n_probe` centroids are expanded,
//!   so most of the pool is never touched.
//!
//! The index is a **candidate generator**: it returns the token ids nearest a
//! query token (the embeddable database maps those to documents and re-ranks the
//! survivors exactly). It is in-memory and derived — rebuilt from the store on
//! open like every other index — so it never joins the crash path. The collection
//! metric is a similarity (cosine or dot), as a `multivector` collection requires.

use std::collections::HashSet;

use quiver_simd::Metric;

use crate::kmeans::{kmeans, nearest_centroid};
use crate::{IndexError, Neighbor, ProductQuantizer, Quantizer, ordering_distance, report_metric};

// Lloyd iterations for the coarse centroid training (matches the PQ trainer).
const TRAIN_ITERS: usize = 25;

/// Build / search parameters for a [`ColbertIndex`].
#[derive(Debug, Clone, Copy)]
pub struct ColbertConfig {
    /// Number of coarse centroids over the token pool. Typical ≈ √(tokens).
    pub n_centroids: usize,
    /// Centroids probed per query token (PLAID pruning); higher trades latency for
    /// recall. Clamped to `n_centroids`.
    pub n_probe: usize,
    /// PQ subspaces for the residual code (must divide the dimensionality); the
    /// code length in bytes.
    pub pq_subspaces: usize,
    /// Seed for reproducible centroid and codebook training.
    pub seed: u64,
}

impl Default for ColbertConfig {
    fn default() -> Self {
        Self {
            n_centroids: 256,
            n_probe: 16,
            pq_subspaces: 8,
            seed: 0x0C01_BE27,
        }
    }
}

// Unit-normalize for cosine; pass through otherwise (matches the other indexes).
fn prepare(metric: Metric, v: &[f32]) -> Vec<f32> {
    match metric {
        Metric::Cosine => {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                v.iter().map(|x| x / norm).collect()
            } else {
                v.to_vec()
            }
        }
        Metric::L2 | Metric::Dot => v.to_vec(),
    }
}

/// A ColBERTv2/PLAID compressed token-pool index.
pub struct ColbertIndex {
    dim: usize,
    metric: Metric,
    n_probe: usize,
    n_centroids: usize,
    // Coarse centroids in prepared space, flat `n_centroids × dim`.
    centroids: Vec<f32>,
    // Residual product quantizer (trained under L2 — residuals are not unit
    // vectors, so the collection metric is applied only at scoring time).
    pq: ProductQuantizer,
    code_len: usize,
    // Per token, in insertion order.
    ids: Vec<u64>,
    token_centroid: Vec<u32>,
    token_code: Vec<u8>,
    // Inverted lists: centroid id → token indices (into `ids` / `token_*`).
    lists: Vec<Vec<u32>>,
    // Soft-deleted token ids, filtered from results (ADR-0026 style).
    deleted: HashSet<u64>,
}

impl ColbertIndex {
    /// Build the index over `ids` and their `vectors` (flat `n × dim`).
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] if `vectors.len()` is not
    /// `n × dim`, or [`IndexError::InvalidConfig`] if `pq_subspaces` does not
    /// divide `dim`.
    pub fn build(
        ids: &[u64],
        vectors: &[f32],
        dim: usize,
        metric: Metric,
        config: ColbertConfig,
    ) -> Result<Self, IndexError> {
        let n = ids.len();
        if vectors.len() != n * dim {
            return Err(IndexError::DimensionMismatch {
                expected: n * dim,
                got: vectors.len(),
            });
        }
        let n_centroids = config.n_centroids.max(1);
        let m = config.pq_subspaces.max(1);

        // Prepare (normalize for cosine) the whole pool once.
        let mut prepared = vec![0f32; n * dim];
        for i in 0..n {
            let p = prepare(metric, &vectors[i * dim..(i + 1) * dim]);
            prepared[i * dim..(i + 1) * dim].copy_from_slice(&p);
        }

        let centroids = if n == 0 {
            vec![0f32; n_centroids * dim]
        } else {
            kmeans(&prepared, n, dim, n_centroids, TRAIN_ITERS, config.seed)
        };

        // Assign each token to its nearest centroid and collect its residual.
        let mut token_centroid = vec![0u32; n];
        let mut residuals = vec![0f32; n * dim];
        for i in 0..n {
            let p = &prepared[i * dim..(i + 1) * dim];
            let cid = nearest_centroid(p, &centroids, dim);
            token_centroid[i] = cid as u32;
            let c = &centroids[cid * dim..(cid + 1) * dim];
            for d in 0..dim {
                residuals[i * dim + d] = p[d] - c[d];
            }
        }

        // Product-quantize the residuals under L2 (faithful reconstruction).
        let pq = ProductQuantizer::train(&residuals, n, dim, m, Metric::L2, config.seed ^ 0x5152)?;
        let code_len = pq.code_len();
        let mut token_code = vec![0u8; n * code_len];
        for i in 0..n {
            pq.encode_into(
                &residuals[i * dim..(i + 1) * dim],
                &mut token_code[i * code_len..(i + 1) * code_len],
            );
        }

        let mut lists = vec![Vec::new(); n_centroids];
        for (i, &cid) in token_centroid.iter().enumerate() {
            lists[cid as usize].push(i as u32);
        }

        Ok(Self {
            dim,
            metric,
            n_probe: config.n_probe.max(1),
            n_centroids,
            centroids,
            pq,
            code_len,
            ids: ids.to_vec(),
            token_centroid,
            token_code,
            lists,
            deleted: HashSet::new(),
        })
    }

    /// Add one token incrementally: assign it to the nearest existing centroid and
    /// store its quantized residual (the centroids are fixed until a rebuild, the
    /// IVF-incremental stance of ADR-0023).
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] if `vector.len() != dim`.
    pub fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError> {
        if vector.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: vector.len(),
            });
        }
        let p = prepare(self.metric, vector);
        let cid = nearest_centroid(&p, &self.centroids, self.dim);
        let c = &self.centroids[cid * self.dim..(cid + 1) * self.dim];
        let residual: Vec<f32> = p.iter().zip(c).map(|(x, y)| x - y).collect();
        let token = self.ids.len() as u32;
        let mut code = vec![0u8; self.code_len];
        self.pq.encode_into(&residual, &mut code);
        self.ids.push(id);
        self.token_centroid.push(cid as u32);
        self.token_code.extend_from_slice(&code);
        self.lists[cid].push(token);
        Ok(())
    }

    /// Soft-delete a token id so it is never returned (idempotent; returns whether
    /// it was newly tombstoned).
    pub fn mark_deleted(&mut self, id: u64) -> bool {
        self.deleted.insert(id)
    }

    /// The fraction of tokens that are soft-deleted, in `[0, 1]`.
    #[must_use]
    pub fn deleted_fraction(&self) -> f64 {
        if self.ids.is_empty() {
            0.0
        } else {
            self.deleted.len() as f64 / self.ids.len() as f64
        }
    }

    /// Live token count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len() - self.deleted.len()
    }

    /// Whether the index holds no live tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The metric this index searches under.
    #[must_use]
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// Dimensionality of the indexed vectors.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    // Reconstruct an approximate token vector: its centroid plus the decoded
    // residual.
    fn reconstruct(&self, token: u32) -> Vec<f32> {
        let cid = self.token_centroid[token as usize] as usize;
        let c = &self.centroids[cid * self.dim..(cid + 1) * self.dim];
        let code =
            &self.token_code[token as usize * self.code_len..(token as usize + 1) * self.code_len];
        let residual = self.pq.reconstruct(code);
        c.iter().zip(residual).map(|(x, r)| x + r).collect()
    }

    /// Return the `k` token ids nearest `query`, closest first, using PLAID
    /// centroid pruning. `ef` overrides the probe breadth (0 ⇒ the configured
    /// `n_probe`); higher widens the search for recall.
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] if `query.len() != dim`.
    pub fn search(&self, query: &[f32], k: usize, ef: usize) -> Result<Vec<Neighbor>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }
        if k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        let q = prepare(self.metric, query);
        // Score centroids, then probe the closest ones (PLAID pruning).
        let mut centroid_scored: Vec<(f32, usize)> = (0..self.n_centroids)
            .map(|c| {
                (
                    ordering_distance(
                        self.metric,
                        &q,
                        &self.centroids[c * self.dim..(c + 1) * self.dim],
                    ),
                    c,
                )
            })
            .collect();
        centroid_scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let probe = if ef == 0 { self.n_probe } else { ef }.min(self.n_centroids);

        // Score the tokens in the probed lists by their reconstructed similarity.
        let mut scored: Vec<(f32, u32)> = Vec::new();
        for &(_, c) in centroid_scored.iter().take(probe) {
            for &token in &self.lists[c] {
                if self.deleted.contains(&self.ids[token as usize]) {
                    continue;
                }
                let recon = self.reconstruct(token);
                scored.push((ordering_distance(self.metric, &q, &recon), token));
            }
        }
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.truncate(k);
        Ok(scored
            .into_iter()
            .map(|(ord, token)| Neighbor {
                id: self.ids[token as usize],
                distance: report_metric(self.metric, ord),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;
    use std::collections::HashSet as Set;

    fn rand_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_f64() as f32) * 2.0 - 1.0)
            .collect()
    }

    fn brute_force_nearest(
        data: &[Vec<f32>],
        live: &Set<u64>,
        q: &[f32],
        k: usize,
        metric: Metric,
    ) -> Set<u64> {
        let mut scored: Vec<(f32, u64)> = data
            .iter()
            .enumerate()
            .filter(|(i, _)| live.contains(&(*i as u64)))
            .map(|(i, v)| (ordering_distance(metric, q, v), i as u64))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    // Mildly clustered tokens — the structure real ColBERT embeddings have and
    // that residual compression + centroid pruning exploit (uniform-random tokens
    // are a degenerate worst case for any quantizer).
    fn clustered(rng: &mut SplitMix64, n: usize, dim: usize, n_clusters: usize) -> Vec<Vec<f32>> {
        let centers: Vec<Vec<f32>> = (0..n_clusters).map(|_| rand_vec(rng, dim)).collect();
        (0..n)
            .map(|i| {
                let c = &centers[i % n_clusters];
                c.iter()
                    .map(|&x| x + (rng.next_f64() as f32 - 0.5) * 0.4)
                    .collect()
            })
            .collect()
    }

    // Candidate-generation recall: this index over-fetches a candidate set that the
    // embeddable database re-ranks exactly (ADR-0028/0034), so the meaningful metric
    // is how much of the true top-`k` lands in a generous fetch — not exact top-k.
    fn recall_at_k(metric: Metric) -> f64 {
        let (dim, n, queries, k) = (32, 2000, 50, 10);
        let mut rng = SplitMix64::new(0xC0_1B ^ metric as u64);
        let data = clustered(&mut rng, n, dim, 40);
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let cfg = ColbertConfig {
            n_centroids: 64,
            n_probe: 32,
            pq_subspaces: 8,
            seed: 7,
        };
        let idx = ColbertIndex::build(&ids, &flat, dim, metric, cfg).unwrap();
        let live: Set<u64> = (0..n as u64).collect();

        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(&mut rng, dim);
            let truth = brute_force_nearest(&data, &live, &q, k, metric);
            // Over-fetch 4× (the embeddable candidate factor), as the re-rank does.
            let got = idx.search(&q, k * 4, 0).unwrap();
            let got_ids: Set<u64> = got.iter().map(|n| n.id).collect();
            hits += truth.iter().filter(|t| got_ids.contains(t)).count();
        }
        hits as f64 / (queries * k) as f64
    }

    #[test]
    fn recall_cosine_meets_threshold() {
        let r = recall_at_k(Metric::Cosine);
        // Lossy residual codes + centroid pruning; the embeddable re-rank recovers
        // exact order, so high candidate-set coverage of the true top-k is the goal.
        assert!(r >= 0.90, "ColBERT cosine candidate recall was {r:.3}");
    }

    #[test]
    fn recall_dot_meets_threshold() {
        let r = recall_at_k(Metric::Dot);
        assert!(r >= 0.85, "ColBERT dot candidate recall was {r:.3}");
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let ids: Vec<u64> = (0..4).collect();
        // vectors length 4×3 ≠ n×dim (4×8): a dimension mismatch, caught before training.
        assert!(matches!(
            ColbertIndex::build(
                &ids,
                &[0.0; 4 * 3],
                8,
                Metric::Cosine,
                ColbertConfig::default()
            ),
            Err(IndexError::DimensionMismatch { .. })
        ));
        // dim 8 is divisible by the default 8 PQ subspaces.
        let mut idx =
            ColbertIndex::build(&[], &[], 8, Metric::Cosine, ColbertConfig::default()).unwrap();
        assert!(matches!(
            idx.insert(1, &[0.0; 3]),
            Err(IndexError::DimensionMismatch { .. })
        ));
        assert!(matches!(
            idx.search(&[0.0; 5], 1, 0),
            Err(IndexError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn rejects_pq_subspaces_not_dividing_dim() {
        let ids: Vec<u64> = (0..8).collect();
        let cfg = ColbertConfig {
            pq_subspaces: 3,
            ..ColbertConfig::default()
        };
        assert!(matches!(
            ColbertIndex::build(&ids, &[0.0; 8 * 8], 8, Metric::Cosine, cfg),
            Err(IndexError::InvalidConfig(_))
        ));
    }

    #[test]
    fn empty_index_returns_nothing() {
        let idx =
            ColbertIndex::build(&[], &[], 8, Metric::Cosine, ColbertConfig::default()).unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.search(&[0.0; 8], 5, 0).unwrap(), Vec::new());
    }

    #[test]
    fn incremental_insert_then_finds_token() {
        let mut rng = SplitMix64::new(13);
        let n = 400;
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, 16)).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let cfg = ColbertConfig {
            n_centroids: 32,
            n_probe: 16,
            pq_subspaces: 4,
            seed: 1,
        };
        // Build over the first half, stream the rest in.
        let half = n / 2;
        let mut idx =
            ColbertIndex::build(&ids[..half], &flat[..half * 16], 16, Metric::Cosine, cfg).unwrap();
        for (i, v) in data.iter().enumerate().skip(half) {
            idx.insert(i as u64, v).unwrap();
        }
        assert_eq!(idx.len(), n);
        // A streamed-in token is retrievable as a near neighbour of itself.
        let got = idx.search(&data[300], 5, 32).unwrap();
        assert!(
            got.iter().any(|nbr| nbr.id == 300),
            "streamed token missing"
        );
    }

    #[test]
    fn deleted_tokens_are_not_returned() {
        let mut rng = SplitMix64::new(0xDEAD);
        let n = 500;
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, 16)).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let cfg = ColbertConfig {
            n_centroids: 32,
            n_probe: 32,
            pq_subspaces: 4,
            seed: 2,
        };
        let mut idx = ColbertIndex::build(&ids, &flat, 16, Metric::Cosine, cfg).unwrap();
        assert!(idx.mark_deleted(7));
        assert!(!idx.mark_deleted(7), "re-deleting is idempotent");
        assert_eq!(idx.len(), n - 1);
        // Querying with token 7's own vector must no longer return it.
        let got = idx.search(&data[7], 10, 32).unwrap();
        assert!(got.iter().all(|nbr| nbr.id != 7), "deleted token returned");
    }
}
