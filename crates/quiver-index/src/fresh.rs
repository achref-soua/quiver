// SPDX-License-Identifier: AGPL-3.0-only
//! FreshDiskANN-style incremental maintenance for the Vamana graph family
//! (ADR-0033; Singh et al. 2021).
//!
//! A graph index ([`Vamana`] in memory, [`DiskVamana`] on disk) is batch-built
//! and read-only. To absorb writes without rebuilding the whole graph on every
//! change, a "fresh" wrapper layers two cheap, in-memory structures over that
//! read-only **base**:
//!
//! - a small **delta graph** ([`Vamana`] grown with [`Vamana::insert`]) holding
//!   points inserted since the last consolidation, searched at full precision so
//!   a just-inserted vector is immediately findable, and
//! - a **deletion set** of base/delta ids that are filtered from results in
//!   `O(1)` per delete.
//!
//! A query searches the base and the delta, drops deleted ids, and merges the
//! two candidate lists by the metric's ordering. While tombstones are present
//! the search beam is widened by the live fraction (as HNSW does, ADR-0026) so
//! roughly the requested number of *live* candidates survive the filter.
//!
//! Consolidation (FreshDiskANN's StreamingMerge) is driven by the caller: when
//! the pending work ([`FreshVamana::pending_fraction`]) grows past a threshold,
//! `quiver-embed` rebuilds the consolidated base from the store's live rows,
//! resetting the delta and the deletion set. The whole structure stays in memory
//! and derived, so the durability path — and the `kill -9` crash gate — is
//! untouched by construction (ADR-0033).

use std::collections::HashSet;

use quiver_simd::Metric;

use crate::disk::DiskError;
use crate::{DiskSearchParams, DiskVamana, IndexError, Neighbor, Vamana, VamanaConfig};

// Recent inserts and tombstones layered over a read-only base graph. Ids are the
// caller's stable point ids (the embeddable layer's internal ids), shared by the
// base and the delta, so a returned neighbor's id is what the deletion set keys
// on regardless of which graph produced it.
struct GraphDelta {
    delta: Vamana,
    deleted: HashSet<u64>,
}

impl GraphDelta {
    fn new(dim: usize, metric: Metric) -> Result<Self, IndexError> {
        Ok(Self {
            delta: Vamana::new(dim, metric, VamanaConfig::default())?,
            deleted: HashSet::new(),
        })
    }

    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError> {
        self.delta.insert(id, vector)
    }

    fn mark_deleted(&mut self, id: u64) -> bool {
        self.deleted.insert(id)
    }

    // Work accumulated since the last consolidation: delta size plus tombstones.
    fn pending(&self) -> usize {
        self.delta.len() + self.deleted.len()
    }

    // The tombstoned ids, for a durable snapshot (ADR-0063). Order is unspecified.
    fn deleted_ids(&self) -> Vec<u64> {
        self.deleted.iter().copied().collect()
    }
}

// Ordering key with "smaller is closer" semantics, derived from a reported metric
// value: squared-L2 is already a distance; similarities (cosine/dot) are negated
// (mirrors `score::report_metric`). The graph family is L2/cosine only.
fn order_key(metric: Metric, distance: f32) -> f32 {
    match metric {
        Metric::L2 => distance,
        Metric::Dot | Metric::Cosine => -distance,
    }
}

// Widen the per-graph fetch so roughly `fetch` *live* candidates survive the
// tombstone filter (HNSW's rule, ADR-0026): scale by total/live, capped at the
// node count. With no tombstones the fetch is unchanged.
fn widened(fetch: usize, total: usize, deleted: usize) -> usize {
    if deleted == 0 || total == 0 {
        return fetch;
    }
    let live = total.saturating_sub(deleted).max(1);
    fetch.saturating_mul(total).div_ceil(live).min(total)
}

// Merge base and delta candidates: drop deleted ids, order closest-first under the
// metric, keep the best hit per id, and take `k`.
fn merge(
    metric: Metric,
    base_hits: Vec<Neighbor>,
    delta_hits: Vec<Neighbor>,
    deleted: &HashSet<u64>,
    k: usize,
) -> Vec<Neighbor> {
    let mut all: Vec<Neighbor> = base_hits
        .into_iter()
        .chain(delta_hits)
        .filter(|n| !deleted.contains(&n.id))
        .collect();
    all.sort_by(|a, b| {
        order_key(metric, a.distance)
            .total_cmp(&order_key(metric, b.distance))
            .then(a.id.cmp(&b.id))
    });
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(k);
    for n in all {
        if out.len() >= k {
            break;
        }
        if seen.insert(n.id) {
            out.push(n);
        }
    }
    out
}

/// A [`Vamana`] graph with FreshDiskANN incremental maintenance: a read-only base
/// graph plus an in-memory delta and deletion set (ADR-0033).
pub struct FreshVamana {
    base: Vamana,
    ext: GraphDelta,
}

impl FreshVamana {
    /// Wrap a batch-built `base` graph, ready to absorb inserts and deletes.
    ///
    /// # Errors
    /// Propagates [`Vamana::new`] (only [`Metric::Dot`] is rejected, which `base`
    /// already excludes).
    pub fn new(base: Vamana) -> Result<Self, IndexError> {
        let ext = GraphDelta::new(base.dim(), base.metric())?;
        Ok(Self { base, ext })
    }

    /// Insert a point into the delta graph (ADR-0033).
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] on a dimensionality mismatch.
    pub fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError> {
        self.ext.insert(id, vector)
    }

    /// Tombstone a point id so it is never returned (idempotent; returns whether
    /// it was newly tombstoned).
    pub fn mark_deleted(&mut self, id: u64) -> bool {
        self.ext.mark_deleted(id)
    }

    /// Points in the consolidated base graph (including not-yet-reclaimed
    /// tombstones).
    #[must_use]
    pub fn base_len(&self) -> usize {
        self.base.len()
    }

    /// Pending work (delta size + tombstones) as a fraction of the base size — the
    /// caller's consolidation trigger.
    #[must_use]
    pub fn pending_fraction(&self) -> f64 {
        self.ext.pending() as f64 / self.base.len().max(1) as f64
    }

    /// Search the base and the delta, drop tombstones, and return the `k` nearest
    /// live points. `l` is the base/delta beam width (widened while tombstones are
    /// present).
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] on a query dimensionality
    /// mismatch.
    pub fn search(&self, query: &[f32], k: usize, l: usize) -> Result<Vec<Neighbor>, IndexError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let total = self.base.len() + self.ext.delta.len();
        let want = widened(l.max(k), total, self.ext.deleted.len());
        let base_hits = self.base.search(query, want, want)?;
        let delta_hits = self.ext.delta.search(query, want, want)?;
        Ok(merge(
            self.base.metric(),
            base_hits,
            delta_hits,
            &self.ext.deleted,
            k,
        ))
    }
}

/// A [`DiskVamana`] index with FreshDiskANN incremental maintenance: the immutable,
/// `mmap`-ed on-disk graph stays the read-only base, and an in-memory delta and
/// deletion set absorb writes (ADR-0033) — so the on-disk artifact keeps its
/// write-once contract and the crash gate is untouched.
pub struct FreshDiskVamana {
    base: DiskVamana,
    ext: GraphDelta,
}

impl FreshDiskVamana {
    /// Wrap an opened `base` disk index, ready to absorb inserts and deletes.
    ///
    /// # Errors
    /// Propagates [`Vamana::new`] for the in-memory delta.
    pub fn new(base: DiskVamana) -> Result<Self, IndexError> {
        let ext = GraphDelta::new(base.dim(), base.metric())?;
        Ok(Self { base, ext })
    }

    /// Insert a point into the in-memory delta graph (ADR-0033).
    ///
    /// # Errors
    /// Returns [`IndexError::DimensionMismatch`] on a dimensionality mismatch.
    pub fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError> {
        self.ext.insert(id, vector)
    }

    /// Tombstone a point id so it is never returned (idempotent; returns whether
    /// it was newly tombstoned).
    pub fn mark_deleted(&mut self, id: u64) -> bool {
        self.ext.mark_deleted(id)
    }

    /// Points in the consolidated on-disk base graph (including not-yet-reclaimed
    /// tombstones).
    #[must_use]
    pub fn base_len(&self) -> usize {
        self.base.len()
    }

    /// The tombstoned ids, for a durable index snapshot (ADR-0063). Order is
    /// unspecified; callers persist and re-apply them via [`Self::mark_deleted`].
    #[must_use]
    pub fn deleted_ids(&self) -> Vec<u64> {
        self.ext.deleted_ids()
    }

    /// Pending work (delta size + tombstones) as a fraction of the base size — the
    /// caller's consolidation trigger.
    #[must_use]
    pub fn pending_fraction(&self) -> f64 {
        self.ext.pending() as f64 / self.base.len().max(1) as f64
    }

    /// Search the on-disk base and the in-memory delta, drop tombstones, and
    /// return the `k` nearest live points.
    ///
    /// # Errors
    /// Returns a [`DiskError`] on a dimensionality mismatch or an I/O / decrypt
    /// error reading a base node page.
    pub fn search(&self, query: &[f32], k: usize, l: usize) -> Result<Vec<Neighbor>, DiskError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let total = self.base.len() + self.ext.delta.len();
        let want = widened(l.max(k), total, self.ext.deleted.len());
        let base_hits = self
            .base
            .search(query, want, &DiskSearchParams { l_search: want })?;
        let delta_hits = self.ext.delta.search(query, want, want)?;
        Ok(merge(
            self.base.metric(),
            base_hits,
            delta_hits,
            &self.ext.deleted,
            k,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProductQuantizer;
    use crate::rng::SplitMix64;
    use quiver_core::page::PlainCodec;
    use std::collections::HashSet as Set;

    fn rand_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_f64() as f32) * 2.0 - 1.0)
            .collect()
    }

    // Ground-truth nearest live ids by brute force over the metric.
    fn brute_force(
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
            .map(|(i, v)| (crate::ordering_distance(metric, q, v), i as u64))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    // A workload: build a base over the first `split` points, stream the rest into
    // the delta, then tombstone every `del_step`-th id (base and delta alike).
    struct Stream {
        data: Vec<Vec<f32>>,
        live: Set<u64>,
        dim: usize,
        metric: Metric,
        split: usize,
    }

    fn make_stream(n: usize, dim: usize, metric: Metric, split: usize, del_step: usize) -> Stream {
        let mut rng = SplitMix64::new(0xF2E5 ^ n as u64);
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, dim)).collect();
        let live: Set<u64> = (0..n as u64).filter(|i| i % del_step as u64 != 0).collect();
        Stream {
            data,
            live,
            dim,
            metric,
            split,
        }
    }

    impl Stream {
        fn base_vamana(&self) -> Vamana {
            let ids: Vec<u64> = (0..self.split as u64).collect();
            let flat: Vec<f32> = self.data[..self.split].iter().flatten().copied().collect();
            Vamana::build(&ids, &flat, self.dim, self.metric, VamanaConfig::default()).unwrap()
        }

        fn base_disk(&self, dir: &std::path::Path) -> DiskVamana {
            let ids: Vec<u64> = (0..self.split as u64).collect();
            let flat: Vec<f32> = self.data[..self.split].iter().flatten().copied().collect();
            let graph =
                Vamana::build(&ids, &flat, self.dim, self.metric, VamanaConfig::default()).unwrap();
            let pq =
                ProductQuantizer::train(&flat, self.split, self.dim, self.dim / 4, self.metric, 7)
                    .unwrap();
            let path = dir.join("base.qvx");
            crate::disk::write(&path, &graph, &pq, &PlainCodec).unwrap();
            DiskVamana::open(&path, Box::new(PlainCodec)).unwrap()
        }

        // Apply the delta inserts and tombstones to a wrapper via the supplied
        // closures, then return (recall@k, any_deleted_returned) over the queries.
        fn evaluate(
            &self,
            k: usize,
            queries: usize,
            mut search: impl FnMut(&[f32], usize) -> Vec<Neighbor>,
        ) -> (f64, bool) {
            let mut rng = SplitMix64::new(0xA5A5);
            let mut hits = 0usize;
            let mut leaked = false;
            for _ in 0..queries {
                let q = rand_vec(&mut rng, self.dim);
                let truth = brute_force(&self.data, &self.live, &q, k, self.metric);
                let got = search(&q, k);
                if got.iter().any(|n| !self.live.contains(&n.id)) {
                    leaked = true;
                }
                hits += got.iter().filter(|n| truth.contains(&n.id)).count();
            }
            (hits as f64 / (queries * k) as f64, leaked)
        }
    }

    #[test]
    fn fresh_vamana_recall_under_insert_delete_stream() {
        let s = make_stream(1200, 32, Metric::L2, 600, 5);
        let mut fresh = FreshVamana::new(s.base_vamana()).unwrap();
        for i in s.split..s.data.len() {
            fresh.insert(i as u64, &s.data[i]).unwrap();
        }
        for i in (0..s.data.len() as u64).filter(|i| !s.live.contains(i)) {
            fresh.mark_deleted(i);
        }
        let (recall, leaked) = s.evaluate(10, 60, |q, k| fresh.search(q, k, 64).unwrap());
        assert!(!leaked, "a tombstoned id was returned");
        assert!(recall >= 0.90, "fresh Vamana recall@10 was {recall:.3}");
    }

    #[test]
    fn fresh_vamana_cosine_stream() {
        let s = make_stream(900, 24, Metric::Cosine, 450, 6);
        let mut fresh = FreshVamana::new(s.base_vamana()).unwrap();
        for i in s.split..s.data.len() {
            fresh.insert(i as u64, &s.data[i]).unwrap();
        }
        for i in (0..s.data.len() as u64).filter(|i| !s.live.contains(i)) {
            fresh.mark_deleted(i);
        }
        let (recall, leaked) = s.evaluate(10, 40, |q, k| fresh.search(q, k, 64).unwrap());
        assert!(!leaked);
        assert!(recall >= 0.88, "fresh cosine recall@10 was {recall:.3}");
    }

    #[test]
    fn fresh_disk_vamana_recall_under_insert_delete_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let s = make_stream(1000, 32, Metric::L2, 500, 5);
        let mut fresh = FreshDiskVamana::new(s.base_disk(tmp.path())).unwrap();
        for i in s.split..s.data.len() {
            fresh.insert(i as u64, &s.data[i]).unwrap();
        }
        for i in (0..s.data.len() as u64).filter(|i| !s.live.contains(i)) {
            fresh.mark_deleted(i);
        }
        let (recall, leaked) = s.evaluate(10, 50, |q, k| fresh.search(q, k, 100).unwrap());
        assert!(!leaked, "a tombstoned id was returned from the disk base");
        // PQ navigation in the disk base loses a little, recovered by exact re-rank.
        assert!(recall >= 0.85, "fresh disk recall@10 was {recall:.3}");
    }

    #[test]
    fn empty_base_then_delta_only() {
        // A graph that starts empty (base built over zero points) serves entirely
        // from the delta.
        let mut rng = SplitMix64::new(0xE3);
        let base = Vamana::build(&[], &[], 8, Metric::L2, VamanaConfig::default()).unwrap();
        let mut fresh = FreshVamana::new(base).unwrap();
        let data: Vec<Vec<f32>> = (0..200).map(|_| rand_vec(&mut rng, 8)).collect();
        for (i, v) in data.iter().enumerate() {
            fresh.insert(i as u64, v).unwrap();
        }
        let top = fresh.search(&data[42], 1, 50).unwrap();
        assert_eq!(top[0].id, 42);
    }

    #[test]
    fn deleted_then_reinserted_id_is_visible_via_delta() {
        // An update is modelled as tombstone-old + insert-new under a fresh id; the
        // tombstoned base copy is filtered while the delta copy is returned.
        let mut rng = SplitMix64::new(0x99);
        let n = 300;
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, 16)).collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let base = Vamana::build(&ids, &flat, 16, Metric::L2, VamanaConfig::default()).unwrap();
        let mut fresh = FreshVamana::new(base).unwrap();

        // "Update" id 42: tombstone the base copy, insert a moved vector under a new id.
        fresh.mark_deleted(42);
        let mut moved = data[42].clone();
        moved[0] += 0.001;
        let new_id = n as u64;
        fresh.insert(new_id, &moved).unwrap();

        let got = fresh.search(&data[42], 3, 64).unwrap();
        assert!(got.iter().all(|m| m.id != 42), "stale copy still returned");
        assert_eq!(got[0].id, new_id, "updated copy not nearest");
    }

    #[test]
    fn pending_fraction_tracks_writes() {
        let mut rng = SplitMix64::new(0x1234);
        let n = 100;
        let ids: Vec<u64> = (0..n as u64).collect();
        let flat: Vec<f32> = (0..n * 8).map(|_| rng.next_f64() as f32).collect();
        let base = Vamana::build(&ids, &flat, 8, Metric::L2, VamanaConfig::default()).unwrap();
        let mut fresh = FreshVamana::new(base).unwrap();
        assert_eq!(fresh.base_len(), 100);
        assert!(fresh.pending_fraction() < 1e-9);
        for i in 0..10u64 {
            fresh.insert(n as u64 + i, &[0.0; 8]).unwrap();
        }
        fresh.mark_deleted(0);
        fresh.mark_deleted(1);
        // 10 inserts + 2 tombstones over a 100-point base.
        assert!((fresh.pending_fraction() - 0.12).abs() < 1e-9);
    }
}
