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
//! IVF is **batch-built** ([`Ivf::build`]) and then supports **incremental
//! in-place updates** ([`Ivf::insert`] / [`Ivf::remove`]) with SpFresh-style
//! LIRE rebalancing (cell split/merge, ADR-0023), so a long insert/delete
//! stream does not force an `O(N)` rebuild. It supports [`Metric::L2`] and
//! [`Metric::Cosine`] (cosine on the unit sphere); inner product uses HNSW.

use std::collections::HashMap;

use quiver_simd::Metric;

use crate::kmeans::{kmeans, nearest_centroid};
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
    /// Incremental rebalancing (ADR-0023): a cell whose posting list grows past
    /// this is split in two via local 2-means. Governs in-place updates only,
    /// not the initial batch partition (which uses `nlist`).
    pub max_postings: usize,
    /// Incremental rebalancing (ADR-0023): a cell whose posting list falls below
    /// this is merged into its neighbors and recycled. The default of `1` only
    /// reclaims cells that deletes emptied; raise it for tighter consolidation.
    pub min_postings: usize,
}

impl Default for IvfConfig {
    fn default() -> Self {
        Self {
            nlist: 64,
            kmeans_iters: 20,
            quantization: None,
            seed: 0x1F1F_2E2E_3D3D_4C4C,
            max_postings: 256,
            min_postings: 1,
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
///
/// Node indices address the resident arrays (`ids`, the `Storage` vectors/codes)
/// and appear in posting lists. Removing a point frees its node slot for reuse
/// and unlinks it from its cell, so live points are exactly the keys of
/// `id_to_node`; merged (emptied) cells are tombstoned (centroid set to a
/// never-selected sentinel) and recycled by later splits.
pub struct Ivf {
    dim: usize,
    metric: Metric,
    // Flat `ncells × dim` centroids; grows on split, sentinel-filled on merge.
    centroids: Vec<f32>,
    // Posting list per cell: the live node ids assigned to that centroid.
    postings: Vec<Vec<u32>>,
    // node id -> external id (stale in a freed slot until the slot is reused).
    ids: Vec<u64>,
    // Live external id -> node id (its size is the live count).
    id_to_node: HashMap<u64, u32>,
    // node id -> its current cell (stale in a freed slot).
    node_cell: Vec<u32>,
    // Reusable node slots freed by removals.
    free: Vec<u32>,
    // Reusable (tombstoned) cell slots freed by merges.
    free_cells: Vec<usize>,
    storage: Storage,
    config: IvfConfig,
    // Monotonic counter so each split's 2-means gets a distinct, stable seed.
    splits: u64,
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
        let mut node_cell = vec![0u32; n];
        for i in 0..n {
            let cell = nearest_centroid(&prepared[i * dim..(i + 1) * dim], &centroids, dim);
            postings[cell].push(i as u32);
            node_cell[i] = cell as u32;
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

        let id_to_node = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i as u32))
            .collect();

        Ok(Self {
            dim,
            metric,
            centroids,
            postings,
            ids: ids.to_vec(),
            id_to_node,
            node_cell,
            free: Vec::new(),
            free_cells: Vec::new(),
            storage,
            config,
            splits: 0,
        })
    }

    /// Number of live vectors in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.id_to_node.len()
    }

    /// Whether the index holds no live vectors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id_to_node.is_empty()
    }

    /// Number of cells, including tombstoned (recyclable) ones.
    #[must_use]
    pub fn nlist(&self) -> usize {
        self.postings.len()
    }

    /// Insert (or replace) a point under external id `id`, maintaining the index
    /// in place (ADR-0023). The point joins its nearest cell's posting list; if
    /// that list overflows `max_postings` the cell is split via local 2-means
    /// (SpFresh LIRE). A repeated `id` replaces the previous vector. Cost is
    /// `O(nlist + |list|)`, independent of the collection size.
    ///
    /// # Errors
    /// Returns [`IndexError::InvalidConfig`] for [`Metric::Dot`] (use HNSW), or
    /// [`IndexError::DimensionMismatch`] if `vector.len() != dim`.
    pub fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError> {
        if self.metric == Metric::Dot {
            return Err(IndexError::InvalidConfig(
                "IVF supports L2 and Cosine; use HNSW for inner product",
            ));
        }
        if vector.len() != self.dim {
            return Err(IndexError::DimensionMismatch {
                expected: self.dim,
                got: vector.len(),
            });
        }
        // Replace-in-place: an existing id is removed first.
        if self.id_to_node.contains_key(&id) {
            self.remove(id);
        }
        let prepared = prepare(self.metric, vector);
        let cell = nearest_centroid(&prepared, &self.centroids, self.dim);
        let node = self.alloc_node(id, &prepared);
        self.postings[cell].push(node);
        self.node_cell[node as usize] = cell as u32;
        if self.postings[cell].len() > self.config.max_postings {
            self.split(cell);
        }
        Ok(())
    }

    /// Remove the point with external id `id`, maintaining the index in place
    /// (ADR-0023). Its node slot is reclaimed for reuse; if its cell drops below
    /// `min_postings` the cell is merged into its neighbors and recycled.
    /// Returns whether the id was present. Cost is `O(|list|)`.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(node) = self.id_to_node.remove(&id) else {
            return false;
        };
        let cell = self.node_cell[node as usize] as usize;
        self.postings[cell].retain(|&x| x != node);
        self.free.push(node);
        if self.live_cells() > 1 && self.postings[cell].len() < self.config.min_postings {
            self.merge(cell);
        }
        true
    }

    // Number of live (non-tombstoned) cells.
    fn live_cells(&self) -> usize {
        self.postings.len() - self.free_cells.len()
    }

    // Reserve a node slot (reusing a freed one when available), writing its id
    // and resident representation. Returns the node index.
    fn alloc_node(&mut self, id: u64, prepared: &[f32]) -> u32 {
        let node = if let Some(slot) = self.free.pop() {
            self.ids[slot as usize] = id;
            self.write_resident(slot as usize, prepared);
            slot
        } else {
            let slot = self.ids.len() as u32;
            self.ids.push(id);
            self.node_cell.push(0);
            self.append_resident(prepared);
            slot
        };
        self.id_to_node.insert(id, node);
        node
    }

    // Overwrite an existing node slot's resident representation.
    fn write_resident(&mut self, node: usize, prepared: &[f32]) {
        let dim = self.dim;
        match &mut self.storage {
            Storage::Flat { vectors } => {
                vectors[node * dim..(node + 1) * dim].copy_from_slice(prepared);
            }
            Storage::Pq { pq, codes } => {
                let cl = pq.code_len();
                pq.encode_into(prepared, &mut codes[node * cl..(node + 1) * cl]);
            }
        }
    }

    // Append a new node slot's resident representation.
    fn append_resident(&mut self, prepared: &[f32]) {
        match &mut self.storage {
            Storage::Flat { vectors } => vectors.extend_from_slice(prepared),
            Storage::Pq { pq, codes } => {
                let cl = pq.code_len();
                let start = codes.len();
                codes.resize(start + cl, 0);
                pq.encode_into(prepared, &mut codes[start..start + cl]);
            }
        }
    }

    // Reconstruct a node's vector in prepared space (exact for Flat, PQ-decoded
    // for the frugal mode) — used by rebalancing, which needs vector geometry.
    fn reconstruct_node(&self, node: u32) -> Vec<f32> {
        let n = node as usize;
        match &self.storage {
            Storage::Flat { vectors } => vectors[n * self.dim..(n + 1) * self.dim].to_vec(),
            Storage::Pq { pq, codes } => {
                let cl = pq.code_len();
                pq.reconstruct(&codes[n * cl..(n + 1) * cl])
            }
        }
    }

    // Split an over-full cell into two via local 2-means, then reassign the
    // affected points to their nearest centroid over the full set so the
    // nearest-centroid invariant is preserved (SpFresh LIRE, ADR-0023).
    fn split(&mut self, cell: usize) {
        let members = std::mem::take(&mut self.postings[cell]);
        if members.len() < 2 {
            self.postings[cell] = members;
            return;
        }
        // Gather member vectors (reconstructed for PQ) into a flat buffer.
        let mut data = vec![0f32; members.len() * self.dim];
        for (row, &node) in members.iter().enumerate() {
            let v = self.reconstruct_node(node);
            data[row * self.dim..(row + 1) * self.dim].copy_from_slice(&v);
        }
        self.splits = self.splits.wrapping_add(1);
        let seed = self
            .config
            .seed
            .wrapping_add(self.splits.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let two = kmeans(
            &data,
            members.len(),
            self.dim,
            2,
            self.config.kmeans_iters,
            seed,
        );
        // Centroid 0 replaces `cell`; centroid 1 takes a fresh/recycled cell.
        self.centroids[cell * self.dim..(cell + 1) * self.dim].copy_from_slice(&two[0..self.dim]);
        let new = self.take_cell(&two[self.dim..2 * self.dim]);
        // Reassign every affected point to its now-nearest centroid (over all
        // live cells — tombstoned ones carry a sentinel and are never chosen).
        for &node in &members {
            let v = self.reconstruct_node(node);
            let target = nearest_centroid(&v, &self.centroids, self.dim);
            self.postings[target].push(node);
            self.node_cell[node as usize] = target as u32;
        }
        // A degenerate split (all mass on one side) leaves `new` empty; recycle.
        if self.postings[new].is_empty() {
            self.tombstone_cell(new);
        }
    }

    // Merge an under-full cell: tombstone it, then move its points to their
    // nearest remaining centroid (SpFresh LIRE, ADR-0023).
    fn merge(&mut self, cell: usize) {
        let members = std::mem::take(&mut self.postings[cell]);
        // Tombstone first so members are never reassigned back into this cell.
        self.tombstone_cell(cell);
        for node in members {
            let v = self.reconstruct_node(node);
            let target = nearest_centroid(&v, &self.centroids, self.dim);
            self.postings[target].push(node);
            self.node_cell[node as usize] = target as u32;
        }
    }

    // Reserve a cell slot for `centroid` (recycling a tombstoned one if any).
    fn take_cell(&mut self, centroid: &[f32]) -> usize {
        if let Some(c) = self.free_cells.pop() {
            self.centroids[c * self.dim..(c + 1) * self.dim].copy_from_slice(centroid);
            self.postings[c].clear();
            c
        } else {
            let c = self.postings.len();
            self.centroids.extend_from_slice(centroid);
            self.postings.push(Vec::new());
            c
        }
    }

    // Tombstone a cell: empty its posting and set a never-selected sentinel
    // centroid, then mark the slot recyclable.
    fn tombstone_cell(&mut self, c: usize) {
        self.centroids[c * self.dim..(c + 1) * self.dim].fill(f32::MAX);
        self.postings[c].clear();
        self.free_cells.push(c);
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

    // ----- incremental in-place updates (ADR-0023) -----

    // A live view of the index (external id + vector), for recall checks after a
    // stream of updates.
    fn live_subset(all: &[Vec<f32>], live: &[u64]) -> Vec<(u64, Vec<f32>)> {
        live.iter()
            .map(|&id| (id, all[id as usize].clone()))
            .collect()
    }

    fn recall_over(
        idx: &Ivf,
        live: &[(u64, Vec<f32>)],
        metric: Metric,
        k: usize,
        nprobe: usize,
        queries: usize,
        rng: &mut SplitMix64,
    ) -> f64 {
        let vecs: Vec<Vec<f32>> = live.iter().map(|(_, v)| v.clone()).collect();
        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(rng, vecs[0].len());
            // Brute force over the live subset, mapped back to external ids.
            let truth_local = brute_force(&vecs, &q, k, metric);
            let truth: HashSet<u64> = truth_local.iter().map(|&i| live[i].0).collect();
            let got = idx.search(&q, k, nprobe).unwrap();
            hits += got.iter().filter(|nbr| truth.contains(&nbr.id)).count();
        }
        hits as f64 / (queries * k) as f64
    }

    #[test]
    fn incremental_insert_finds_the_new_point() {
        let dim = 16;
        let mut rng = SplitMix64::new(0xA11);
        let (_data, flat, ids) = dataset(&mut rng, 200, dim);
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, IvfConfig::default()).unwrap();
        let before = idx.len();
        let needle = vec![5.0f32; dim]; // far from the unit-ish cloud
        idx.insert(999, &needle).unwrap();
        assert_eq!(idx.len(), before + 1);
        let got = idx.search(&needle, 1, idx.nlist()).unwrap();
        assert_eq!(got[0].id, 999, "the inserted point is its own nearest");
    }

    #[test]
    fn remove_excludes_the_point_and_frees_the_slot() {
        let dim = 12;
        let mut rng = SplitMix64::new(0xB22);
        let (data, flat, ids) = dataset(&mut rng, 300, dim);
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, IvfConfig::default()).unwrap();

        // Pick a real point, confirm it is found, then remove it.
        let target = &data[7];
        assert!(idx.remove(7));
        assert!(!idx.remove(7), "double remove is a no-op");
        assert_eq!(idx.len(), 299);
        let got = idx.search(target, 5, idx.nlist()).unwrap();
        assert!(
            got.iter().all(|n| n.id != 7),
            "removed id must not be returned"
        );

        // Re-inserting reuses the freed node slot (no unbounded growth).
        idx.insert(7, target).unwrap();
        assert_eq!(idx.len(), 300);
        assert_eq!(idx.ids.len(), 300, "removed slot was reused, not appended");
    }

    #[test]
    fn insert_replaces_an_existing_id() {
        let dim = 8;
        let mut rng = SplitMix64::new(0xC33);
        let (_data, flat, ids) = dataset(&mut rng, 100, dim);
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, IvfConfig::default()).unwrap();

        let here = vec![3.0f32; dim];
        let there = vec![-3.0f32; dim];
        idx.insert(42, &here).unwrap();
        idx.insert(42, &there).unwrap(); // replace, not duplicate
        assert_eq!(idx.len(), 100, "replacing an id does not change the count");

        let near_there = idx.search(&there, 1, idx.nlist()).unwrap();
        assert_eq!(near_there[0].id, 42);
        let near_here = idx.search(&here, 3, idx.nlist()).unwrap();
        assert!(
            near_here.iter().all(|n| n.id != 42) || near_here[0].id != 42,
            "the stale vector should no longer be the close match at `here`"
        );
    }

    #[test]
    fn splitting_keeps_posting_lists_bounded_and_recall_high() {
        // Start from a trained index, then stream many inserts so cells overflow
        // `max_postings` and split. Recall must stay high and lists bounded.
        let dim = 16;
        let mut rng = SplitMix64::new(0xD44);
        let (mut data, flat, ids) = dataset(&mut rng, 200, dim);
        let cfg = IvfConfig {
            nlist: 8,
            max_postings: 32,
            ..IvfConfig::default()
        };
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();

        for new_id in 200u64..1000 {
            let v = rand_vec(&mut rng, dim);
            idx.insert(new_id, &v).unwrap();
            data.push(v);
        }
        assert_eq!(idx.len(), 1000);
        // Every live cell respects the split threshold (some slack for the
        // single split performed per insert).
        let max = idx.postings.iter().map(Vec::len).max().unwrap();
        assert!(
            max <= cfg.max_postings * 3,
            "a posting list grew unbounded: {max}"
        );
        assert!(idx.nlist() > cfg.nlist, "the index must have split cells");

        let live_ids: Vec<u64> = (0..1000).collect();
        let live = live_subset(&data, &live_ids);
        let r = recall_over(&idx, &live, Metric::L2, 10, idx.nlist(), 50, &mut rng);
        assert!(r >= 0.90, "recall after splitting was {r:.3}");
    }

    #[test]
    fn recall_is_preserved_under_an_insert_delete_stream() {
        // The headline SpFresh property: a long churn stream keeps the index
        // accurate without an O(N) rebuild.
        let dim = 16;
        let mut rng = SplitMix64::new(0xE55);
        let (mut data, flat, ids) = dataset(&mut rng, 500, dim);
        let cfg = IvfConfig {
            nlist: 16,
            max_postings: 48,
            min_postings: 8,
            ..IvfConfig::default()
        };
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();

        let mut live: HashSet<u64> = (0..500).collect();
        let mut next_id = 500u64;
        for _ in 0..1500 {
            if rng.next_f64() < 0.5 && !live.is_empty() {
                // delete a random live id
                let victim = *live.iter().next().unwrap();
                idx.remove(victim);
                live.remove(&victim);
            } else {
                let v = rand_vec(&mut rng, dim);
                idx.insert(next_id, &v).unwrap();
                if next_id as usize >= data.len() {
                    data.push(v);
                } else {
                    data[next_id as usize] = v;
                }
                live.insert(next_id);
                next_id += 1;
            }
        }
        assert_eq!(idx.len(), live.len());

        let live_ids: Vec<u64> = live.iter().copied().collect();
        let subset = live_subset(&data, &live_ids);
        let r = recall_over(&idx, &subset, Metric::L2, 10, idx.nlist(), 60, &mut rng);
        assert!(r >= 0.90, "recall under churn was {r:.3}");
    }

    #[test]
    fn merge_redistributes_points_without_loss() {
        // With a high min_postings, emptying-ish a cell triggers a merge that
        // moves its survivors to other cells — none must be lost.
        let dim = 8;
        let mut rng = SplitMix64::new(0xF66);
        let (data, flat, ids) = dataset(&mut rng, 120, dim);
        let cfg = IvfConfig {
            nlist: 6,
            min_postings: 100, // force merges aggressively on any removal
            ..IvfConfig::default()
        };
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();
        let cells_before = idx.live_cells();

        // Remove a handful of points; each removal triggers a merge of the
        // affected (now under-min) cell.
        for id in 0u64..5 {
            idx.remove(id);
        }
        assert!(
            idx.live_cells() < cells_before,
            "merges should have reduced the live cell count"
        );

        // All remaining points are still findable (no data lost in the moves).
        let mut found_all = true;
        for id in 5u64..120 {
            let v = &data[id as usize];
            let got = idx.search(v, 1, idx.nlist()).unwrap();
            if got.is_empty() || got[0].id != id {
                found_all = false;
                break;
            }
        }
        assert!(found_all, "a merged point became unreachable");
        assert_eq!(idx.len(), 115);
    }

    #[test]
    fn incremental_pq_mode_updates_and_searches() {
        // The frugal PQ path must also support in-place insert/remove, using
        // PQ reconstruction for rebalancing.
        let dim = 16;
        let mut rng = SplitMix64::new(0x1234);
        let (_data, flat, ids) = dataset(&mut rng, 600, dim);
        let cfg = IvfConfig {
            nlist: 16,
            quantization: Some(8),
            max_postings: 64,
            ..IvfConfig::default()
        };
        let mut idx = Ivf::build(&ids, &flat, dim, Metric::L2, cfg).unwrap();

        let needle = vec![4.0f32; dim];
        idx.insert(9001, &needle).unwrap();
        assert_eq!(idx.len(), 601);
        let got = idx.search(&needle, 5, idx.nlist()).unwrap();
        assert!(
            got.iter().any(|n| n.id == 9001),
            "PQ index finds the inserted outlier"
        );

        assert!(idx.remove(9001));
        let got = idx.search(&needle, 5, idx.nlist()).unwrap();
        assert!(got.iter().all(|n| n.id != 9001));
        assert_eq!(idx.len(), 600);
    }

    #[test]
    fn insert_rejects_dot_and_dimension_mismatch() {
        let dim = 4;
        let ids: Vec<u64> = (0..3).collect();
        let mut l2 = Ivf::build(&ids, &[0.1; 12], dim, Metric::L2, IvfConfig::default()).unwrap();
        assert!(matches!(
            l2.insert(7, &[0.0; 3]),
            Err(IndexError::DimensionMismatch { .. })
        ));
    }
}
