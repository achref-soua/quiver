// SPDX-License-Identifier: AGPL-3.0-only
//! IVF — inverted-file index with coarse Voronoi partitioning (ADR-0007).
//!
//! A coarse k-means quantizer splits the space into `nlist` cells; each vector
//! lands in the posting list of its nearest centroid. A query probes the
//! `nprobe` nearest cells and scans only those posting lists — trading recall
//! for speed via `nprobe`. IVF builds fast and has a predictable RAM profile, so
//! it is the alternative / fallback to the Vamana disk graph (ADR-0007).
//!
//! Two storage modes set the memory ↔ recall point:
//!
//! - **Flat** (no quantization): full vectors held in RAM; candidates scored by
//!   exact distance. Highest recall.
//! - **PQ** (`quantization = Some(m)`): only the centroids and `m`-byte PQ codes
//!   are resident — the memory-frugal mode — and candidates are scored by
//!   asymmetric PQ distance (ADR-0008). Lower RAM, PQ-approximate recall.
//!
//! Like [`Vamana`](crate::Vamana), IVF is **batch-built** ([`Ivf::build`]) and
//! supports [`Metric::L2`] and [`Metric::Cosine`] (cosine on the unit sphere);
//! inner product uses HNSW.

use quiver_simd::Metric;

use crate::kmeans::kmeans;
use crate::quant::Quantizer;
use crate::{IndexError, Neighbor, ProductQuantizer};

/// Build parameters for [`Ivf`].
#[derive(Debug, Clone, Copy)]
pub struct IvfConfig {
    /// Number of Voronoi cells / coarse centroids (`nlist`).
    pub nlist: usize,
    /// Lloyd iterations for the coarse quantizer.
    pub kmeans_iters: usize,
    /// `Some(m)` enables PQ-compressed (memory-frugal) storage with `m`
    /// subspaces; `None` keeps full vectors (exact, IVFFlat).
    pub quantization: Option<usize>,
    /// Seed for reproducible builds.
    pub seed: u64,
}

impl Default for IvfConfig {
    fn default() -> Self {
        Self {
            nlist: 64,
            kmeans_iters: 20,
            quantization: None,
            seed: 0x1F1F_2E2E_3D3D_4C4C,
        }
    }
}

// Resident per-vector data: either full vectors (exact) or PQ codes (frugal).
enum Storage {
    Flat {
        vectors: Vec<f32>,
    },
    Pq {
        pq: ProductQuantizer,
        codes: Vec<u8>,
    },
}

/// An in-memory IVF index.
pub struct Ivf {
    dim: usize,
    metric: Metric,
    centroids: Vec<f32>,
    // Posting list per cell: the internal node ids assigned to that centroid.
    postings: Vec<Vec<u32>>,
    ids: Vec<u64>,
    storage: Storage,
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

fn rank_distance(metric: Metric, q: &[f32], v: &[f32]) -> f32 {
    match metric {
        Metric::L2 => quiver_simd::l2_sq_f32(q, v),
        Metric::Cosine => -quiver_simd::cosine_f32(q, v),
        Metric::Dot => -quiver_simd::dot_f32(q, v),
    }
}

fn report_distance(metric: Metric, q: &[f32], v: &[f32]) -> f32 {
    match metric {
        Metric::L2 => quiver_simd::l2_sq_f32(q, v),
        Metric::Cosine => quiver_simd::cosine_f32(q, v),
        Metric::Dot => quiver_simd::dot_f32(q, v),
    }
}

impl Ivf {
    /// Build an IVF index over `ids` and their `vectors` (flat `n × dim`).
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidConfig`] for [`Metric::Dot`] or a PQ
    /// configuration that does not divide `dim`, or [`IndexError::DimensionMismatch`]
    /// if `vectors.len() != n × dim`.
    pub fn build(
        ids: &[u64],
        vectors: &[f32],
        dim: usize,
        metric: Metric,
        config: IvfConfig,
    ) -> Result<Self, IndexError> {
        if metric == Metric::Dot {
            return Err(IndexError::InvalidConfig(
                "IVF supports L2 and Cosine; use HNSW for inner product",
            ));
        }
        let n = ids.len();
        if vectors.len() != n * dim {
            return Err(IndexError::DimensionMismatch {
                expected: n * dim,
                got: vectors.len(),
            });
        }
        let nlist = config.nlist.max(1).min(n.max(1));

        // Prepare (normalize for cosine) into a flat arena.
        let mut prepared = vec![0f32; n * dim];
        for i in 0..n {
            let p = prepare(metric, &vectors[i * dim..(i + 1) * dim]);
            prepared[i * dim..(i + 1) * dim].copy_from_slice(&p);
        }

        // Coarse quantizer + cell assignment.
        let centroids = if n == 0 {
            vec![0f32; nlist * dim]
        } else {
            kmeans(&prepared, n, dim, nlist, config.kmeans_iters, config.seed)
        };
        let mut postings = vec![Vec::new(); nlist];
        for i in 0..n {
            let cell =
                crate::kmeans::nearest_centroid(&prepared[i * dim..(i + 1) * dim], &centroids, dim);
            postings[cell].push(i as u32);
        }

        let storage = match config.quantization {
            Some(m) => {
                let pq = ProductQuantizer::train(&prepared, n, dim, m, metric, config.seed)?;
                let code_len = pq.code_len();
                let mut codes = vec![0u8; n * code_len];
                for i in 0..n {
                    // `prepared` is already normalized; encode in prepared space.
                    pq.encode_into(
                        &prepared[i * dim..(i + 1) * dim],
                        &mut codes[i * code_len..(i + 1) * code_len],
                    );
                }
                Storage::Pq { pq, codes }
            }
            None => Storage::Flat { vectors: prepared },
        };

        Ok(Self {
            dim,
            metric,
            centroids,
            postings,
            ids: ids.to_vec(),
            storage,
        })
    }

    /// Number of vectors in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Number of Voronoi cells.
    #[must_use]
    pub fn nlist(&self) -> usize {
        self.postings.len()
    }

    /// Search for the `k` nearest neighbors to `query`, probing the `nprobe`
    /// nearest cells. Larger `nprobe` trades latency for recall.
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] if `query.len() != dim`.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        nprobe: usize,
    ) -> Result<Vec<Neighbor>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }
        if self.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let prepared = prepare(self.metric, query);

        // Rank cells by centroid distance, probe the closest `nprobe`.
        let nprobe = nprobe.clamp(1, self.postings.len());
        let mut cells: Vec<(f32, usize)> = self
            .centroids
            .chunks_exact(self.dim)
            .enumerate()
            .map(|(c, centroid)| (rank_distance(self.metric, &prepared, centroid), c))
            .collect();
        cells.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

        // Score every candidate in the probed posting lists.
        let mut scored: Vec<(f32, u32, f32)> = Vec::new();
        match &self.storage {
            Storage::Flat { vectors } => {
                for &(_, cell) in cells.iter().take(nprobe) {
                    for &node in &self.postings[cell] {
                        let v = &vectors[node as usize * self.dim..(node as usize + 1) * self.dim];
                        scored.push((
                            rank_distance(self.metric, &prepared, v),
                            node,
                            report_distance(self.metric, &prepared, v),
                        ));
                    }
                }
            }
            Storage::Pq { pq, codes } => {
                let scorer = pq.scorer(&prepared);
                let code_len = pq.code_len();
                for &(_, cell) in cells.iter().take(nprobe) {
                    for &node in &self.postings[cell] {
                        let start = node as usize * code_len;
                        let approx = scorer.distance(&codes[start..start + code_len]);
                        // PQ mode reports the approximate score (no resident
                        // full vectors to re-rank against — the frugal trade).
                        scored.push((approx, node, approx));
                    }
                }
            }
        }

        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        Ok(scored
            .into_iter()
            .take(k)
            .map(|(_, node, report)| Neighbor {
                id: self.ids[node as usize],
                distance: report,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;
    use std::collections::HashSet;

    fn rand_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_f64() as f32) * 2.0 - 1.0)
            .collect()
    }

    fn brute_force(data: &[Vec<f32>], q: &[f32], k: usize, metric: Metric) -> HashSet<usize> {
        let pq = prepare(metric, q);
        let mut scored: Vec<(f32, usize)> = data
            .iter()
            .enumerate()
            .map(|(i, v)| (rank_distance(metric, &pq, &prepare(metric, v)), i))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    fn dataset(rng: &mut SplitMix64, n: usize, dim: usize) -> (Vec<Vec<f32>>, Vec<f32>, Vec<u64>) {
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(rng, dim)).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        (data, flat, ids)
    }

    fn recall(
        idx: &Ivf,
        data: &[Vec<f32>],
        metric: Metric,
        k: usize,
        nprobe: usize,
        queries: usize,
        rng: &mut SplitMix64,
    ) -> f64 {
        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(rng, data[0].len());
            let truth = brute_force(data, &q, k, metric);
            let got = idx.search(&q, k, nprobe).unwrap();
            hits += got
                .iter()
                .filter(|nbr| truth.contains(&(nbr.id as usize)))
                .count();
        }
        hits as f64 / (queries * k) as f64
    }

    #[test]
    fn ivf_flat_high_recall_with_enough_probes() {
        let (dim, n) = (32, 1000);
        let mut rng = SplitMix64::new(0x1F1);
        let (data, flat, ids) = dataset(&mut rng, n, dim);
        let cfg = IvfConfig {
            nlist: 32,
            ..IvfConfig::default()
        };
        let idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();
        // Probing most cells approaches exhaustive search (IVFFlat is exact at
        // nprobe = nlist); the recall/nprobe trade itself is covered separately.
        let r = recall(&idx, &data, Metric::L2, 10, 28, 50, &mut rng);
        assert!(r >= 0.95, "IVFFlat recall@10 with nprobe=28 was {r:.3}");
    }

    #[test]
    fn nprobe_trades_recall_monotonically() {
        let (dim, n) = (24, 800);
        let mut rng = SplitMix64::new(0x2E2);
        let (data, flat, ids) = dataset(&mut rng, n, dim);
        let cfg = IvfConfig {
            nlist: 40,
            ..IvfConfig::default()
        };
        let idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();
        let mut a = SplitMix64::new(9);
        let mut b = SplitMix64::new(9);
        let low = recall(&idx, &data, Metric::L2, 10, 2, 40, &mut a);
        let high = recall(&idx, &data, Metric::L2, 10, 40, 40, &mut b);
        assert!(
            high >= low,
            "more probes should not reduce recall: {high:.3} vs {low:.3}"
        );
        assert!(
            high >= 0.97,
            "full-probe recall should be near-exhaustive: {high:.3}"
        );
    }

    #[test]
    fn ivf_pq_is_frugal_and_usable() {
        let (dim, n) = (32, 1500);
        let mut rng = SplitMix64::new(0x3D3);
        let (data, flat, ids) = dataset(&mut rng, n, dim);
        let cfg = IvfConfig {
            nlist: 32,
            quantization: Some(8),
            ..IvfConfig::default()
        };
        let idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();
        // PQ mode trades exactness for memory; still finds most neighbors with
        // a broad probe.
        let r = recall(&idx, &data, Metric::L2, 10, 32, 50, &mut rng);
        assert!(r >= 0.70, "IVF+PQ recall@10 was {r:.3}");
    }

    #[test]
    fn cosine_ivf_searches() {
        let (dim, n) = (24, 600);
        let mut rng = SplitMix64::new(0x4C4);
        let (data, flat, ids) = dataset(&mut rng, n, dim);
        let cfg = IvfConfig {
            nlist: 24,
            ..IvfConfig::default()
        };
        let idx = Ivf::build(&ids, &flat, dim, Metric::Cosine, cfg).unwrap();
        let r = recall(&idx, &data, Metric::Cosine, 10, 24, 30, &mut rng);
        assert!(r >= 0.95, "cosine IVF recall@10 was {r:.3}");
    }

    #[test]
    fn build_is_deterministic() {
        let (dim, n) = (16, 500);
        let mut rng = SplitMix64::new(7);
        let (_data, flat, ids) = dataset(&mut rng, n, dim);
        let build = || {
            let idx = Ivf::build(&ids, &flat, dim, Metric::L2, IvfConfig::default()).unwrap();
            idx.search(&vec![0.2; dim], 10, 8)
                .unwrap()
                .into_iter()
                .map(|n| n.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn dot_is_rejected_and_empty_is_handled() {
        let ids: Vec<u64> = (0..5).collect();
        assert!(matches!(
            Ivf::build(&ids, &[0.0; 20], 4, Metric::Dot, IvfConfig::default()),
            Err(IndexError::InvalidConfig(_))
        ));
        let empty = Ivf::build(&[], &[], 4, Metric::L2, IvfConfig::default()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.search(&[0.0; 4], 5, 4).unwrap(), Vec::new());
    }
}
