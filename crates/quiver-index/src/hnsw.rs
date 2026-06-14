// SPDX-License-Identifier: AGPL-3.0-only
//! In-memory HNSW (Malkov & Yashunin, IEEE TPAMI 2020; arXiv:1603.09320).
//!
//! A multi-layer proximity graph: greedy descent through the sparse upper layers
//! to an entry region, then an `ef`-bounded best-first search at the dense base
//! layer. Neighbor selection uses the paper's diversity heuristic (Algorithm 4),
//! which gives materially better recall on clustered data than naive top-`M`.
//!
//! Phase 1 scope: built single-threaded, queried read-only, vectors held full
//! precision in a flat arena. The cache-optimized flat adjacency arena, lock
//! free concurrent reads (atomic publication + EBR, ADR-0006), and quantized
//! codes are Phase 2 work; this module keeps a clear, correct graph first.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};

use quiver_simd::Metric;

use crate::{Index, IndexError, Neighbor};

/// Build-time parameters for [`Hnsw`].
#[derive(Debug, Clone, Copy)]
pub struct HnswConfig {
    /// Target out-degree per node per layer (`M`); the base layer allows `2M`.
    pub m: usize,
    /// Candidate list size during construction (`efConstruction`). Higher builds
    /// a better graph at higher build cost.
    pub ef_construction: usize,
    /// Seed for the level-assignment RNG, so builds are reproducible.
    pub seed: u64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

// A scored candidate. Ordered by distance (ties broken by node id) so a
// `BinaryHeap` is a max-heap on distance and `Reverse` makes a min-heap.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    dist: f32,
    node: u32,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist
            .total_cmp(&other.dist)
            .then_with(|| self.node.cmp(&other.node))
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Distance with "smaller is closer" semantics for every metric, so the search
// heaps order consistently. Shared with the embeddable pre-filter scan via
// `crate::ordering_distance` so the two never diverge.
fn dist(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    crate::ordering_distance(metric, a, b)
}

fn vec_of(vectors: &[f32], dim: usize, node: u32) -> &[f32] {
    let start = node as usize * dim;
    &vectors[start..start + dim]
}

// The paper's neighbor-selection heuristic: keep a candidate only if it is
// closer to the target than to any already-selected neighbor (diversity).
// `candidates` carry their distance to the target; returns up to `m` node ids.
fn select_neighbors(
    metric: Metric,
    vectors: &[f32],
    dim: usize,
    candidates: &[Candidate],
    m: usize,
) -> Vec<u32> {
    let mut ranked = candidates.to_vec();
    ranked.sort_unstable();
    let mut selected: Vec<u32> = Vec::with_capacity(m);
    for cand in &ranked {
        if selected.len() >= m {
            break;
        }
        let cand_vec = vec_of(vectors, dim, cand.node);
        let diverse = selected
            .iter()
            .all(|&s| dist(metric, cand_vec, vec_of(vectors, dim, s)) > cand.dist);
        if diverse {
            selected.push(cand.node);
        }
    }
    selected
}

// A tiny SplitMix64 PRNG for reproducible level assignment.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    // A float in [0, 1) from the top 53 bits.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

/// An in-memory HNSW index.
pub struct Hnsw {
    dim: usize,
    metric: Metric,
    m: usize,
    m_max: usize,
    m_max0: usize,
    ef_construction: usize,
    ml: f64,
    rng: SplitMix64,
    // Flat vector arena: node `i` occupies `[i*dim .. (i+1)*dim]`.
    vectors: Vec<f32>,
    // External id per node, by insertion order.
    ids: Vec<u64>,
    // Top level of each node.
    levels: Vec<usize>,
    // Adjacency: `conns[node][layer]` are the node's neighbors at that layer.
    conns: Vec<Vec<Vec<u32>>>,
    entry_point: Option<u32>,
    max_level: usize,
}

impl Hnsw {
    /// Create an empty index over `dim`-dimensional vectors with the given
    /// metric and configuration.
    #[must_use]
    pub fn new(dim: usize, metric: Metric, config: HnswConfig) -> Self {
        let m = config.m.max(2);
        Self {
            dim,
            metric,
            m,
            m_max: m,
            m_max0: m * 2,
            ef_construction: config.ef_construction.max(m),
            ml: 1.0 / (m as f64).ln(),
            rng: SplitMix64::new(config.seed),
            vectors: Vec::new(),
            ids: Vec::new(),
            levels: Vec::new(),
            conns: Vec::new(),
            entry_point: None,
            max_level: 0,
        }
    }

    /// The dimensionality this index was built with.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The metric this index searches under.
    #[must_use]
    pub fn metric(&self) -> Metric {
        self.metric
    }

    fn random_level(&mut self) -> usize {
        // l = floor(-ln(u) * mL), with u in (0, 1].
        let u = 1.0 - self.rng.next_f64();
        (-u.ln() * self.ml) as usize
    }

    fn vector(&self, node: u32) -> &[f32] {
        vec_of(&self.vectors, self.dim, node)
    }

    fn neighbors(&self, node: u32, layer: usize) -> &[u32] {
        self.conns[node as usize]
            .get(layer)
            .map_or(&[][..], Vec::as_slice)
    }

    fn max_conns(&self, layer: usize) -> usize {
        if layer == 0 { self.m_max0 } else { self.m_max }
    }

    // Best-first search of one layer; returns up to `ef` candidates, closest
    // first.
    fn search_layer(
        &self,
        query: &[f32],
        entry_points: &[u32],
        ef: usize,
        layer: usize,
    ) -> Vec<Candidate> {
        let mut visited: HashSet<u32> = HashSet::new();
        let mut frontier: BinaryHeap<Reverse<Candidate>> = BinaryHeap::new();
        let mut results: BinaryHeap<Candidate> = BinaryHeap::new();

        for &ep in entry_points {
            let d = dist(self.metric, self.vector(ep), query);
            let c = Candidate { dist: d, node: ep };
            visited.insert(ep);
            frontier.push(Reverse(c));
            results.push(c);
        }
        while results.len() > ef {
            results.pop();
        }

        while let Some(Reverse(current)) = frontier.pop() {
            let farthest = results.peek().map_or(f32::INFINITY, |c| c.dist);
            if results.len() >= ef && current.dist > farthest {
                break;
            }
            for &nb in self.neighbors(current.node, layer) {
                if visited.insert(nb) {
                    let d = dist(self.metric, self.vector(nb), query);
                    let farthest = results.peek().map_or(f32::INFINITY, |c| c.dist);
                    if results.len() < ef || d < farthest {
                        let c = Candidate { dist: d, node: nb };
                        frontier.push(Reverse(c));
                        results.push(c);
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }

        let mut out = results.into_vec();
        out.sort_unstable();
        out
    }

    // Link the new `node` to `selected` at `layer`, adding back-links and
    // pruning over-full neighbor lists with the selection heuristic.
    fn connect(&mut self, node: u32, selected: &[u32], layer: usize) {
        self.conns[node as usize][layer] = selected.to_vec();
        let max = self.max_conns(layer);
        for &e in selected {
            self.conns[e as usize][layer].push(node);
            if self.conns[e as usize][layer].len() > max {
                let current: Vec<u32> = self.conns[e as usize][layer].clone();
                let e_vec = self.vector(e).to_vec();
                let cands: Vec<Candidate> = current
                    .iter()
                    .map(|&n| Candidate {
                        dist: dist(self.metric, &e_vec, self.vector(n)),
                        node: n,
                    })
                    .collect();
                let pruned = select_neighbors(self.metric, &self.vectors, self.dim, &cands, max);
                self.conns[e as usize][layer] = pruned;
            }
        }
    }
}

impl Index for Hnsw {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError> {
        if vector.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: vector.len(),
            });
        }
        let node = self.ids.len() as u32;
        self.ids.push(id);
        self.vectors.extend_from_slice(vector);
        let level = self.random_level();
        self.levels.push(level);
        self.conns.push(vec![Vec::new(); level + 1]);

        let Some(entry) = self.entry_point else {
            self.entry_point = Some(node);
            self.max_level = level;
            return Ok(());
        };

        // Greedy descent through the layers above the new node's top level.
        let mut ep = entry;
        let mut cur = self.max_level;
        while cur > level {
            let w = self.search_layer(vector, &[ep], 1, cur);
            if let Some(best) = w.first() {
                ep = best.node;
            }
            cur -= 1;
        }

        // Connect from the highest shared layer down to the base.
        let mut ep_set = vec![ep];
        let mut layer = level.min(self.max_level);
        loop {
            let w = self.search_layer(vector, &ep_set, self.ef_construction, layer);
            let selected = select_neighbors(self.metric, &self.vectors, self.dim, &w, self.m);
            self.connect(node, &selected, layer);
            ep_set = w.iter().map(|c| c.node).collect();
            if layer == 0 {
                break;
            }
            layer -= 1;
        }

        if level > self.max_level {
            self.max_level = level;
            self.entry_point = Some(node);
        }
        Ok(())
    }

    fn search(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
    ) -> Result<Vec<Neighbor>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }
        let Some(entry) = self.entry_point else {
            return Ok(Vec::new());
        };
        if k == 0 {
            return Ok(Vec::new());
        }

        let mut ep = entry;
        let mut cur = self.max_level;
        while cur > 0 {
            let w = self.search_layer(query, &[ep], 1, cur);
            if let Some(best) = w.first() {
                ep = best.node;
            }
            cur -= 1;
        }
        let ef = ef_search.max(k);
        let w = self.search_layer(query, &[ep], ef, 0);
        Ok(w.into_iter()
            .take(k)
            .map(|c| Neighbor {
                id: self.ids[c.node as usize],
                // Un-negate similarities so the reported value is the true metric.
                distance: crate::report_metric(self.metric, c.dist),
            })
            .collect())
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reuse the PRNG for test data, kept separate from the index's own.
    fn rand_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_f64() as f32) * 2.0 - 1.0)
            .collect()
    }

    fn brute_force(data: &[Vec<f32>], q: &[f32], k: usize, metric: Metric) -> Vec<u64> {
        let mut scored: Vec<Candidate> = data
            .iter()
            .enumerate()
            .map(|(i, v)| Candidate {
                dist: dist(metric, v, q),
                node: i as u32,
            })
            .collect();
        scored.sort_unstable();
        scored
            .into_iter()
            .take(k)
            .map(|c| u64::from(c.node))
            .collect()
    }

    #[test]
    fn empty_index_returns_nothing() {
        let h = Hnsw::new(4, Metric::L2, HnswConfig::default());
        assert!(h.is_empty());
        assert_eq!(h.search(&[0.0; 4], 5, 50).unwrap(), Vec::new());
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let mut h = Hnsw::new(4, Metric::L2, HnswConfig::default());
        assert!(matches!(
            h.insert(1, &[0.0; 3]),
            Err(IndexError::DimensionMismatch {
                expected: 4,
                got: 3
            })
        ));
        h.insert(1, &[0.0; 4]).unwrap();
        assert!(matches!(
            h.search(&[0.0; 5], 1, 10),
            Err(IndexError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn finds_inserted_vector_as_nearest() {
        let mut rng = SplitMix64::new(7);
        let mut h = Hnsw::new(16, Metric::L2, HnswConfig::default());
        let mut data = Vec::new();
        for i in 0..200u64 {
            let v = rand_vec(&mut rng, 16);
            h.insert(i, &v).unwrap();
            data.push(v);
        }
        // Querying with an exact stored vector returns it first, distance ~0.
        let top = h.search(&data[42], 1, 50).unwrap();
        assert_eq!(top[0].id, 42);
        assert!(top[0].distance < 1e-4);
    }

    #[test]
    fn recall_at_10_meets_threshold_l2() {
        let dim = 32;
        let n = 2000;
        let queries = 100;
        let k = 10;
        let mut rng = SplitMix64::new(0xDEAD_BEEF);
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, dim)).collect();

        let mut h = Hnsw::new(dim, Metric::L2, HnswConfig::default());
        for (i, v) in data.iter().enumerate() {
            h.insert(i as u64, v).unwrap();
        }

        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(&mut rng, dim);
            let truth: HashSet<u64> = brute_force(&data, &q, k, Metric::L2).into_iter().collect();
            let got = h.search(&q, k, 64).unwrap();
            hits += got.iter().filter(|n| truth.contains(&n.id)).count();
        }
        let recall = hits as f64 / (queries * k) as f64;
        assert!(
            recall >= 0.95,
            "recall@10 was {recall:.3}, expected >= 0.95"
        );
    }

    #[test]
    fn cosine_and_dot_find_correct_neighbors() {
        for metric in [Metric::Cosine, Metric::Dot] {
            let dim = 24;
            let n = 500;
            let mut rng = SplitMix64::new(123);
            let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, dim)).collect();
            let mut h = Hnsw::new(dim, metric, HnswConfig::default());
            for (i, v) in data.iter().enumerate() {
                h.insert(i as u64, v).unwrap();
            }
            let mut hits = 0usize;
            let k = 5;
            for _ in 0..40 {
                let q = rand_vec(&mut rng, dim);
                let truth: HashSet<u64> = brute_force(&data, &q, k, metric).into_iter().collect();
                let got = h.search(&q, k, 64).unwrap();
                hits += got.iter().filter(|n| truth.contains(&n.id)).count();
            }
            let recall = hits as f64 / (40 * k) as f64;
            assert!(recall >= 0.90, "{metric:?} recall {recall:.3} < 0.90");
        }
    }

    #[test]
    fn build_is_deterministic_for_a_fixed_seed() {
        let mut rng = SplitMix64::new(5);
        let data: Vec<Vec<f32>> = (0..300).map(|_| rand_vec(&mut rng, 12)).collect();
        let build = || {
            let mut h = Hnsw::new(12, Metric::L2, HnswConfig::default());
            for (i, v) in data.iter().enumerate() {
                h.insert(i as u64, v).unwrap();
            }
            let q = vec![0.1f32; 12];
            h.search(&q, 10, 50)
                .unwrap()
                .into_iter()
                .map(|n| n.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
    }
}
