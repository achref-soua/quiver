// SPDX-License-Identifier: AGPL-3.0-only
//! The embeddable, in-process Quiver database handle.
//!
//! [`Database`] composes the storage engine ([`quiver_core::Store`]) with a
//! per-collection vector index and payload filtering ([`quiver_query::Filter`])
//! into one handle. It exposes the same logical operations the server speaks
//! (`docs/api/wire-protocol.md`), so library mode and server mode exercise
//! identical engine semantics — the server is a thin transport/policy shell.
//!
//! ## Index lifecycle
//! The store is the source of truth. Each collection chooses its index via the
//! descriptor's [`IndexSpec`] (default in-memory HNSW); the index is built from
//! the store on open. HNSW applies new-id inserts incrementally; once an IVF
//! index is built it applies inserts, in-place updates, and deletes
//! incrementally with LIRE rebalancing (ADR-0023). An update to HNSW, or any
//! write to the Vamana / disk graph (built over the whole collection), marks the
//! index stale and the next search rebuilds it — so those suit
//! bulk-load-then-query. In-place incremental update for the disk graph is a
//! later increment (SpFresh).
//!
//! ## Filtered (hybrid) search
//! A search may carry a [`quiver_query::Filter`] over the payload. The planner
//! decomposes it into the predicates the collection's secondary indexes can
//! answer; when those narrow the query to a small candidate set it scans that
//! set exactly (perfect recall, no filtered-ANN cliff), and otherwise it
//! over-fetches from the ANN index and post-filters. Both arms re-check the full
//! filter, so results are exact regardless of which path runs.
//!
//! ## Concurrency (Phase 1)
//! Single-writer: every operation takes `&mut self` (a search may rebuild a
//! stale index). A server serializes access behind a lock; the lock-free MVCC
//! snapshot model (ADR-0006) arrives with Phase 2.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use quiver_core::{CollectionId, SecPredicate, SecValue, Store};
use quiver_index::{
    DiskSearchParams, DiskVamana, Hnsw, HnswConfig, Index, Ivf, IvfConfig, Metric, Neighbor,
    ProductQuantizer, Vamana, VamanaConfig, ordering_distance, report_metric,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use quiver_core::keyring::{KeyRing, SingleCodecKeyRing};
pub use quiver_core::page::PageCodec;
pub use quiver_core::{
    Descriptor, DistanceMetric, Dtype, FieldType, FilterableField, IndexKind, IndexSpec,
};
pub use quiver_query::Filter;

/// Errors returned by the embeddable database.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from the storage engine (includes not-found / already-exists /
    /// invalid-argument from the catalog and write path).
    #[error(transparent)]
    Core(#[from] quiver_core::CoreError),
    /// An error from the vector index.
    #[error(transparent)]
    Index(#[from] quiver_index::IndexError),
    /// An error from the disk-resident index (build, open, or query).
    #[error(transparent)]
    Disk(#[from] quiver_index::DiskError),
    /// A payload could not be (de)serialized as JSON.
    #[error("payload json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The named collection is not loaded in this database.
    #[error("collection not found: {0}")]
    CollectionNotFound(String),
    /// The requested index / metric combination is not supported.
    #[error("unsupported configuration: {0}")]
    Unsupported(&'static str),
    /// A durable index snapshot could not be restored (ADR-0025); the caller
    /// falls back to rebuilding from the store, so this does not surface to users.
    #[error(transparent)]
    IndexSnapshot(#[from] quiver_index::SnapshotError),
    /// An index snapshot envelope could not be (de)serialized.
    #[error("index snapshot envelope: {0}")]
    Envelope(#[from] postcard::Error),
}

/// Result alias for database operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A single search or fetch result.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// External id of the point.
    pub id: String,
    /// Distance / similarity under the collection metric (0 for a direct fetch).
    pub score: f32,
    /// The payload, if requested.
    pub payload: Option<Value>,
    /// The vector, if requested.
    pub vector: Option<Vec<f32>>,
}

/// Parameters for a [`Database::search`].
#[derive(Debug, Clone)]
pub struct SearchParams {
    /// Number of results to return.
    pub k: usize,
    /// Optional payload predicate. The planner pre-filters through the
    /// collection's secondary indexes when the filter is selective enough, and
    /// otherwise post-filters the ANN candidates; either way the full predicate
    /// is re-checked, so results are exact.
    pub filter: Option<Filter>,
    /// Search beam width (recall/latency knob), clamped up to at least `k`.
    pub ef_search: usize,
    /// Include payloads in the results.
    pub with_payload: bool,
    /// Include vectors in the results.
    pub with_vector: bool,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            k: 10,
            filter: None,
            ef_search: 64,
            with_payload: true,
            with_vector: false,
        }
    }
}

// How many extra candidates to pull before post-filtering, so a filtered query
// still has enough survivors to fill `k`.
const FILTER_OVERFETCH: usize = 8;

// Selectivity threshold for the hybrid planner. When a payload filter decomposes
// into secondary-index predicates that narrow a query to at most this many live
// candidate rows, those rows are scanned exactly (brute force) instead of going
// through the ANN index. Below this size an exact scan is both cheaper and
// higher-recall than filtered ANN, which can return too few results when the
// filter is very selective (the "filtered-search recall cliff"). Qdrant calls
// the equivalent knob its full-scan threshold.
const FULL_SCAN_THRESHOLD: usize = 10_000;

// Soft-deleted fraction at which a built HNSW is rebuilt from the store to
// reclaim its tombstoned graph nodes (ADR-0026). Below it, a delete is an O(1)
// soft-delete; at it, the next access rebuilds.
const HNSW_REBUILD_DELETED_FRACTION: f64 = 0.2;

// The vector index backing one collection. HNSW and (once built) IVF are
// maintained incrementally; Vamana and the disk graph are batch-built from the
// store (the `Option` is `None` until first build) and rebuild lazily after
// writes, so they suit bulk-load-then-query.
enum CollectionIndex {
    Hnsw(Hnsw),
    Vamana(Option<Vamana>),
    Ivf(Option<Ivf>),
    // The disk-resident DiskANN index: PQ codes in RAM, graph + full vectors on
    // (encrypted) SSD, exact re-rank (ADR-0019).
    Disk(Option<DiskVamana>),
}

impl CollectionIndex {
    // Search, mapping the generic `ef` knob onto each index's search width.
    fn search(&self, query: &[f32], k: usize, ef: usize) -> Result<Vec<Neighbor>> {
        Ok(match self {
            CollectionIndex::Hnsw(h) => h.search(query, k, ef)?,
            CollectionIndex::Vamana(Some(g)) => g.search(query, k, ef)?,
            CollectionIndex::Ivf(Some(i)) => i.search(query, k, ef)?,
            CollectionIndex::Disk(Some(d)) => {
                d.search(query, k, &DiskSearchParams { l_search: ef })?
            }
            CollectionIndex::Vamana(None)
            | CollectionIndex::Ivf(None)
            | CollectionIndex::Disk(None) => Vec::new(),
        })
    }
}

/// On-disk envelope (ADR-0025) for a durable IVF snapshot: the `Ivf` bytes plus
/// the internal->external id mapping they are addressed by, postcard-encoded and
/// handed to the store as one opaque blob. On open the envelope is decoded, the
/// `Ivf` restored, and the post-checkpoint WAL tail replayed. A decode/version
/// error means "rebuild from the store" — the snapshot is only ever a fast path.
#[derive(Serialize, Deserialize)]
struct IndexEnvelope {
    version: u16,
    int_to_ext: Vec<String>,
    ivf: Vec<u8>,
}

// Envelope format version, independent of the product SemVer (and of the inner
// `Ivf` snapshot version); a mismatch falls back to a rebuild.
const INDEX_ENVELOPE_VERSION: u16 = 1;

struct CollectionHandle {
    id: CollectionId,
    descriptor: Descriptor,
    index: CollectionIndex,
    int_to_ext: Vec<String>,
    ext_to_int: HashMap<String, u64>,
    stale: bool,
}

/// An in-process Quiver database over one data directory.
pub struct Database {
    store: Store,
    collections: HashMap<String, CollectionHandle>,
}

impl Database {
    /// Open (creating if absent) the database at `dir` with encryption-at-rest
    /// disabled, rebuilding each collection's index from the store.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::from_store(Store::open(dir)?)
    }

    /// Open the database with a specific page codec — used to enable
    /// encryption-at-rest by passing `quiver-crypto`'s AEAD codec. Mirrors
    /// [`quiver_core::Store::open_with_codec`]; the codec seals both paged files
    /// and the WAL, so no plaintext user data reaches the disk.
    pub fn open_with_codec(dir: &Path, codec: Box<dyn PageCodec>) -> Result<Self> {
        Self::from_store(Store::open_with_codec(dir, codec)?)
    }

    /// Open the database with a [`KeyRing`], the seam that lets `quiver-crypto`'s
    /// envelope key-ring seal each collection under its own data-encryption key
    /// (enabling crypto-shredding). Mirrors
    /// [`quiver_core::Store::open_with_keyring`].
    pub fn open_with_keyring(dir: &Path, keyring: Box<dyn KeyRing>) -> Result<Self> {
        Self::from_store(Store::open_with_keyring(dir, keyring)?)
    }

    // Build the in-memory handles (and their HNSW indexes) over an opened store.
    fn from_store(store: Store) -> Result<Self> {
        let mut collections = HashMap::new();
        for name in store.collection_names() {
            let Some(id) = store.collection_id(&name) else {
                continue;
            };
            let Some(descriptor) = store.descriptor(id).cloned() else {
                continue;
            };
            let mut handle = CollectionHandle {
                id,
                index: empty_index(&descriptor),
                descriptor,
                int_to_ext: Vec::new(),
                ext_to_int: HashMap::new(),
                stale: true,
            };
            load_index(&store, &mut handle)?;
            collections.insert(name, handle);
        }
        Ok(Self { store, collections })
    }

    /// Create a collection. Errors if the name already exists, or if the index
    /// specification is unsupported for the metric.
    pub fn create_collection(&mut self, name: &str, descriptor: Descriptor) -> Result<()> {
        validate_index(&descriptor)?;
        let id = self.store.create_collection(name, descriptor.clone())?;
        let index = empty_index(&descriptor);
        self.collections.insert(
            name.to_owned(),
            CollectionHandle {
                id,
                descriptor,
                index,
                int_to_ext: Vec::new(),
                ext_to_int: HashMap::new(),
                stale: false,
            },
        );
        Ok(())
    }

    /// Drop a collection and its data. Returns whether it existed.
    pub fn drop_collection(&mut self, name: &str) -> Result<bool> {
        let existed = self.store.drop_collection(name)?;
        self.collections.remove(name);
        Ok(existed)
    }

    /// Crypto-shred a collection: drop it and destroy its data-encryption key, so
    /// its sealed data is unrecoverable even with the master key, then reclaim
    /// its files. Mirrors [`quiver_core::Store::shred_collection`]; with an
    /// envelope key-ring this is irreversible erasure, with a single-codec
    /// key-ring it is `drop` plus a checkpoint. Returns whether it existed.
    pub fn shred_collection(&mut self, name: &str) -> Result<bool> {
        let existed = self.store.shred_collection(name)?;
        self.collections.remove(name);
        Ok(existed)
    }

    /// Names of all collections, sorted.
    #[must_use]
    pub fn collection_names(&self) -> Vec<String> {
        self.store.collection_names()
    }

    /// The descriptor of a collection, if it exists.
    #[must_use]
    pub fn descriptor(&self, name: &str) -> Option<&Descriptor> {
        self.collections.get(name).map(|h| &h.descriptor)
    }

    /// Number of live points in a collection.
    pub fn len(&self, name: &str) -> Result<usize> {
        let handle = self.handle(name)?;
        Ok(self.store.len(handle.id)?)
    }

    /// Whether a collection has no points.
    pub fn is_empty(&self, name: &str) -> Result<bool> {
        Ok(self.len(name)? == 0)
    }

    /// Insert or replace a point with a JSON payload.
    pub fn upsert(
        &mut self,
        collection: &str,
        id: &str,
        vector: &[f32],
        payload: &Value,
    ) -> Result<()> {
        let handle = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
        let payload_bytes = serde_json::to_vec(payload)?;
        self.store.upsert(handle.id, id, vector, &payload_bytes)?;
        // Maintain the in-memory index in place where the kind allows it, else
        // defer to a lazy rebuild on the next search. HNSW absorbs a brand-new id
        // (it cannot update one in place); a built/trained IVF inserts or
        // replaces any id (ADR-0023). A batch graph, the disk index, an
        // unbuilt/empty index, or an already-stale handle rebuilds instead.
        if handle.stale {
            return Ok(());
        }
        let known = handle.ext_to_int.contains_key(id);
        let is_hnsw = matches!(handle.index, CollectionIndex::Hnsw(_));
        let is_live_ivf =
            matches!(&handle.index, CollectionIndex::Ivf(Some(ivf)) if !ivf.is_empty());
        if is_hnsw && !known {
            let internal = handle.int_to_ext.len() as u64;
            if let CollectionIndex::Hnsw(h) = &mut handle.index {
                h.insert(internal, vector)?;
            }
            handle.ext_to_int.insert(id.to_owned(), internal);
            handle.int_to_ext.push(id.to_owned());
        } else if is_live_ivf {
            // Reuse the internal id for an in-place update; allocate a fresh,
            // dense one for a new id (so `int_to_ext` stays index-addressable).
            let internal = if known {
                handle.ext_to_int[id]
            } else {
                let i = handle.int_to_ext.len() as u64;
                handle.ext_to_int.insert(id.to_owned(), i);
                handle.int_to_ext.push(id.to_owned());
                i
            };
            if let CollectionIndex::Ivf(Some(ivf)) = &mut handle.index {
                ivf.insert(internal, vector)?;
            }
        } else {
            handle.stale = true;
        }
        Ok(())
    }

    /// Delete a point by id. Returns whether it existed.
    pub fn delete(&mut self, collection: &str, id: &str) -> Result<bool> {
        let handle = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
        let existed = self.store.delete(handle.id, id)?;
        if !existed {
            return Ok(false);
        }
        // A built IVF removes in place (ADR-0023) and a built HNSW soft-deletes
        // (ADR-0026); other kinds defer to a rebuild. The id->internal mapping is
        // kept so a later re-insert reuses the slot — a removed or soft-deleted
        // internal is simply never returned by the index.
        let internal = handle.ext_to_int.get(id).copied();
        let live_ivf = !handle.stale && matches!(handle.index, CollectionIndex::Ivf(Some(_)));
        let live_hnsw = !handle.stale && matches!(handle.index, CollectionIndex::Hnsw(_));
        match internal {
            Some(internal) if live_ivf => {
                if let CollectionIndex::Ivf(Some(ivf)) = &mut handle.index {
                    ivf.remove(internal);
                }
            }
            Some(internal) if live_hnsw => {
                let mut crowded = false;
                if let CollectionIndex::Hnsw(h) = &mut handle.index {
                    h.mark_deleted(internal as u32);
                    crowded = h.deleted_fraction() >= HNSW_REBUILD_DELETED_FRACTION;
                }
                if crowded {
                    handle.stale = true;
                }
            }
            _ => handle.stale = true,
        }
        Ok(true)
    }

    /// Fetch a single point by id, with its payload and vector.
    pub fn get(&self, collection: &str, id: &str) -> Result<Option<Match>> {
        let handle = self.handle(collection)?;
        match self.store.get(handle.id, id)? {
            Some(record) => Ok(Some(Match {
                id: id.to_owned(),
                score: 0.0,
                payload: Some(serde_json::from_slice(&record.payload)?),
                vector: Some(record.vector),
            })),
            None => Ok(None),
        }
    }

    /// Search a collection for the nearest points to `query`, optionally
    /// post-filtered by payload predicate.
    pub fn search(
        &mut self,
        collection: &str,
        query: &[f32],
        params: &SearchParams,
    ) -> Result<Vec<Match>> {
        // Rebuild the index first if a prior update/delete left it stale.
        if self.handle(collection)?.stale {
            let store = &self.store;
            let handle = self
                .collections
                .get_mut(collection)
                .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
            rebuild_index(store, handle)?;
        }

        let handle = self.handle(collection)?;

        // Hybrid planning: if the filter narrows to a small, secondary-indexed
        // candidate set, scan those rows exactly instead of post-filtering ANN
        // hits — exact, and immune to the filtered-ANN recall cliff.
        if let Some(filter) = &params.filter
            && let Some(candidates) = candidate_ids(
                &self.store,
                handle.id,
                filter,
                &handle.descriptor.filterable,
            )?
            && candidates.len() <= FULL_SCAN_THRESHOLD
        {
            return self.exact_filtered_search(
                handle.id,
                &handle.descriptor,
                query,
                params,
                filter,
                &candidates,
            );
        }

        let fetch = if params.filter.is_some() {
            params
                .k
                .saturating_mul(FILTER_OVERFETCH)
                .max(params.ef_search)
        } else {
            params.k
        };
        let raw = handle.index.search(query, fetch, params.ef_search)?;

        let need_record = params.filter.is_some() || params.with_payload || params.with_vector;
        let mut out = Vec::with_capacity(params.k);
        for neighbor in raw {
            if out.len() >= params.k {
                break;
            }
            let Some(ext_id) = handle.int_to_ext.get(neighbor.id as usize) else {
                continue;
            };
            let record = if need_record {
                self.store.get(handle.id, ext_id)?
            } else {
                None
            };
            let payload_value: Option<Value> = match &record {
                Some(r) if params.filter.is_some() || params.with_payload => {
                    Some(serde_json::from_slice(&r.payload)?)
                }
                _ => None,
            };
            if let Some(filter) = &params.filter {
                let value = payload_value.as_ref().unwrap_or(&Value::Null);
                if !filter.matches(value) {
                    continue;
                }
            }
            out.push(Match {
                id: ext_id.clone(),
                score: neighbor.distance,
                payload: if params.with_payload {
                    payload_value
                } else {
                    None
                },
                vector: if params.with_vector {
                    record.map(|r| r.vector)
                } else {
                    None
                },
            });
        }
        Ok(out)
    }

    // Exactly score `candidates` (a superset of the filter's matches that the
    // secondary indexes produced) against the query, re-check the full filter
    // for correctness, and return the top `k`. The pre-filter arm of the hybrid
    // planner: perfect recall over an already-narrowed set.
    fn exact_filtered_search(
        &self,
        cid: CollectionId,
        descriptor: &Descriptor,
        query: &[f32],
        params: &SearchParams,
        filter: &Filter,
        candidates: &BTreeSet<String>,
    ) -> Result<Vec<Match>> {
        let metric = to_index_metric(descriptor.metric);
        let mut scored: Vec<(f32, String, Value, Vec<f32>)> = Vec::new();
        for ext_id in candidates {
            let Some(record) = self.store.get(cid, ext_id)? else {
                continue;
            };
            let payload: Value = serde_json::from_slice(&record.payload)?;
            if !filter.matches(&payload) {
                continue;
            }
            let ordering = ordering_distance(metric, query, &record.vector);
            scored.push((ordering, ext_id.clone(), payload, record.vector));
        }
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.truncate(params.k);
        Ok(scored
            .into_iter()
            .map(|(ordering, id, payload, vector)| Match {
                id,
                score: report_metric(metric, ordering),
                payload: params.with_payload.then_some(payload),
                vector: params.with_vector.then_some(vector),
            })
            .collect())
    }

    /// Flush a durable checkpoint of all collections, capturing a durable
    /// snapshot of each built, up-to-date IVF index (ADR-0025) so it reloads on
    /// open instead of rebuilding. Other index kinds, and a stale or unbuilt IVF,
    /// are rebuilt on open.
    pub fn checkpoint(&mut self) -> Result<()> {
        let mut snapshots: HashMap<CollectionId, Vec<u8>> = HashMap::new();
        for handle in self.collections.values() {
            if handle.stale {
                continue;
            }
            if let CollectionIndex::Ivf(Some(ivf)) = &handle.index {
                if ivf.is_empty() {
                    continue;
                }
                let envelope = IndexEnvelope {
                    version: INDEX_ENVELOPE_VERSION,
                    int_to_ext: handle.int_to_ext.clone(),
                    ivf: ivf.snapshot()?,
                };
                snapshots.insert(handle.id, postcard::to_allocvec(&envelope)?);
            }
        }
        self.store.checkpoint_with_index_snapshots(&snapshots)?;
        Ok(())
    }

    /// Compact every collection with reclaimable space, merging its sealed
    /// segments and dropping deleted/shadowed rows. Crash-safe; a no-op for
    /// collections with nothing to reclaim.
    pub fn compact(&mut self) -> Result<()> {
        Ok(self.store.compact()?)
    }

    fn handle(&self, name: &str) -> Result<&CollectionHandle> {
        self.collections
            .get(name)
            .ok_or_else(|| Error::CollectionNotFound(name.to_owned()))
    }
}

fn to_index_metric(metric: DistanceMetric) -> Metric {
    match metric {
        DistanceMetric::Dot => Metric::Dot,
        DistanceMetric::Cosine => Metric::Cosine,
        DistanceMetric::L2 => Metric::L2,
    }
}

// Reject index/metric combinations the engine cannot serve.
fn validate_index(descriptor: &Descriptor) -> Result<()> {
    match descriptor.index.kind {
        IndexKind::Vamana | IndexKind::Ivf | IndexKind::DiskVamana
            if descriptor.metric == DistanceMetric::Dot =>
        {
            Err(Error::Unsupported(
                "vamana, ivf, and the disk index support l2 and cosine; use hnsw for dot",
            ))
        }
        _ => Ok(()),
    }
}

// An empty index of the kind the descriptor selects. Batch kinds start unbuilt.
fn empty_index(descriptor: &Descriptor) -> CollectionIndex {
    match descriptor.index.kind {
        IndexKind::Vamana => CollectionIndex::Vamana(None),
        IndexKind::DiskVamana => CollectionIndex::Disk(None),
        IndexKind::Ivf => CollectionIndex::Ivf(None),
        _ => CollectionIndex::Hnsw(Hnsw::new(
            descriptor.dim as usize,
            to_index_metric(descriptor.metric),
            HnswConfig::default(),
        )),
    }
}

// A product-quantization subspace count that divides `dim`, targeting roughly
// eight dimensions per subspace; falls back to one whole-vector codebook.
fn default_pq_m(dim: usize) -> usize {
    let target = (dim / 8).max(1);
    (1..=target)
        .rev()
        .find(|&m| dim.is_multiple_of(m))
        .unwrap_or(1)
}

// Build the descriptor's index over `ids` (internal 0..n) and their flat vectors.
// Fixed seed for codebook training, so a collection's disk index is reproducible.
const PQ_SEED: u64 = 0x5176_5044_5141_5453;
// The disk index artifact, overwritten in place each rebuild; the caller drops
// the previous handle (unmapping the file) first.
const DISK_INDEX_FILE: &str = "vamana.qvx";

fn build_index(
    store: &Store,
    cid: CollectionId,
    descriptor: &Descriptor,
    ids: &[u64],
    flat: &[f32],
) -> Result<CollectionIndex> {
    let dim = descriptor.dim as usize;
    let metric = to_index_metric(descriptor.metric);
    Ok(match descriptor.index.kind {
        IndexKind::Vamana => CollectionIndex::Vamana(Some(Vamana::build(
            ids,
            flat,
            dim,
            metric,
            VamanaConfig::default(),
        )?)),
        IndexKind::DiskVamana => {
            CollectionIndex::Disk(Some(build_disk_index(store, cid, descriptor, ids, flat)?))
        }
        IndexKind::Ivf => {
            let cfg = IvfConfig {
                quantization: descriptor.index.pq_subspaces.map(|m| m as usize),
                ..IvfConfig::default()
            };
            CollectionIndex::Ivf(Some(Ivf::build(ids, flat, dim, metric, cfg)?))
        }
        _ => {
            let mut h = Hnsw::new(dim, metric, HnswConfig::default());
            for (i, &id) in ids.iter().enumerate() {
                h.insert(id, &flat[i * dim..(i + 1) * dim])?;
            }
            CollectionIndex::Hnsw(h)
        }
    })
}

// Build the Vamana graph + PQ codebook, write the encrypted disk artifact under
// the collection's index dir with the store's codec, and open it for queries.
fn build_disk_index(
    store: &Store,
    cid: CollectionId,
    descriptor: &Descriptor,
    ids: &[u64],
    flat: &[f32],
) -> Result<DiskVamana> {
    let dim = descriptor.dim as usize;
    let metric = to_index_metric(descriptor.metric);
    let graph = Vamana::build(ids, flat, dim, metric, VamanaConfig::default())?;
    let m = descriptor
        .index
        .pq_subspaces
        .map_or_else(|| default_pq_m(dim), |x| x as usize);
    let pq = ProductQuantizer::train(flat, ids.len(), dim, m, metric, PQ_SEED)?;
    let dir = store.index_dir(cid);
    std::fs::create_dir_all(&dir).map_err(quiver_index::DiskError::Io)?;
    let path = dir.join(DISK_INDEX_FILE);
    // Seal the index artifact with the collection's own codec (its DEK under an
    // envelope key-ring), so a crypto-shred of the collection also makes its
    // index unreadable. The same owned handle writes and then mmap-opens it.
    let codec = store.collection_codec_clone(cid)?;
    quiver_index::disk::write(&path, &graph, &pq, codec.as_ref())?;
    Ok(DiskVamana::open(&path, codec)?)
}

// Load a collection's index on open: restore the durable IVF snapshot and replay
// the post-checkpoint tail (ADR-0025) when one is present and intact, otherwise
// rebuild from the store. The snapshot is only a fast path — any problem reading
// or restoring it falls back to the authoritative rebuild.
fn load_index(store: &Store, handle: &mut CollectionHandle) -> Result<()> {
    if handle.descriptor.index.kind == IndexKind::Ivf
        && let Ok(Some(blob)) = store.read_index_snapshot(handle.id)
        && restore_ivf_snapshot(store, handle, &blob).is_ok()
    {
        return Ok(());
    }
    rebuild_index(store, handle)
}

// Restore an IVF from its snapshot envelope and catch it up to the store's
// current state by replaying the post-checkpoint tail (ADR-0025). Tombstoned ids
// are removed before the active upserts are applied, so a row shadowed this
// window (present in both) ends with its new vector.
fn restore_ivf_snapshot(store: &Store, handle: &mut CollectionHandle, blob: &[u8]) -> Result<()> {
    let envelope: IndexEnvelope = postcard::from_bytes(blob)?;
    if envelope.version != INDEX_ENVELOPE_VERSION {
        return Err(Error::Unsupported(
            "unsupported index snapshot envelope version",
        ));
    }
    let ivf = Ivf::restore(&envelope.ivf)?;
    handle.ext_to_int = envelope
        .int_to_ext
        .iter()
        .enumerate()
        .map(|(i, ext)| (ext.clone(), i as u64))
        .collect();
    handle.int_to_ext = envelope.int_to_ext;
    handle.index = CollectionIndex::Ivf(Some(ivf));
    handle.stale = false;

    let tail = store.recovery_tail(handle.id)?;
    for ext in &tail.deleted {
        let Some(&internal) = handle.ext_to_int.get(ext) else {
            continue;
        };
        if let CollectionIndex::Ivf(Some(ivf)) = &mut handle.index {
            ivf.remove(internal);
        }
    }
    for (ext, record) in tail.upserts {
        let internal = match handle.ext_to_int.get(&ext) {
            Some(&i) => i,
            None => {
                let i = handle.int_to_ext.len() as u64;
                handle.ext_to_int.insert(ext.clone(), i);
                handle.int_to_ext.push(ext);
                i
            }
        };
        if let CollectionIndex::Ivf(Some(ivf)) = &mut handle.index {
            ivf.insert(internal, &record.vector)?;
        }
    }
    Ok(())
}

// Rebuild a collection's index from the store's current live rows.
fn rebuild_index(store: &Store, handle: &mut CollectionHandle) -> Result<()> {
    let mut int_to_ext = Vec::new();
    let mut ext_to_int = HashMap::new();
    let mut flat: Vec<f32> = Vec::new();
    for (ext_id, record) in store.scan(handle.id)? {
        let internal = int_to_ext.len() as u64;
        flat.extend_from_slice(&record.vector);
        ext_to_int.insert(ext_id.clone(), internal);
        int_to_ext.push(ext_id);
    }
    let ids: Vec<u64> = (0..int_to_ext.len() as u64).collect();
    // Drop the previous index before rebuilding: a disk index `mmap`s a file we
    // are about to overwrite in place, and the mapping assumes an immutable file.
    handle.index = empty_index(&handle.descriptor);
    handle.index = build_index(store, handle.id, &handle.descriptor, &ids, &flat)?;
    handle.int_to_ext = int_to_ext;
    handle.ext_to_int = ext_to_int;
    handle.stale = false;
    Ok(())
}

// The set of live external ids guaranteed to contain every row the `filter`
// accepts (a sound superset), resolved through the collection's secondary
// indexes. `None` means the filter cannot be narrowed with the available indexes
// and the caller should post-filter instead; `Some(set)` is a candidate
// superset, and an empty set proves no row can match.
//
// Only indexable leaves — equality, `in`, and range comparisons on declared
// filterable fields of a matching type — constrain the set. Everything else
// (negation, existence, `ne`, non-indexed fields, type mismatches) is treated as
// unconstrained, which keeps the result a superset; exactness is restored when
// the caller re-checks the full `Filter` on each surviving candidate.
fn candidate_ids(
    store: &Store,
    cid: CollectionId,
    filter: &Filter,
    filterable: &[FilterableField],
) -> Result<Option<BTreeSet<String>>> {
    match filter {
        Filter::And(subs) => {
            // Intersect the constrained children; an unconstrained child (None)
            // is dropped, which only widens the set — still a superset.
            let mut acc: Option<BTreeSet<String>> = None;
            for sub in subs {
                if let Some(set) = candidate_ids(store, cid, sub, filterable)? {
                    acc = Some(match acc {
                        Some(existing) => existing.intersection(&set).cloned().collect(),
                        None => set,
                    });
                }
            }
            Ok(acc)
        }
        Filter::Or(subs) => {
            // The union of the children — but one unconstrained child makes the
            // whole disjunction unconstrained (its rows could be anything).
            let mut acc = BTreeSet::new();
            for sub in subs {
                match candidate_ids(store, cid, sub, filterable)? {
                    Some(set) => acc.extend(set),
                    None => return Ok(None),
                }
            }
            Ok(Some(acc))
        }
        // A negation cannot be narrowed to a superset with these indexes.
        Filter::Not(_) => Ok(None),
        // A leaf: indexable ⇒ its matching ids; otherwise unconstrained.
        leaf => match leaf_predicate(leaf, filterable) {
            Some(pred) => Ok(Some(store.matching_ids(cid, &pred)?.into_iter().collect())),
            None => Ok(None),
        },
    }
}

// Map a single filter leaf to an indexable secondary-index predicate, if its
// field is declared filterable and the value types line up. Boolean nodes and
// predicates the secondary index cannot answer (`Ne`, `Exists`) return `None`.
fn leaf_predicate(filter: &Filter, filterable: &[FilterableField]) -> Option<SecPredicate> {
    let field_type = |field: &str| {
        filterable
            .iter()
            .find(|f| f.path == field)
            .map(|f| f.field_type)
    };
    match filter {
        Filter::Eq { field, value } => Some(SecPredicate::Eq {
            field: field.clone(),
            value: sec_value(field_type(field)?, value)?,
        }),
        Filter::In { field, values } => {
            let ft = field_type(field)?;
            // Every value must encode, or the `in` set would be understated and
            // the candidate superset unsound.
            let values: Option<Vec<SecValue>> = values.iter().map(|v| sec_value(ft, v)).collect();
            Some(SecPredicate::In {
                field: field.clone(),
                values: values?,
            })
        }
        Filter::Lt { field, value } => {
            one_sided_range(field, field_type(field)?, value, false, false)
        }
        Filter::Lte { field, value } => {
            one_sided_range(field, field_type(field)?, value, false, true)
        }
        Filter::Gt { field, value } => {
            one_sided_range(field, field_type(field)?, value, true, false)
        }
        Filter::Gte { field, value } => {
            one_sided_range(field, field_type(field)?, value, true, true)
        }
        _ => None,
    }
}

// Build a one-sided range predicate from a comparison leaf. `is_lower` selects
// whether `value` is the lower (`Gt`/`Gte`) or upper (`Lt`/`Lte`) bound;
// `inclusive` is that bound's inclusivity. `None` on a type mismatch.
fn one_sided_range(
    field: &str,
    field_type: FieldType,
    value: &Value,
    is_lower: bool,
    inclusive: bool,
) -> Option<SecPredicate> {
    let v = sec_value(field_type, value)?;
    let (lo, hi, lo_inclusive, hi_inclusive) = if is_lower {
        (Some(v), None, inclusive, false)
    } else {
        (None, Some(v), false, inclusive)
    };
    Some(SecPredicate::Range {
        field: field.to_owned(),
        lo,
        hi,
        lo_inclusive,
        hi_inclusive,
    })
}

// Encode a filter value as a typed secondary-index value, or `None` when the
// JSON type does not match the field's declared type (so it cannot be
// pre-filtered and the planner falls back to post-filtering).
fn sec_value(field_type: FieldType, value: &Value) -> Option<SecValue> {
    match (field_type, value) {
        (FieldType::Keyword, Value::String(s)) => Some(SecValue::Keyword(s.clone())),
        (FieldType::Numeric, Value::Number(n)) => n.as_f64().map(SecValue::Numeric),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn desc() -> Descriptor {
        Descriptor::new(4, Dtype::F32, DistanceMetric::L2)
    }

    fn open(dir: &Path) -> Database {
        Database::open(dir).unwrap()
    }

    #[test]
    fn create_upsert_search_get_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("items", desc()).unwrap();
        db.upsert(
            "items",
            "a",
            &[0.0, 0.0, 0.0, 0.0],
            &json!({"color": "red"}),
        )
        .unwrap();
        db.upsert(
            "items",
            "b",
            &[1.0, 0.0, 0.0, 0.0],
            &json!({"color": "blue"}),
        )
        .unwrap();
        db.upsert(
            "items",
            "c",
            &[5.0, 5.0, 5.0, 5.0],
            &json!({"color": "red"}),
        )
        .unwrap();

        let near = db
            .search("items", &[0.1, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(near[0].id, "a");
        assert_eq!(near[1].id, "b");

        let got = db.get("items", "c").unwrap().unwrap();
        assert_eq!(got.vector, Some(vec![5.0, 5.0, 5.0, 5.0]));
        assert_eq!(got.payload, Some(json!({"color": "red"})));
    }

    #[test]
    fn filtered_search_only_returns_matching_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("items", desc()).unwrap();
        for i in 0..20u32 {
            let color = if i % 2 == 0 { "red" } else { "blue" };
            db.upsert(
                "items",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({"color": color, "n": i}),
            )
            .unwrap();
        }
        let params = SearchParams {
            k: 5,
            filter: Some(Filter::Eq {
                field: "color".into(),
                value: json!("red"),
            }),
            ef_search: 64,
            with_payload: true,
            with_vector: false,
        };
        let results = db.search("items", &[0.0; 4], &params).unwrap();
        assert!(!results.is_empty());
        for m in &results {
            assert_eq!(m.payload.as_ref().unwrap()["color"], json!("red"));
        }
    }

    #[test]
    fn persists_and_rebuilds_index_on_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            db.create_collection("items", desc()).unwrap();
            for i in 0..50u32 {
                db.upsert(
                    "items",
                    &format!("p{i}"),
                    &[i as f32, 1.0, 2.0, 3.0],
                    &json!({}),
                )
                .unwrap();
            }
            db.checkpoint().unwrap();
        }
        let mut db = open(tmp.path());
        assert_eq!(db.len("items").unwrap(), 50);
        let res = db
            .search("items", &[7.0, 1.0, 2.0, 3.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "p7");
    }

    #[test]
    fn update_reflects_new_vector_after_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("items", desc()).unwrap();
        db.upsert("items", "a", &[0.0; 4], &json!({})).unwrap();
        db.upsert("items", "b", &[10.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        // Move "a" far away; querying near the origin should now prefer "b".
        db.upsert("items", "a", &[100.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        let res = db
            .search("items", &[0.0; 4], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "b");
        assert_eq!(
            db.get("items", "a").unwrap().unwrap().vector,
            Some(vec![100.0, 0.0, 0.0, 0.0])
        );
    }

    #[test]
    fn delete_removes_from_search() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("items", desc()).unwrap();
        db.upsert("items", "a", &[0.0; 4], &json!({})).unwrap();
        db.upsert("items", "b", &[1.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        assert!(db.delete("items", "a").unwrap());
        let res = db
            .search("items", &[0.0; 4], &SearchParams::default())
            .unwrap();
        assert!(res.iter().all(|m| m.id != "a"));
        assert!(db.get("items", "a").unwrap().is_none());
    }

    #[test]
    fn unknown_collection_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        assert!(matches!(
            db.search("nope", &[0.0; 4], &SearchParams::default()),
            Err(Error::CollectionNotFound(_))
        ));
        db.create_collection("c", desc()).unwrap();
        assert!(matches!(
            db.create_collection("c", desc()),
            Err(Error::Core(quiver_core::CoreError::AlreadyExists(_)))
        ));
    }

    fn desc_with(kind: IndexKind) -> Descriptor {
        Descriptor::new(4, Dtype::F32, DistanceMetric::L2).with_index(IndexSpec {
            kind,
            pq_subspaces: None,
        })
    }

    #[test]
    fn vamana_and_ivf_collections_find_the_nearest_point() {
        for kind in [IndexKind::Vamana, IndexKind::Ivf] {
            let tmp = tempfile::tempdir().unwrap();
            let mut db = open(tmp.path());
            db.create_collection("c", desc_with(kind)).unwrap();
            for i in 0..40u32 {
                db.upsert(
                    "c",
                    &format!("p{i}"),
                    &[i as f32, 0.0, 0.0, 0.0],
                    &json!({}),
                )
                .unwrap();
            }
            // ef_search maps onto the index's search width (l_search / nprobe).
            let res = db
                .search("c", &[7.0, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert_eq!(res[0].id, "p7", "{kind:?} nearest");
        }
    }

    #[test]
    fn index_kind_persists_and_rebuilds_on_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            db.create_collection("v", desc_with(IndexKind::Vamana))
                .unwrap();
            for i in 0..20u32 {
                db.upsert(
                    "v",
                    &format!("p{i}"),
                    &[i as f32, 1.0, 2.0, 3.0],
                    &json!({}),
                )
                .unwrap();
            }
            db.checkpoint().unwrap();
        }
        let mut db = open(tmp.path());
        assert_eq!(db.descriptor("v").unwrap().index.kind, IndexKind::Vamana);
        let res = db
            .search("v", &[7.0, 1.0, 2.0, 3.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "p7");
    }

    #[test]
    fn ivf_upserts_and_deletes_incrementally_without_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("c", desc_with(IndexKind::Ivf))
            .unwrap();
        for i in 0..50u32 {
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({}),
            )
            .unwrap();
        }
        // The first search builds (and trains) the IVF from the store.
        let _ = db
            .search("c", &[1.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert!(!db.collections["c"].stale, "the search built the index");

        // Incremental insert: a new outlier is found, with no rebuild scheduled.
        db.upsert("c", "far", &[500.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        assert!(!db.collections["c"].stale, "ivf insert stayed incremental");
        let res = db
            .search("c", &[500.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "far");

        // Incremental delete: the point disappears, still with no rebuild.
        assert!(db.delete("c", "far").unwrap());
        assert!(!db.collections["c"].stale, "ivf delete stayed incremental");
        let res = db
            .search("c", &[500.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert!(res.iter().all(|m| m.id != "far"), "deleted point is gone");
    }

    #[test]
    fn ivf_incremental_update_replaces_the_vector() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("c", desc_with(IndexKind::Ivf))
            .unwrap();
        for i in 0..30u32 {
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({}),
            )
            .unwrap();
        }
        let _ = db.search("c", &[0.0; 4], &SearchParams::default()).unwrap();
        // Move p5 far away in place.
        db.upsert("c", "p5", &[900.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        assert!(!db.collections["c"].stale);
        let at_new = db
            .search("c", &[900.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(at_new[0].id, "p5", "p5 found at its new location");
        let at_old = db
            .search("c", &[5.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert!(at_old.iter().all(|m| m.id != "p5"), "stale vector is gone");
    }

    #[test]
    fn ivf_reinsert_after_incremental_delete_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("c", desc_with(IndexKind::Ivf))
            .unwrap();
        for i in 0..20u32 {
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({}),
            )
            .unwrap();
        }
        let _ = db.search("c", &[0.0; 4], &SearchParams::default()).unwrap();
        assert!(db.delete("c", "p3").unwrap());
        assert!(!db.collections["c"].stale);
        // Re-insert the same id; it must be searchable again (the slot is reused).
        db.upsert("c", "p3", &[3.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        assert!(!db.collections["c"].stale);
        let res = db
            .search("c", &[3.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "p3");
    }

    #[test]
    fn hnsw_in_place_update_falls_back_to_rebuild() {
        // HNSW cannot update an id in place, so an update marks the index stale.
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("c", desc()).unwrap();
        for i in 0..10u32 {
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({}),
            )
            .unwrap();
        }
        let _ = db.search("c", &[0.0; 4], &SearchParams::default()).unwrap();
        assert!(!db.collections["c"].stale);
        db.upsert("c", "p2", &[42.0, 0.0, 0.0, 0.0], &json!({}))
            .unwrap();
        assert!(db.collections["c"].stale, "hnsw update schedules a rebuild");
        // The rebuild on the next search reflects the new vector.
        let res = db
            .search("c", &[42.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "p2");
    }

    #[test]
    fn unsupported_index_configurations_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        // Vamana/IVF do not support inner product.
        let dot_vamana =
            Descriptor::new(4, Dtype::F32, DistanceMetric::Dot).with_index(IndexSpec {
                kind: IndexKind::Vamana,
                pq_subspaces: None,
            });
        assert!(matches!(
            db.create_collection("a", dot_vamana),
            Err(Error::Unsupported(_))
        ));
        // ...nor does the disk-resident index.
        let dot_disk = Descriptor::new(4, Dtype::F32, DistanceMetric::Dot).with_index(IndexSpec {
            kind: IndexKind::DiskVamana,
            pq_subspaces: None,
        });
        assert!(matches!(
            db.create_collection("b", dot_disk),
            Err(Error::Unsupported(_))
        ));
    }

    // Recursively check whether a file named `name` exists under `dir`.
    fn contains_file(dir: &Path, name: &str) -> bool {
        std::fs::read_dir(dir).is_ok_and(|rd| {
            rd.flatten().any(|e| {
                let p = e.path();
                if p.is_dir() {
                    contains_file(&p, name)
                } else {
                    p.file_name().is_some_and(|f| f == name)
                }
            })
        })
    }

    #[test]
    fn disk_index_collection_searches_persists_and_writes_an_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            db.create_collection("d", desc_with(IndexKind::DiskVamana))
                .unwrap();
            for i in 0..40u32 {
                db.upsert(
                    "d",
                    &format!("p{i}"),
                    &[i as f32, 0.0, 0.0, 0.0],
                    &json!({}),
                )
                .unwrap();
            }
            let res = db
                .search("d", &[7.0, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert_eq!(res[0].id, "p7");
            db.checkpoint().unwrap();
        }
        // The encrypted disk artifact was written under the collection's index dir.
        assert!(
            contains_file(tmp.path(), "vamana.qvx"),
            "disk index file missing"
        );
        // Reopening rebuilds and re-opens the disk index; search still works.
        let mut db = open(tmp.path());
        assert_eq!(
            db.descriptor("d").unwrap().index.kind,
            IndexKind::DiskVamana
        );
        let res = db
            .search("d", &[7.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "p7");
    }

    // ---- hybrid (pre-filtered) search ----

    // A collection whose `city` (keyword) and `n` (numeric) payload fields are
    // declared filterable, so the planner can pre-filter on them.
    fn desc_filterable() -> Descriptor {
        Descriptor::new(4, Dtype::F32, DistanceMetric::L2).with_filterable(vec![
            FilterableField::keyword("city"),
            FilterableField::numeric("n"),
        ])
    }

    // 30 points on the x-axis (distance to the origin grows with i), cycling
    // through three cities, each carrying its index as a numeric `n`.
    // Checkpointed so the rows live in a sealed segment's secondary index — the
    // pre-filter primitive — not only the active buffer.
    fn seed_cities(db: &mut Database) {
        const CITIES: [&str; 3] = ["paris", "lyon", "rome"];
        db.create_collection("c", desc_filterable()).unwrap();
        for i in 0..30u32 {
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({"city": CITIES[i as usize % 3], "n": i}),
            )
            .unwrap();
        }
        db.checkpoint().unwrap();
    }

    #[test]
    fn hybrid_equality_prefilter_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        seed_cities(&mut db);
        let params = SearchParams {
            k: 5,
            filter: Some(Filter::Eq {
                field: "city".into(),
                value: json!("lyon"),
            }),
            ..SearchParams::default()
        };
        let res = db.search("c", &[0.0; 4], &params).unwrap();
        assert!(!res.is_empty());
        // lyon is i % 3 == 1 → p1, p4, p7, …; nearest the origin is p1.
        assert_eq!(res[0].id, "p1");
        for m in &res {
            assert_eq!(m.payload.as_ref().unwrap()["city"], json!("lyon"));
        }
    }

    #[test]
    fn hybrid_numeric_range_prefilter_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        seed_cities(&mut db);
        let params = SearchParams {
            k: 4,
            filter: Some(Filter::Gte {
                field: "n".into(),
                value: json!(10),
            }),
            ..SearchParams::default()
        };
        let res = db.search("c", &[0.0; 4], &params).unwrap();
        // Among n >= 10, the nearest the origin is n == 10 (p10).
        assert_eq!(res[0].id, "p10");
        for m in &res {
            assert!(m.payload.as_ref().unwrap()["n"].as_u64().unwrap() >= 10);
        }
    }

    #[test]
    fn hybrid_unsatisfiable_filter_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        seed_cities(&mut db);
        // No row holds this city, so the pre-filter proves the result empty
        // without touching the vector index.
        let params = SearchParams {
            filter: Some(Filter::Eq {
                field: "city".into(),
                value: json!("atlantis"),
            }),
            ..SearchParams::default()
        };
        assert!(db.search("c", &[0.0; 4], &params).unwrap().is_empty());
    }

    #[test]
    fn hybrid_and_or_composition_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        seed_cities(&mut db);
        // (city in {paris, rome}) AND (n < 12): a keyword `in` intersected with a
        // numeric range, both pre-filterable.
        let params = SearchParams {
            k: 10,
            filter: Some(Filter::And(vec![
                Filter::In {
                    field: "city".into(),
                    values: vec![json!("paris"), json!("rome")],
                },
                Filter::Lt {
                    field: "n".into(),
                    value: json!(12),
                },
            ])),
            ..SearchParams::default()
        };
        let res = db.search("c", &[0.0; 4], &params).unwrap();
        // paris is i % 3 == 0 → the nearest qualifying point is p0.
        assert_eq!(res[0].id, "p0");
        for m in &res {
            let payload = m.payload.as_ref().unwrap();
            let city = payload["city"].as_str().unwrap();
            assert!(city == "paris" || city == "rome");
            assert!(payload["n"].as_u64().unwrap() < 12);
        }
    }

    #[test]
    fn hybrid_rechecks_non_indexable_clause() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        seed_cities(&mut db);
        // city is pre-filterable; the Not(…) clause is not, so it is re-checked
        // exactly on the narrowed candidates — paris rows excluding p0.
        let params = SearchParams {
            k: 10,
            filter: Some(Filter::And(vec![
                Filter::Eq {
                    field: "city".into(),
                    value: json!("paris"),
                },
                Filter::Not(Box::new(Filter::Eq {
                    field: "n".into(),
                    value: json!(0),
                })),
            ])),
            ..SearchParams::default()
        };
        let res = db.search("c", &[0.0; 4], &params).unwrap();
        assert!(res.iter().all(|m| m.id != "p0"));
        // The nearest paris point after p0 is p3.
        assert_eq!(res[0].id, "p3");
        for m in &res {
            assert_eq!(m.payload.as_ref().unwrap()["city"], json!("paris"));
        }
    }

    #[test]
    fn post_filter_fallback_on_undeclared_field_is_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        // Only `city` is filterable; a filter on the undeclared `tier` cannot
        // pre-filter and falls back to ANN post-filtering — still exact.
        db.create_collection(
            "c",
            Descriptor::new(4, Dtype::F32, DistanceMetric::L2)
                .with_filterable(vec![FilterableField::keyword("city")]),
        )
        .unwrap();
        for i in 0..20u32 {
            let tier = if i % 2 == 0 { "gold" } else { "silver" };
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({"city": "paris", "tier": tier}),
            )
            .unwrap();
        }
        let params = SearchParams {
            k: 5,
            filter: Some(Filter::Eq {
                field: "tier".into(),
                value: json!("gold"),
            }),
            ..SearchParams::default()
        };
        let res = db.search("c", &[0.0; 4], &params).unwrap();
        assert!(!res.is_empty());
        for m in &res {
            assert_eq!(m.payload.as_ref().unwrap()["tier"], json!("gold"));
        }
    }

    #[test]
    fn leaf_predicate_maps_only_indexable_filterable_leaves() {
        let fields = vec![
            FilterableField::keyword("city"),
            FilterableField::numeric("n"),
        ];
        // Keyword equality on a filterable field maps.
        assert_eq!(
            leaf_predicate(
                &Filter::Eq {
                    field: "city".into(),
                    value: json!("paris")
                },
                &fields
            ),
            Some(SecPredicate::Eq {
                field: "city".into(),
                value: SecValue::Keyword("paris".into())
            })
        );
        // A numeric comparison maps to a one-sided range.
        assert_eq!(
            leaf_predicate(
                &Filter::Gte {
                    field: "n".into(),
                    value: json!(3)
                },
                &fields
            ),
            Some(SecPredicate::Range {
                field: "n".into(),
                lo: Some(SecValue::Numeric(3.0)),
                hi: None,
                lo_inclusive: true,
                hi_inclusive: false,
            })
        );
        // Undeclared field, type mismatch, `ne`, and `exists` do not map.
        let undeclared = Filter::Eq {
            field: "tier".into(),
            value: json!("gold"),
        };
        let mismatch = Filter::Eq {
            field: "city".into(),
            value: json!(5),
        };
        let ne = Filter::Ne {
            field: "city".into(),
            value: json!("x"),
        };
        let exists = Filter::Exists {
            field: "city".into(),
        };
        assert!(leaf_predicate(&undeclared, &fields).is_none());
        assert!(leaf_predicate(&mismatch, &fields).is_none());
        assert!(leaf_predicate(&ne, &fields).is_none());
        assert!(leaf_predicate(&exists, &fields).is_none());
    }

    // ----- durable IVF index recovery (ADR-0025) -----

    // The first collection created in a fresh store has id 0.
    fn ivf_index_dir(root: &Path) -> std::path::PathBuf {
        root.join("collections").join("0000000000").join("index")
    }

    fn idx_snapshot_files(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(ivf_index_dir(root))
            .map(|rd| {
                rd.filter_map(std::result::Result::ok)
                    .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                    .filter(|n| n.starts_with("idx-"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    fn nearest(db: &mut Database, q: &[f32]) -> Vec<String> {
        db.search("c", q, &SearchParams::default())
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    fn seed_ivf(db: &mut Database, n: u32) {
        db.create_collection("c", desc_with(IndexKind::Ivf))
            .unwrap();
        for i in 0..n {
            db.upsert(
                "c",
                &format!("p{i}"),
                &[i as f32, 0.0, 0.0, 0.0],
                &json!({}),
            )
            .unwrap();
        }
        // The first search builds (and trains) the IVF from the store.
        let _ = nearest(db, &[1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn ivf_snapshot_is_written_at_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        seed_ivf(&mut db, 40);
        db.checkpoint().unwrap();
        assert_eq!(idx_snapshot_files(tmp.path()).len(), 1);
    }

    #[test]
    fn ivf_loads_from_snapshot_rather_than_rebuilding() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            db.create_collection("c", desc_with(IndexKind::Ivf))
                .unwrap();
            db.upsert("c", "a", &[0.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
            db.upsert("c", "m", &[1.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
            // First search builds the IVF — int_to_ext is the sorted scan order.
            let _ = nearest(&mut db, &[0.0, 0.0, 0.0, 0.0]);
            // Incremental upserts append in insertion order, diverging from sort.
            db.upsert("c", "z", &[2.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
            db.upsert("c", "b", &[3.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
            db.checkpoint().unwrap();
            assert_eq!(db.collections["c"].int_to_ext, ["a", "m", "z", "b"]);
        }
        let db = open(tmp.path());
        // Loaded from the snapshot: the insertion-order mapping is preserved. A
        // rebuild would have produced the sorted order ["a", "b", "m", "z"].
        assert_eq!(
            db.collections["c"].int_to_ext,
            ["a", "m", "z", "b"],
            "index was rebuilt, not loaded from the snapshot"
        );
    }

    #[test]
    fn ivf_recovery_replays_post_checkpoint_upserts() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            seed_ivf(&mut db, 30);
            db.checkpoint().unwrap();
            // Post-checkpoint upsert, no further checkpoint.
            db.upsert("c", "far", &[500.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
        }
        let mut db = open(tmp.path());
        assert_eq!(nearest(&mut db, &[500.0, 0.0, 0.0, 0.0])[0], "far");
        assert_eq!(nearest(&mut db, &[1.0, 0.0, 0.0, 0.0])[0], "p1");
    }

    #[test]
    fn ivf_recovery_replays_post_checkpoint_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            seed_ivf(&mut db, 30);
            db.checkpoint().unwrap();
            assert!(db.delete("c", "p7").unwrap());
        }
        let mut db = open(tmp.path());
        assert!(
            nearest(&mut db, &[7.0, 0.0, 0.0, 0.0])
                .iter()
                .all(|id| id != "p7")
        );
        assert!(db.get("c", "p7").unwrap().is_none());
        assert!(db.get("c", "p6").unwrap().is_some());
    }

    #[test]
    fn ivf_recovery_replays_post_checkpoint_updates() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            seed_ivf(&mut db, 30);
            db.checkpoint().unwrap();
            // Move p0 far away — an in-place update shadowing its sealed row.
            db.upsert("c", "p0", &[999.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
        }
        let mut db = open(tmp.path());
        assert_eq!(nearest(&mut db, &[999.0, 0.0, 0.0, 0.0])[0], "p0");
        assert_ne!(
            nearest(&mut db, &[0.0, 0.0, 0.0, 0.0])[0],
            "p0",
            "the stale p0 vector survived the update"
        );
    }

    #[test]
    fn corrupt_ivf_snapshot_falls_back_to_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            seed_ivf(&mut db, 30);
            db.checkpoint().unwrap();
        }
        // Corrupt the snapshot file; recovery must fall back to a rebuild.
        let files = idx_snapshot_files(tmp.path());
        assert_eq!(files.len(), 1);
        std::fs::write(ivf_index_dir(tmp.path()).join(&files[0]), b"corrupt").unwrap();

        let mut db = open(tmp.path());
        assert_eq!(nearest(&mut db, &[7.0, 0.0, 0.0, 0.0])[0], "p7");
    }
}
