// SPDX-License-Identifier: AGPL-3.0-only
//! Lock-free MVCC serving snapshot: the immutable per-collection read view
//! (CollectionSnapshot) a lock-free reader loads (ADR-0064). Split out of the
//! crate root for review; the public API is unchanged (re-exported by `lib.rs`).
#![allow(clippy::wildcard_imports)]

use super::*;

// The single writer mutates indexes in place under the write lock, so a reader
// cannot share the live index by `Arc` and read it lock-free — that is a data
// race. Instead, in MVCC mode the writer publishes an immutable
// [`CollectionSnapshot`] (the base index as of the last rebuild + an [`Overlay`]
// of writes since) into an [`ArcSwap`]; readers `load()` it without a lock. The
// overlay is index-kind-agnostic and bounded by the rebuild cadence, so it never
// rewrites the index and a republish costs O(overlay), not O(index). MVCC changes
// **visibility, not durability**: the WAL-fsync acknowledgement and the `kill -9`
// crash gate are untouched (the overlay is derived from the same WAL the store
// already replays).

/// Per-collection lock-free serving snapshot pointer: the single writer
/// `store`s a new [`CollectionSnapshot`]; readers `load` one without a lock.
/// (`ArcSwap<T>` stores an `Arc<T>` internally, so this is one `Arc` per load.)
pub type SnapshotCell = Arc<ArcSwap<CollectionSnapshot>>;

/// Writes accumulated since a [`CollectionSnapshot`]'s base index was published.
/// A lock-free read brute-scans these recent vectors and merges them with the
/// base-index search, dropping tombstoned ids. Cloned (O(overlay)) on each write
/// and reset to empty when a rebuild folds it into a fresh base.
#[derive(Default, Clone)]
pub(crate) struct Overlay {
    // (vector, external id) for points upserted since the base, in id order: the
    // j-th entry's internal id is `base_len + j`.
    pub(crate) upserts: Vec<(Arc<[f32]>, String)>,
    // Internal ids (base or overlay) deleted or superseded since the base; their
    // hits are dropped from a search.
    pub(crate) tombstones: HashSet<u64>,
}

/// An immutable, lock-free-readable view of a single-vector collection (ADR-0064):
/// the base index as of the last rebuild, the base id map, and the overlay of
/// writes since. Obtained via [`Database::collection_snapshot`] and read with
/// [`CollectionSnapshot::search`]; a read is snapshot-isolated — it sees one
/// consistent `(base, overlay)` pair, and a write that lands mid-read is simply
/// the next snapshot.
pub struct CollectionSnapshot {
    pub(crate) base: Arc<CollectionIndex>,
    pub(crate) base_int_to_ext: Arc<Vec<String>>,
    pub(crate) base_len: u64,
    pub(crate) overlay: Arc<Overlay>,
    pub(crate) metric: Metric,
}

impl CollectionSnapshot {
    // An empty snapshot for a freshly created/opened collection (no base yet); the
    // writer publishes a real one at the first rebuild. Reading it yields no hits.
    pub(crate) fn empty(metric: Metric) -> Self {
        Self {
            base: Arc::new(CollectionIndex::None),
            base_int_to_ext: Arc::new(Vec::new()),
            base_len: 0,
            overlay: Arc::new(Overlay::default()),
            metric,
        }
    }

    // Map an internal id to its external id: base ids index the base map, overlay
    // ids (`>= base_len`) index the overlay's upserts.
    fn ext_id(&self, internal: u64) -> Option<&str> {
        if internal < self.base_len {
            self.base_int_to_ext
                .get(internal as usize)
                .map(String::as_str)
        } else {
            self.overlay
                .upserts
                .get((internal - self.base_len) as usize)
                .map(|(_, e)| e.as_str())
        }
    }

    /// Lock-free nearest-neighbor search over the base index merged with the
    /// overlay (ADR-0064 increment 1) — **pure vector reads**: no payload filter
    /// and no payload/vector fetch (those need the store and land in increment 2).
    /// Returns the `k` nearest live points, closest first, scored in the true
    /// collection metric — within recall tolerance of the locked
    /// [`Database::search_snapshot`] path for the same case. (Ordering and scores
    /// match exactly whenever the ANN base returns the same candidate set; the
    /// only divergence is the base index's own approximation, identical to what
    /// the locked path sees.)
    ///
    /// # Errors
    /// Propagates an index search error.
    pub fn search(&self, query: &[f32], k: usize, ef_search: usize) -> Result<Vec<Match>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        // Overlay tombstones that shadow *base* points (internal id < base_len) get
        // dropped from the base hits below, so asking the base for exactly k would
        // thin the result under a true top-k (up to the ~20% overlay churn cap).
        // Compensate by widening the base's k/ef by the live-fraction — the same
        // trick the base indexes' own soft-delete paths use (ADR-0064). Overlay
        // upserts (id >= base_len) are brute-scored below and need no widening.
        let base_tombstones = self
            .overlay
            .tombstones
            .iter()
            .filter(|&&id| id < self.base_len)
            .count() as u64;
        let (k_base, ef_base) = if base_tombstones == 0 || self.base_len == 0 {
            (k, ef_search)
        } else {
            let live_base = self.base_len.saturating_sub(base_tombstones).max(1);
            // n * base_len / live_base, capped at base_len, floored at n (never shrink).
            let widen = |n: usize| -> usize {
                (n as u64)
                    .saturating_mul(self.base_len)
                    .div_ceil(live_base)
                    .clamp(n as u64, self.base_len) as usize
            };
            let k_base = widen(k);
            (k_base, widen(ef_search).max(k_base))
        };
        // Collect candidates in "smaller is closer" ordering space (uniform across
        // metrics — `score::ordering_distance`), dropping tombstoned ids.
        let mut cands: Vec<(f32, u64)> = Vec::new();
        for n in self.base.search(query, k_base, ef_base)? {
            if !self.overlay.tombstones.contains(&n.id) {
                // `Neighbor.distance` is the reported metric; `report_metric` is its
                // own inverse (identity for L2, negation for similarities), so it
                // maps the reported value back to the ordering key.
                cands.push((report_metric(self.metric, n.distance), n.id));
            }
        }
        for (j, (vector, _)) in self.overlay.upserts.iter().enumerate() {
            let internal = self.base_len + j as u64;
            if !self.overlay.tombstones.contains(&internal) {
                cands.push((ordering_distance(self.metric, query, vector), internal));
            }
        }
        cands.sort_by(|a, b| a.0.total_cmp(&b.0));
        cands.truncate(k);
        let mut out = Vec::with_capacity(cands.len());
        for (ordering, internal) in cands {
            if let Some(ext) = self.ext_id(internal) {
                out.push(Match {
                    id: ext.to_owned(),
                    score: report_metric(self.metric, ordering),
                    payload: None,
                    vector: None,
                });
            }
        }
        Ok(out)
    }
}

// A fresh, empty snapshot cell for a collection's metric — the initial value of
// `CollectionHandle::snapshot` (the writer publishes a real base at the first
// rebuild when MVCC is on; left untouched, at one tiny allocation, when off).
pub(crate) fn empty_snapshot(descriptor: &Descriptor) -> SnapshotCell {
    let metric = to_index_metric(descriptor.metric);
    Arc::new(ArcSwap::from_pointee(CollectionSnapshot::empty(metric)))
}
