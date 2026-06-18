// SPDX-License-Identifier: AGPL-3.0-only
//! In-memory Vamana graph (DiskANN; Subramanya et al., NeurIPS 2019).
//!
//! A single flat directed graph with bounded out-degree `R`, built so that a
//! greedy beam search finds high-recall neighbors in few hops. Two ingredients:
//!
//! - **GreedySearch** — beam search from the medoid; returns the closest
//!   candidates *and* the full visited set.
//! - **RobustPrune** — choose ≤`R` diverse out-neighbors using the α-slack rule:
//!   keep the closest candidate, then drop any other candidate that the chosen
//!   one is already `α×` closer to (so edges span the space instead of clumping).
//!
//! This module builds and searches the graph **in memory at full precision** —
//! the recall-validated core. Phase 2's disk index lays this same graph out on
//! SSD with PQ-compressed navigation and exact re-rank (a following PR); the
//! accessors here ([`Vamana::neighbors`], [`Vamana::vector`], [`Vamana::medoid`])
//! exist so that layout can read the built graph.
//!
//! Vamana is best **batch-built** ([`Vamana::build`]) when the whole set is
//! known — the medoid, build order, and two-pass pruning settle a higher-recall
//! graph. For streaming maintenance it also supports one-at-a-time
//! [`Vamana::insert`] (the FreshDiskANN temporary index, ADR-0033), which a
//! "fresh" wrapper layers over a read-only base graph. It supports
//! [`Metric::L2`] and
//! [`Metric::Cosine`] (cosine vectors are unit-normalized, so the graph is built
//! on the sphere where L2 ordering matches cosine ordering); inner-product
//! (MIPS) collections use HNSW or IVF.

use std::cmp::Ordering;
use std::collections::HashSet;

use quiver_simd::Metric;

use crate::rng::SplitMix64;
use crate::{IndexError, Neighbor};

/// Build parameters for [`Vamana`].
#[derive(Debug, Clone, Copy)]
pub struct VamanaConfig {
    /// Maximum out-degree per node (`R`). Typical 32–128.
    pub r: usize,
    /// Build-time search-list width (`L`). Higher builds a better graph slower.
    pub l_build: usize,
    /// Prune slack (`α ≥ 1`). `1.2` is the DiskANN default; larger keeps longer
    /// edges (better reach, denser graph).
    pub alpha: f32,
    /// Seed for the random initial graph and build order (reproducible builds).
    pub seed: u64,
}

impl Default for VamanaConfig {
    fn default() -> Self {
        Self {
            r: 32,
            l_build: 64,
            alpha: 1.2,
            seed: 0x5EED_5EED_5EED_5EED,
        }
    }
}

// A scored node, ordered by distance then id (so heaps/sorts are deterministic).
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

/// An in-memory Vamana graph index.
pub struct Vamana {
    dim: usize,
    metric: Metric,
    r: usize,
    // Build/insert search-list width and prune slack, kept so incremental
    // inserts ([`Vamana::insert`]) reuse the same parameters as the batch build.
    l_build: usize,
    alpha: f32,
    // Prepared vectors (unit-normalized for cosine), flat: node i at [i*dim..].
    vectors: Vec<f32>,
    ids: Vec<u64>,
    // Out-neighbors per node (each ≤ r after pruning).
    adjacency: Vec<Vec<u32>>,
    medoid: u32,
}

// Navigation distance: squared L2 on prepared vectors. For cosine, vectors are
// unit length, so L2² = 2 − 2·cos is order-equivalent to cosine similarity.
fn nav_dist(a: &[f32], b: &[f32]) -> f32 {
    quiver_simd::l2_sq_f32(a, b)
}

// Unit-normalize for cosine; pass through otherwise.
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

impl Vamana {
    /// Build a Vamana graph over `ids` and their `vectors` (flat `n × dim`).
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidConfig`] for [`Metric::Dot`] (unsupported),
    /// or [`IndexError::DimensionMismatch`] if `vectors.len()` is not `n × dim`.
    ///
    /// # Panics
    /// Does not panic for valid inputs.
    pub fn build(
        ids: &[u64],
        vectors: &[f32],
        dim: usize,
        metric: Metric,
        config: VamanaConfig,
    ) -> Result<Self, IndexError> {
        if metric == Metric::Dot {
            return Err(IndexError::InvalidConfig(
                "Vamana supports L2 and Cosine; use HNSW or IVF for inner product",
            ));
        }
        let n = ids.len();
        if vectors.len() != n * dim {
            return Err(IndexError::DimensionMismatch {
                expected: n * dim,
                got: vectors.len(),
            });
        }
        let r = config.r.max(1);

        // Prepare (normalize for cosine) into a flat arena.
        let mut prepared = vec![0f32; n * dim];
        for i in 0..n {
            let p = prepare(metric, &vectors[i * dim..(i + 1) * dim]);
            prepared[i * dim..(i + 1) * dim].copy_from_slice(&p);
        }

        let mut graph = Self {
            dim,
            metric,
            r,
            l_build: config.l_build.max(1),
            alpha: config.alpha.max(1.0),
            vectors: prepared,
            ids: ids.to_vec(),
            adjacency: vec![Vec::new(); n],
            medoid: 0,
        };
        if n <= 1 {
            return Ok(graph);
        }

        graph.medoid = graph.compute_medoid();
        let mut rng = SplitMix64::new(config.seed);
        graph.init_random_graph(&mut rng);

        // Two passes: α = 1 to settle, then the configured slack for reach.
        let order = random_permutation(n, &mut rng);
        for &alpha in &[1.0f32, config.alpha.max(1.0)] {
            for &p in &order {
                let (_, visited) =
                    graph.greedy_search(graph.vector(p), graph.medoid, config.l_build);
                let pruned = graph.robust_prune(p, visited, alpha);
                graph.adjacency[p as usize] = pruned.clone();
                // Add back-edges, re-pruning any neighbor that overflows `R`.
                for &j in &pruned {
                    if j == p {
                        continue;
                    }
                    let adj = &mut graph.adjacency[j as usize];
                    if !adj.contains(&p) {
                        adj.push(p);
                        if adj.len() > r {
                            let cand = graph.adjacency[j as usize].clone();
                            graph.adjacency[j as usize] = graph.robust_prune(j, cand, alpha);
                        }
                    }
                }
            }
        }
        Ok(graph)
    }

    /// Create an empty graph that points can be added to one at a time with
    /// [`Vamana::insert`] (the FreshDiskANN temporary index, ADR-0033). Use
    /// [`Vamana::build`] when the whole set is known up front — the batch build
    /// settles the graph in two passes and yields slightly higher recall.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidConfig`] for [`Metric::Dot`] (unsupported;
    /// use HNSW or IVF for inner product).
    pub fn new(dim: usize, metric: Metric, config: VamanaConfig) -> Result<Self, IndexError> {
        if metric == Metric::Dot {
            return Err(IndexError::InvalidConfig(
                "Vamana supports L2 and Cosine; use HNSW or IVF for inner product",
            ));
        }
        Ok(Self {
            dim,
            metric,
            r: config.r.max(1),
            l_build: config.l_build.max(1),
            alpha: config.alpha.max(1.0),
            vectors: Vec::new(),
            ids: Vec::new(),
            adjacency: Vec::new(),
            medoid: 0,
        })
    }

    /// Add one point incrementally (ADR-0033): the FreshDiskANN insert — a greedy
    /// search from the medoid for candidates, `robust_prune` to pick the
    /// new node's ≤`R` out-neighbors, then bidirectional edges with a re-prune of
    /// any neighbor that overflows `R`. The medoid is kept as-is (the temporary
    /// index this backs is small and consolidated often), so navigation stays
    /// anchored at the first inserted node.
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
        let p = self.ids.len() as u32;
        let prepared = prepare(self.metric, vector);
        self.vectors.extend_from_slice(&prepared);
        self.ids.push(id);
        self.adjacency.push(Vec::new());
        // The first node is the medoid and has no neighbors yet.
        if p == 0 {
            self.medoid = 0;
            return Ok(());
        }

        let (_, visited) = self.greedy_search(self.vector(p), self.medoid, self.l_build);
        let pruned = self.robust_prune(p, visited, self.alpha);
        self.adjacency[p as usize] = pruned.clone();
        // Add back-edges, re-pruning any neighbor that overflows `R` (mirrors the
        // batch build's back-edge step).
        for &j in &pruned {
            if j == p {
                continue;
            }
            let adj = &mut self.adjacency[j as usize];
            if !adj.contains(&p) {
                adj.push(p);
                if adj.len() > self.r {
                    let cand = self.adjacency[j as usize].clone();
                    self.adjacency[j as usize] = self.robust_prune(j, cand, self.alpha);
                }
            }
        }
        Ok(())
    }

    /// Number of points in the graph.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the graph holds no points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The metric this graph searches under.
    #[must_use]
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// Dimensionality of the indexed vectors.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Maximum out-degree (`R`) the graph was built with.
    #[must_use]
    pub fn max_degree(&self) -> usize {
        self.r
    }

    /// External ids, indexed by internal node id.
    #[must_use]
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// The medoid node id (the navigation start, useful to the disk layout).
    #[must_use]
    pub fn medoid(&self) -> u32 {
        self.medoid
    }

    /// Out-neighbors of `node`.
    #[must_use]
    pub fn neighbors(&self, node: u32) -> &[u32] {
        &self.adjacency[node as usize]
    }

    /// The (prepared) vector of `node`.
    #[must_use]
    pub fn vector(&self, node: u32) -> &[f32] {
        &self.vectors[node as usize * self.dim..(node as usize + 1) * self.dim]
    }

    /// Search for the `k` nearest neighbors to `query`, closest first.
    /// `l_search` is the beam width (clamped up to at least `k`).
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] if `query.len() != dim`.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        l_search: usize,
    ) -> Result<Vec<Neighbor>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }
        if self.ids.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let prepared = prepare(self.metric, query);
        let l = l_search.max(k);
        let (candidates, _) = self.greedy_search(&prepared, self.medoid, l);
        Ok(candidates
            .into_iter()
            .take(k)
            .map(|c| Neighbor {
                id: self.ids[c.node as usize],
                distance: self.report_distance(&prepared, c.node, c.dist),
            })
            .collect())
    }

    // Report the metric value: L2² as navigated, true cosine for cosine.
    fn report_distance(&self, prepared_query: &[f32], node: u32, nav: f32) -> f32 {
        match self.metric {
            Metric::L2 => nav,
            Metric::Cosine => quiver_simd::cosine_f32(prepared_query, self.vector(node)),
            Metric::Dot => -nav, // unreachable (Dot rejected at build)
        }
    }

    // Medoid ≈ the node nearest the mean vector (O(n), a good navigation start).
    fn compute_medoid(&self) -> u32 {
        let n = self.ids.len();
        let mut mean = vec![0f32; self.dim];
        for i in 0..n {
            for (m, &x) in mean.iter_mut().zip(self.vector(i as u32)) {
                *m += x;
            }
        }
        let inv = 1.0 / n as f32;
        mean.iter_mut().for_each(|m| *m *= inv);
        let mut best = 0u32;
        let mut best_d = f32::INFINITY;
        for i in 0..n {
            let d = nav_dist(&mean, self.vector(i as u32));
            if d < best_d {
                best_d = d;
                best = i as u32;
            }
        }
        best
    }

    // Seed each node with `r` random distinct out-neighbors for initial reach.
    fn init_random_graph(&mut self, rng: &mut SplitMix64) {
        let n = self.ids.len();
        for i in 0..n {
            let mut seen = HashSet::new();
            let want = self.r.min(n - 1);
            while seen.len() < want {
                let cand = rng.below(n) as u32;
                if cand != i as u32 {
                    seen.insert(cand);
                }
            }
            self.adjacency[i] = seen.into_iter().collect();
        }
    }

    // Beam search from `start`; returns (closest candidates sorted, visited set).
    fn greedy_search(&self, query: &[f32], start: u32, l: usize) -> (Vec<Candidate>, Vec<u32>) {
        let mut working: Vec<Candidate> = vec![Candidate {
            dist: nav_dist(query, self.vector(start)),
            node: start,
        }];
        let mut in_working: HashSet<u32> = HashSet::from([start]);
        let mut visited: HashSet<u32> = HashSet::new();
        let mut visited_order: Vec<u32> = Vec::new();

        // Expand the closest not-yet-visited candidate until none remain.
        while let Some(p) = working
            .iter()
            .filter(|c| !visited.contains(&c.node))
            .min()
            .copied()
        {
            visited.insert(p.node);
            visited_order.push(p.node);
            for &nb in &self.adjacency[p.node as usize] {
                if !in_working.contains(&nb) {
                    in_working.insert(nb);
                    working.push(Candidate {
                        dist: nav_dist(query, self.vector(nb)),
                        node: nb,
                    });
                }
            }
            // Keep only the `l` closest in the working set.
            working.sort_unstable();
            if working.len() > l {
                for c in working.drain(l..) {
                    in_working.remove(&c.node);
                }
            }
        }
        (working, visited_order)
    }

    // RobustPrune: choose ≤ r diverse neighbors of `p` from `candidates`.
    fn robust_prune(&self, p: u32, candidates: Vec<u32>, alpha: f32) -> Vec<u32> {
        let p_vec = self.vector(p);
        // Distinct candidates excluding p, scored by distance to p.
        let mut seen = HashSet::new();
        let mut cand: Vec<Candidate> = candidates
            .into_iter()
            .filter(|&c| c != p && seen.insert(c))
            .map(|c| Candidate {
                dist: nav_dist(p_vec, self.vector(c)),
                node: c,
            })
            .collect();

        let mut result: Vec<u32> = Vec::with_capacity(self.r);
        while !cand.is_empty() {
            cand.sort_unstable();
            let p_star = cand[0];
            result.push(p_star.node);
            if result.len() >= self.r {
                break;
            }
            let star_vec = self.vector(p_star.node);
            // Drop any candidate that p* is already α× closer to than p is.
            cand.retain(|c| alpha * nav_dist(star_vec, self.vector(c.node)) > c.dist);
        }
        result
    }
}

// A deterministic Fisher–Yates shuffle of 0..n.
fn random_permutation(n: usize, rng: &mut SplitMix64) -> Vec<u32> {
    let mut perm: Vec<u32> = (0..n as u32).collect();
    for i in (1..n).rev() {
        let j = rng.below(i + 1);
        perm.swap(i, j);
    }
    perm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rand_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_f64() as f32) * 2.0 - 1.0)
            .collect()
    }

    fn brute_force(data: &[Vec<f32>], q: &[f32], k: usize, metric: Metric) -> Vec<usize> {
        let mut scored: Vec<Candidate> = data
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let pv = prepare(metric, v);
                let pq = prepare(metric, q);
                Candidate {
                    dist: nav_dist(&pq, &pv),
                    node: i as u32,
                }
            })
            .collect();
        scored.sort_unstable();
        scored
            .into_iter()
            .take(k)
            .map(|c| c.node as usize)
            .collect()
    }

    fn recall_at_k(metric: Metric) -> f64 {
        // Kept modest so the suite stays fast and does not starve the
        // quiver-core SIGKILL crash fixture when run in parallel; recall is
        // statistically solid at this size.
        let (dim, n, queries, k) = (32, 1000, 50, 10);
        let mut rng = SplitMix64::new(0xDEAD_BEEF ^ metric_seed(metric));
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, dim)).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let g = Vamana::build(&ids, &flat, dim, metric, VamanaConfig::default()).unwrap();

        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(&mut rng, dim);
            let truth: HashSet<usize> = brute_force(&data, &q, k, metric).into_iter().collect();
            let got = g.search(&q, k, 64).unwrap();
            hits += got
                .iter()
                .filter(|nbr| truth.contains(&(nbr.id as usize)))
                .count();
        }
        hits as f64 / (queries * k) as f64
    }

    fn metric_seed(metric: Metric) -> u64 {
        match metric {
            Metric::L2 => 1,
            Metric::Cosine => 2,
            Metric::Dot => 3,
        }
    }

    #[test]
    fn recall_at_10_l2() {
        let r = recall_at_k(Metric::L2);
        assert!(r >= 0.95, "Vamana L2 recall@10 was {r:.3}");
    }

    #[test]
    fn recall_at_10_cosine() {
        let r = recall_at_k(Metric::Cosine);
        assert!(r >= 0.95, "Vamana cosine recall@10 was {r:.3}");
    }

    #[test]
    fn finds_exact_vector_as_nearest() {
        let mut rng = SplitMix64::new(7);
        let data: Vec<Vec<f32>> = (0..300).map(|_| rand_vec(&mut rng, 16)).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..300u64).collect();
        let g = Vamana::build(&ids, &flat, 16, Metric::L2, VamanaConfig::default()).unwrap();
        let top = g.search(&data[42], 1, 50).unwrap();
        assert_eq!(top[0].id, 42);
        assert!(top[0].distance < 1e-4);
    }

    #[test]
    fn out_degree_is_bounded() {
        let mut rng = SplitMix64::new(11);
        let n = 500;
        let data: Vec<f32> = (0..n * 8).map(|_| rng.next_f64() as f32).collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let cfg = VamanaConfig {
            r: 24,
            ..VamanaConfig::default()
        };
        let g = Vamana::build(&ids, &data, 8, Metric::L2, cfg).unwrap();
        for node in 0..n as u32 {
            assert!(g.neighbors(node).len() <= 24, "node {node} over-degree");
        }
    }

    #[test]
    fn build_is_deterministic() {
        let mut rng = SplitMix64::new(5);
        let n = 400;
        let data: Vec<f32> = (0..n * 12)
            .map(|_| rng.next_f64() as f32 * 2.0 - 1.0)
            .collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let build = || {
            let g = Vamana::build(&ids, &data, 12, Metric::L2, VamanaConfig::default()).unwrap();
            let q = vec![0.1f32; 12];
            g.search(&q, 10, 50)
                .unwrap()
                .into_iter()
                .map(|nbr| nbr.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn dot_metric_is_rejected() {
        let data = vec![0f32; 10 * 4];
        let ids: Vec<u64> = (0..10).collect();
        assert!(matches!(
            Vamana::build(&ids, &data, 4, Metric::Dot, VamanaConfig::default()),
            Err(IndexError::InvalidConfig(_))
        ));
    }

    #[test]
    fn empty_and_singleton() {
        let g = Vamana::build(&[], &[], 4, Metric::L2, VamanaConfig::default()).unwrap();
        assert!(g.is_empty());
        assert_eq!(g.search(&[0.0; 4], 5, 10).unwrap(), Vec::new());
        let g = Vamana::build(
            &[7],
            &[1.0, 2.0, 3.0, 4.0],
            4,
            Metric::L2,
            VamanaConfig::default(),
        )
        .unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g.search(&[1.0, 2.0, 3.0, 4.0], 3, 10).unwrap()[0].id, 7);
    }

    #[test]
    fn new_rejects_dot() {
        assert!(matches!(
            Vamana::new(4, Metric::Dot, VamanaConfig::default()),
            Err(IndexError::InvalidConfig(_))
        ));
    }

    #[test]
    fn insert_into_empty_then_finds_it() {
        let mut g = Vamana::new(4, Metric::L2, VamanaConfig::default()).unwrap();
        assert!(g.is_empty());
        g.insert(7, &[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(g.len(), 1);
        let top = g.search(&[1.0, 2.0, 3.0, 4.0], 3, 10).unwrap();
        assert_eq!(top[0].id, 7);
        assert!(top[0].distance < 1e-4);
    }

    #[test]
    fn insert_dimension_mismatch_is_rejected() {
        let mut g = Vamana::new(4, Metric::L2, VamanaConfig::default()).unwrap();
        assert!(matches!(
            g.insert(1, &[0.0; 3]),
            Err(IndexError::DimensionMismatch {
                expected: 4,
                got: 3
            })
        ));
    }

    // Build the graph purely by incremental inserts and measure recall against
    // the same brute-force ground truth the batch build is held to. A
    // single-pass incremental graph is a touch below a two-pass batch build, so
    // the threshold is a hair lower than `build`'s 0.95.
    fn incremental_recall_at_k(metric: Metric) -> f64 {
        let (dim, n, queries, k) = (32, 1000, 50, 10);
        let mut rng = SplitMix64::new(0x1_2345_6789_u64.wrapping_add(metric_seed(metric)));
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, dim)).collect();
        let mut g = Vamana::new(dim, metric, VamanaConfig::default()).unwrap();
        for (i, v) in data.iter().enumerate() {
            g.insert(i as u64, v).unwrap();
        }
        assert_eq!(g.len(), n);

        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(&mut rng, dim);
            let truth: HashSet<usize> = brute_force(&data, &q, k, metric).into_iter().collect();
            let got = g.search(&q, k, 64).unwrap();
            hits += got
                .iter()
                .filter(|nbr| truth.contains(&(nbr.id as usize)))
                .count();
        }
        hits as f64 / (queries * k) as f64
    }

    #[test]
    fn incremental_insert_recall_l2() {
        let r = incremental_recall_at_k(Metric::L2);
        assert!(r >= 0.90, "incremental L2 recall@10 was {r:.3}");
    }

    #[test]
    fn incremental_insert_recall_cosine() {
        let r = incremental_recall_at_k(Metric::Cosine);
        assert!(r >= 0.90, "incremental cosine recall@10 was {r:.3}");
    }

    #[test]
    fn incremental_insert_is_deterministic() {
        let mut rng = SplitMix64::new(13);
        let data: Vec<Vec<f32>> = (0..400).map(|_| rand_vec(&mut rng, 12)).collect();
        let build = || {
            let mut g = Vamana::new(12, Metric::L2, VamanaConfig::default()).unwrap();
            for (i, v) in data.iter().enumerate() {
                g.insert(i as u64, v).unwrap();
            }
            let q = vec![0.1f32; 12];
            g.search(&q, 10, 50)
                .unwrap()
                .into_iter()
                .map(|nbr| nbr.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn incremental_out_degree_is_bounded() {
        let mut rng = SplitMix64::new(0xB0);
        let cfg = VamanaConfig {
            r: 24,
            ..VamanaConfig::default()
        };
        let mut g = Vamana::new(8, Metric::L2, cfg).unwrap();
        for i in 0..500u64 {
            g.insert(i, &rand_vec(&mut rng, 8)).unwrap();
        }
        for node in 0..500u32 {
            assert!(g.neighbors(node).len() <= 24, "node {node} over-degree");
        }
    }
}
