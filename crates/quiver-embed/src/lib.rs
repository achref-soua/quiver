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
//! incrementally with LIRE rebalancing (ADR-0023). The Vamana / disk graph
//! family is maintained the FreshDiskANN way (ADR-0033): the batch-built graph
//! is a read-only base, recent inserts land in an in-memory delta graph, and
//! deletes are tombstoned, so writes are size-independent; when the pending work
//! grows past a fixed fraction of the base the next access consolidates by
//! rebuilding from the store. All indexes stay derived (rebuilt from the store
//! on open), so the crash gate never sees an index write.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use quiver_core::{SecPredicate, SecValue, Store};
use quiver_index::{
    ColbertConfig, ColbertIndex, DiskVamana, FreshDiskVamana, FreshVamana, Hnsw, HnswConfig, Index,
    Ivf, IvfConfig, Metric, Neighbor, ProductQuantizer, Vamana, VamanaConfig, max_sim,
    ordering_distance, report_metric,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use quiver_core::keyring::{KeyRing, SingleCodecKeyRing};
pub use quiver_core::page::PageCodec;
pub use quiver_core::{CollectionId, CommitObserver, WalEntry, WalOp};
pub use quiver_core::{
    Descriptor, DistanceMetric, Dtype, FieldType, FilterableField, IndexKind, IndexSpec,
    VectorEncryption,
};
pub use quiver_query::Filter;
pub use quiver_query::{DEFAULT_RRF_K0, SPARSE_KEY, SparseInvertedIndex, SparseVector, rrf_fuse};

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

/// A multi-vector (late-interaction / ColBERT) document result: a document id, its
/// MaxSim relevance, the payload, and — if requested — the document's token
/// vectors (ADR-0028).
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentMatch {
    /// Document id.
    pub id: String,
    /// MaxSim relevance score (0 for a direct fetch).
    pub score: f32,
    /// The document payload, if requested / present (stored on the anchor token).
    pub payload: Option<Value>,
    /// The document's token vectors, if requested.
    pub vectors: Option<Vec<Vec<f32>>>,
}

// A candidate document during multi-vector re-ranking, before it becomes a
// [`DocumentMatch`]: `(MaxSim score, document id, anchor payload, token vectors
// when requested)`. Named so the re-rank buffer stays under clippy's
// type-complexity threshold.
type ScoredDocument = (f32, String, Option<Value>, Option<Vec<Vec<f32>>>);

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

// Hybrid search (ADR-0043) pulls `k * RRF_CANDIDATE_FACTOR` (at least
// `MIN_RRF_CANDIDATES`) candidates from each side before Reciprocal Rank Fusion,
// so a document ranked outside the top `k` on one side can still surface via the
// other.
const RRF_CANDIDATE_FACTOR: usize = 10;
const MIN_RRF_CANDIDATES: usize = 100;

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

/// Pending graph work — delta inserts plus tombstones — at which a FreshDiskANN
/// graph collection consolidates: the next access rebuilds the base from the
/// store, reclaiming tombstones and folding in the delta (ADR-0033). Below it,
/// inserts go to the in-memory delta and deletes are `O(1)` tombstones. Mirrors
/// [`HNSW_REBUILD_DELETED_FRACTION`].
const GRAPH_REBUILD_PENDING_FRACTION: f64 = 0.2;

// The vector index backing one collection. HNSW and (once built) IVF are
// maintained incrementally; the Vamana and disk graphs are batch-built from the
// store (the `Option` is `None` until first build) and then maintained
// incrementally the FreshDiskANN way — a read-only base graph plus an in-memory
// delta and deletion set, consolidated by a rebuild past a churn threshold
// (ADR-0033).
enum CollectionIndex {
    // A fetch-only collection with no server-side ANN index: a client-side-encrypted
    // collection (ADR-0032) stores opaque ciphertext the server never ranks, so the
    // client fetches points and ranks locally.
    None,
    Hnsw(Hnsw),
    Vamana(Option<FreshVamana>),
    Ivf(Option<Ivf>),
    // The disk-resident DiskANN index: PQ codes in RAM, graph + full vectors on
    // (encrypted) SSD, exact re-rank (ADR-0019), with a FreshDiskANN in-memory
    // delta layered on top so the on-disk artifact stays immutable.
    Disk(Option<FreshDiskVamana>),
    // The ColBERTv2/PLAID compressed token-pool index for a multi-vector
    // collection (ADR-0034): centroid + residual-PQ codes with centroid-pruned
    // candidate generation.
    Colbert(Option<ColbertIndex>),
}

impl CollectionIndex {
    // Search, mapping the generic `ef` knob onto each index's search width.
    fn search(&self, query: &[f32], k: usize, ef: usize) -> Result<Vec<Neighbor>> {
        Ok(match self {
            CollectionIndex::Hnsw(h) => h.search(query, k, ef)?,
            CollectionIndex::Vamana(Some(g)) => g.search(query, k, ef)?,
            CollectionIndex::Ivf(Some(i)) => i.search(query, k, ef)?,
            CollectionIndex::Disk(Some(d)) => d.search(query, k, ef)?,
            CollectionIndex::Colbert(Some(c)) => c.search(query, k, ef)?,
            CollectionIndex::None
            | CollectionIndex::Vamana(None)
            | CollectionIndex::Ivf(None)
            | CollectionIndex::Disk(None)
            | CollectionIndex::Colbert(None) => Vec::new(),
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
    // For a multi-vector (ColBERT) collection: each document id mapped to its
    // token count, so a re-rank can gather all of a document's token rows
    // (`<doc-id><US><ordinal>`) and `document_count` is O(1). `None` for a
    // single-vector collection. Maintained eagerly on document writes and rebuilt
    // authoritatively from the store on open / rebuild; never persisted (ADR-0028).
    docs: Option<BTreeMap<String, u32>>,
    // Derived inverted index over the collection's `__quiver_sparse__` payloads,
    // for the sparse half of hybrid search (ADR-0045). `Some` for a single-vector,
    // server-searchable collection; `None` for multi-vector and client-side-encrypted
    // collections (which never run hybrid search) — and as a backstop, when `None`
    // the sparse ranking falls back to a full store scan. Built on rebuild from the
    // store and maintained incrementally on upsert/delete; never persisted.
    sparse: Option<SparseInvertedIndex>,
}

// Whether a collection should carry a derived sparse inverted index: only
// single-vector, server-searchable collections run hybrid search (ADR-0045).
fn uses_sparse_index(descriptor: &Descriptor) -> bool {
    !descriptor.multivector && descriptor.vector_encryption != VectorEncryption::ClientSide
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
                docs: None,
                // Populated by `load_index` / `rebuild_index` from the store.
                sparse: None,
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
        let docs = descriptor.multivector.then(BTreeMap::new);
        // A fresh single-vector collection starts with an empty inverted index
        // maintained incrementally from the first upsert (an empty index allocates
        // nothing until a sparse vector arrives).
        let sparse = uses_sparse_index(&descriptor).then(SparseInvertedIndex::new);
        self.collections.insert(
            name.to_owned(),
            CollectionHandle {
                id,
                descriptor,
                index,
                int_to_ext: Vec::new(),
                ext_to_int: HashMap::new(),
                stale: false,
                docs,
                sparse,
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

    /// Install a replication commit observer, invoked with each committed
    /// [`WalEntry`] in commit order (ADR-0030). The server uses this to drive a
    /// leader's replication stream.
    pub fn set_commit_observer(&mut self, observer: CommitObserver) {
        self.store.set_commit_observer(observer);
    }

    /// The operations that recreate the current logical state, for a replication
    /// follower to bootstrap from (ADR-0030).
    ///
    /// # Errors
    /// Propagates a store read error.
    pub fn replication_snapshot(&self) -> Result<Vec<WalOp>> {
        Ok(self.store.replication_snapshot()?)
    }

    /// Apply a replicated operation from a leader (ADR-0030): persist and apply it
    /// to the store (preserving the leader's collection id), then reconcile the
    /// in-memory index handles — register a new collection, drop a removed one, or
    /// mark a touched collection's index stale so the next read rebuilds from the
    /// replicated state.
    ///
    /// # Errors
    /// Propagates a store apply error.
    pub fn apply_replicated(&mut self, op: WalOp) -> Result<()> {
        let target = match &op {
            WalOp::CreateCollection { collection_id, .. }
            | WalOp::DropCollection { collection_id }
            | WalOp::Upsert { collection_id, .. }
            | WalOp::Delete { collection_id, .. } => Some(*collection_id),
            WalOp::Checkpoint { .. } => None,
        };
        let create_name = match &op {
            WalOp::CreateCollection { name, .. } => Some(name.clone()),
            _ => None,
        };
        let is_drop = matches!(op, WalOp::DropCollection { .. });
        self.store.apply_replicated(op)?;

        if let Some(name) = create_name {
            // Register a fresh handle for the newly replicated collection.
            if let Some(id) = target
                && let Some(descriptor) = self.store.descriptor(id).cloned()
            {
                let docs = descriptor.multivector.then(BTreeMap::new);
                let index = empty_index(&descriptor);
                // Replicated writes mark the handle stale, so the next read rebuilds
                // the inverted index from the replicated store.
                self.collections.insert(
                    name,
                    CollectionHandle {
                        id,
                        descriptor,
                        index,
                        int_to_ext: Vec::new(),
                        ext_to_int: HashMap::new(),
                        stale: false,
                        docs,
                        sparse: None,
                    },
                );
            }
        } else if is_drop {
            if let Some(id) = target {
                self.collections.retain(|_, h| h.id != id);
            }
        } else if let Some(id) = target
            && let Some(handle) = self.collections.values_mut().find(|h| h.id == id)
        {
            handle.stale = true;
        }
        Ok(())
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
        require_single_vector(handle)?;
        let payload_bytes = serde_json::to_vec(payload)?;
        self.store.upsert(handle.id, id, vector, &payload_bytes)?;
        // Client-side-encrypted collections have no server-side index to maintain
        // (ADR-0032): the stored vector is an opaque placeholder the server never
        // ranks. The point is durable in the store; nothing else to do.
        if handle.descriptor.vector_encryption == VectorEncryption::ClientSide {
            return Ok(());
        }
        // Maintain the in-memory index in place where the kind allows it, else
        // defer to a lazy rebuild on the next search (ADR-0023/0026/0033).
        index_upsert_point(handle, id, vector)?;
        // Keep the derived sparse inverted index in step (ADR-0045).
        sparse_index_upsert_point(handle, id, payload);
        Ok(())
    }

    /// Upsert a batch of points with a single WAL `fdatasync` (ADR-0038).
    ///
    /// `points` is `(id, vector, payload)` tuples.  The batch is committed
    /// atomically — all points or none (from the client's perspective).  This
    /// is the preferred path for the REST `POST /v1/collections/{c}/points`
    /// handler which already delivers a batch per HTTP request.
    pub fn upsert_batch(
        &mut self,
        collection: &str,
        points: &[(&str, &[f32], &serde_json::Value)],
    ) -> Result<u64> {
        let handle = self
            .collections
            .get(collection)
            .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
        require_single_vector(handle)?;
        let coll_id = handle.id;
        let is_client_side = handle.descriptor.vector_encryption == VectorEncryption::ClientSide;

        let payload_bytes: Vec<Vec<u8>> = points
            .iter()
            .map(|(_, _, p)| serde_json::to_vec(p).map_err(Error::Json))
            .collect::<Result<_>>()?;

        let records: Vec<(&str, &[f32], &[u8])> = points
            .iter()
            .zip(payload_bytes.iter())
            .map(|((id, vec, _), p)| (*id, *vec, p.as_slice()))
            .collect();

        self.store.upsert_batch(coll_id, &records)?;

        if is_client_side {
            return Ok(records.len() as u64);
        }

        for (id, vector, payload) in points {
            let handle = self
                .collections
                .get_mut(collection)
                .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
            index_upsert_point(handle, id, vector)?;
            sparse_index_upsert_point(handle, id, payload);
        }
        Ok(records.len() as u64)
    }

    /// Delete a point by id. Returns whether it existed.
    pub fn delete(&mut self, collection: &str, id: &str) -> Result<bool> {
        let handle = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
        require_single_vector(handle)?;
        let existed = self.store.delete(handle.id, id)?;
        if !existed {
            return Ok(false);
        }
        // No server-side index to update for client-side-encrypted collections
        // (ADR-0032); the store delete is authoritative.
        if handle.descriptor.vector_encryption == VectorEncryption::ClientSide {
            return Ok(true);
        }
        // A built IVF removes in place (ADR-0023), a built HNSW soft-deletes
        // (ADR-0026), and a built FreshDiskANN graph tombstones in its deletion set
        // (ADR-0033); other kinds defer to a rebuild. The id->internal mapping is
        // kept so a later re-insert allocates afresh — a removed or soft-deleted
        // internal is simply never returned by the index.
        index_delete_point(handle, id);
        // Drop the point from the derived sparse inverted index too (ADR-0045).
        sparse_index_delete_point(handle, id);
        Ok(true)
    }

    /// Fetch a single point by id, with its payload and vector.
    pub fn get(&self, collection: &str, id: &str) -> Result<Option<Match>> {
        let handle = self.handle(collection)?;
        require_single_vector(handle)?;
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

    /// Fetch points without ranking — an optional cleartext payload `filter`
    /// narrows the set and `limit` bounds it. This is the retrieval path for a
    /// client-side-encrypted collection (ADR-0032): the server returns the entitled
    /// set (each point's payload carries the sealed vector blob under the reserved
    /// `__quiver_vec__` key) and the client decrypts and ranks locally. It also
    /// serves as a general "list points" primitive for any single-vector collection.
    ///
    /// Results come in the store's scan order, not by relevance; the filter is
    /// re-checked exactly against each candidate (a selective filter could use the
    /// secondary index in future — today it scans).
    ///
    /// # Errors
    /// Errors if the collection does not exist or is multi-vector.
    pub fn fetch(
        &self,
        collection: &str,
        filter: Option<&Filter>,
        limit: usize,
        with_payload: bool,
        with_vector: bool,
    ) -> Result<Vec<Match>> {
        let handle = self.handle(collection)?;
        require_single_vector(handle)?;
        let mut out = Vec::new();
        for (id, record) in self.store.scan(handle.id)? {
            if out.len() >= limit {
                break;
            }
            let payload: Value = serde_json::from_slice(&record.payload)?;
            if let Some(filter) = filter
                && !filter.matches(&payload)
            {
                continue;
            }
            out.push(Match {
                id,
                score: 0.0,
                payload: with_payload.then_some(payload),
                vector: with_vector.then_some(record.vector),
            });
        }
        Ok(out)
    }

    /// Search a collection for the nearest points to `query`, optionally
    /// post-filtered by payload predicate.
    pub fn search(
        &mut self,
        collection: &str,
        query: &[f32],
        params: &SearchParams,
    ) -> Result<Vec<Match>> {
        require_single_vector(self.handle(collection)?)?;
        require_server_searchable(self.handle(collection)?)?;
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

    /// Hybrid search (ADR-0043): fuse a dense ANN ranking and a sparse
    /// inverted-index ranking with Reciprocal Rank Fusion. Either query may be
    /// `None` (giving pure dense or pure sparse search through the same path); at
    /// least one is required. The same payload `filter` is re-checked on both
    /// sides, so results stay exact. `rrf_k0` is the RRF rank-bias constant
    /// ([`DEFAULT_RRF_K0`]).
    pub fn hybrid_search(
        &mut self,
        collection: &str,
        dense_query: Option<&[f32]>,
        sparse_query: Option<&SparseVector>,
        params: &SearchParams,
        rrf_k0: f32,
    ) -> Result<Vec<Match>> {
        require_single_vector(self.handle(collection)?)?;
        require_server_searchable(self.handle(collection)?)?;
        if dense_query.is_none() && sparse_query.is_none() {
            return Err(Error::Unsupported(
                "hybrid_search requires a dense query, a sparse query, or both",
            ));
        }
        if self.handle(collection)?.stale {
            let store = &self.store;
            let handle = self
                .collections
                .get_mut(collection)
                .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
            rebuild_index(store, handle)?;
        }
        let handle = self.handle(collection)?;

        // Pull a deep-enough candidate list from each side so the fusion is
        // meaningful, then RRF down to `k`.
        let depth = params
            .k
            .saturating_mul(RRF_CANDIDATE_FACTOR)
            .max(MIN_RRF_CANDIDATES);
        let filter = params.filter.as_ref();
        let mut lists: Vec<Vec<String>> = Vec::new();
        if let Some(q) = dense_query {
            lists.push(self.dense_ranked_ids(handle, q, depth, params.ef_search, filter)?);
        }
        if let Some(sp) = sparse_query {
            lists.push(self.sparse_ranked_ids(handle, sp, depth, filter)?);
        }
        let fused = rrf_fuse(&lists, rrf_k0, params.k);

        let mut out = Vec::with_capacity(fused.len());
        for (ext_id, score) in fused {
            let record = if params.with_payload || params.with_vector {
                self.store.get(handle.id, &ext_id)?
            } else {
                None
            };
            let payload = match (&record, params.with_payload) {
                (Some(r), true) => Some(serde_json::from_slice(&r.payload)?),
                _ => None,
            };
            out.push(Match {
                id: ext_id,
                score,
                payload,
                vector: if params.with_vector {
                    record.map(|r| r.vector)
                } else {
                    None
                },
            });
        }
        Ok(out)
    }

    // Dense candidates as a ranked list of external ids (the filter re-checked),
    // for hybrid fusion.
    fn dense_ranked_ids(
        &self,
        handle: &CollectionHandle,
        query: &[f32],
        depth: usize,
        ef_search: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<String>> {
        let raw = handle.index.search(query, depth, ef_search.max(depth))?;
        let mut ids = Vec::new();
        for neighbor in raw {
            let Some(ext_id) = handle.int_to_ext.get(neighbor.id as usize) else {
                continue;
            };
            if !self.passes_filter(handle.id, ext_id, filter)? {
                continue;
            }
            ids.push(ext_id.clone());
            if ids.len() >= depth {
                break;
            }
        }
        Ok(ids)
    }

    // Sparse candidates as a ranked list of external ids. With the derived inverted
    // index present (the common case), score only the query's nonzero dimensions via
    // the posting lists, then re-check the filter on the ranked ids until `depth` are
    // filled — so low-scored rows never load a payload (ADR-0045). When the index is
    // absent (a not-yet-rebuilt or client-side collection), fall back to the full
    // store scan, which stays correct under the incremental upsert/delete path.
    fn sparse_ranked_ids(
        &self,
        handle: &CollectionHandle,
        query: &SparseVector,
        depth: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<String>> {
        if let Some(idx) = handle.sparse.as_ref() {
            let mut ids = Vec::new();
            for (ext_id, _score) in idx.search(query) {
                if !self.passes_filter(handle.id, &ext_id, filter)? {
                    continue;
                }
                ids.push(ext_id);
                if ids.len() >= depth {
                    break;
                }
            }
            return Ok(ids);
        }
        self.sparse_ranked_ids_by_scan(handle.id, query, depth, filter)
    }

    // The store-scan fallback for [`sparse_ranked_ids`]: load every row, score its
    // `__quiver_sparse__` vector by dot product against the query, re-check the
    // filter, and return the top `depth`. O(N-rows), but correct without an index.
    fn sparse_ranked_ids_by_scan(
        &self,
        cid: CollectionId,
        query: &SparseVector,
        depth: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<String>> {
        let qmap: HashMap<u32, f32> = query
            .indices
            .iter()
            .copied()
            .zip(query.values.iter().copied())
            .collect();
        let mut scored: Vec<(f32, String)> = Vec::new();
        for (ext_id, record) in self.store.scan(cid)? {
            if record.payload.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&record.payload) else {
                continue;
            };
            if let Some(filter) = filter
                && !filter.matches(&value)
            {
                continue;
            }
            let Some(raw) = value.get(SPARSE_KEY) else {
                continue;
            };
            let Ok(sv) = serde_json::from_value::<SparseVector>(raw.clone()) else {
                continue;
            };
            let mut score = 0.0f32;
            for (dim, weight) in sv.indices.iter().zip(sv.values.iter()) {
                if let Some(qw) = qmap.get(dim) {
                    score += qw * weight;
                }
            }
            if score > 0.0 {
                scored.push((score, ext_id));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        Ok(scored.into_iter().take(depth).map(|(_, id)| id).collect())
    }

    // Re-check a payload filter against a row (loading its payload). `None` filter
    // always passes.
    fn passes_filter(
        &self,
        cid: CollectionId,
        ext_id: &str,
        filter: Option<&Filter>,
    ) -> Result<bool> {
        let Some(filter) = filter else {
            return Ok(true);
        };
        let value: Value = match self.store.get(cid, ext_id)? {
            Some(r) => serde_json::from_slice(&r.payload)?,
            None => Value::Null,
        };
        Ok(filter.matches(&value))
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

    /// Insert or replace a multi-vector (late-interaction / ColBERT) document: its
    /// `vectors` are stored as a group of token rows and its `payload` once on the
    /// anchor token (ADR-0028). Re-upserting a document first removes the tokens a
    /// shorter version would leave behind, so the document is replaced cleanly.
    ///
    /// # Errors
    /// Errors if the collection is single-vector, the document has no vectors, a
    /// vector's dimensionality is wrong, or the id contains the reserved separator.
    pub fn upsert_document(
        &mut self,
        collection: &str,
        doc_id: &str,
        vectors: &[Vec<f32>],
        payload: &Value,
    ) -> Result<()> {
        let handle = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
        require_multivector(handle)?;
        if doc_id.contains(DOC_TOKEN_SEP) {
            return Err(Error::Unsupported(
                "document id must not contain the reserved 0x1f separator",
            ));
        }
        if vectors.is_empty() {
            return Err(Error::Unsupported("a document needs at least one vector"));
        }
        let dim = handle.descriptor.dim as usize;
        if vectors.iter().any(|v| v.len() != dim) {
            return Err(Error::Unsupported(
                "every document vector must match the collection dimensionality",
            ));
        }
        // Remove only the trailing tokens a shorter re-upsert leaves behind; the
        // rest are overwritten by the upserts below.
        let previous = handle
            .docs
            .as_ref()
            .and_then(|d| d.get(doc_id))
            .copied()
            .unwrap_or(0) as usize;
        for j in vectors.len()..previous {
            self.store.delete(handle.id, &token_id(doc_id, j))?;
            // Tombstone the dropped token row in the ANN index too (ADR-0034).
            index_delete_point(handle, &token_id(doc_id, j));
        }
        let payload_bytes = serde_json::to_vec(payload)?;
        for (j, vector) in vectors.iter().enumerate() {
            // The payload is stored once, on the anchor token; the rest carry none.
            let bytes: &[u8] = if j == 0 {
                payload_bytes.as_slice()
            } else {
                &[]
            };
            self.store
                .upsert(handle.id, &token_id(doc_id, j), vector, bytes)?;
            // Fold the token row into the ANN index incrementally instead of
            // marking the whole collection stale (ADR-0034); the underlying index
            // consolidates by a rebuild past its own churn threshold.
            index_upsert_point(handle, &token_id(doc_id, j), vector)?;
        }
        if let Some(docs) = handle.docs.as_mut() {
            docs.insert(doc_id.to_owned(), vectors.len() as u32);
        }
        Ok(())
    }

    /// Search a multi-vector collection by a set of query token vectors, ranking
    /// documents by MaxSim late interaction (ADR-0028). At or below the exact-scan
    /// threshold every document is scored exactly; above it, candidates are
    /// generated by nearest-neighbour search over the token pool (recall tuned by
    /// `ef_search`) and re-ranked exactly. An optional `filter` is applied to each
    /// document's payload, exactly. A document has no single vector, so `with_payload`
    /// returns the anchor payload and `with_vector` returns the token vectors.
    pub fn search_multi_vector(
        &mut self,
        collection: &str,
        query_tokens: &[Vec<f32>],
        params: &SearchParams,
    ) -> Result<Vec<DocumentMatch>> {
        require_multivector(self.handle(collection)?)?;
        let dim = self.handle(collection)?.descriptor.dim as usize;
        if query_tokens.is_empty() {
            return Ok(Vec::new());
        }
        if query_tokens.iter().any(|v| v.len() != dim) {
            return Err(Error::Unsupported(
                "every query token must match the collection dimensionality",
            ));
        }

        let doc_count = self
            .handle(collection)?
            .docs
            .as_ref()
            .map_or(0, BTreeMap::len);
        let candidates: Vec<String> = if doc_count <= MULTIVECTOR_EXACT_DOC_THRESHOLD {
            // Exact: score every document. No ANN index needed.
            self.handle(collection)?
                .docs
                .as_ref()
                .map(|d| d.keys().cloned().collect())
                .unwrap_or_default()
        } else {
            // Large corpus: generate candidates from the token pool, rebuilding the
            // ANN index first if a prior write left it stale.
            if self.handle(collection)?.stale {
                let store = &self.store;
                let handle = self
                    .collections
                    .get_mut(collection)
                    .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
                rebuild_index(store, handle)?;
            }
            let handle = self.handle(collection)?;
            let per_token_k = params
                .k
                .saturating_mul(MULTIVECTOR_CANDIDATE_FACTOR)
                .max(params.ef_search);
            let mut set = BTreeSet::new();
            for token in query_tokens {
                for neighbor in handle.index.search(token, per_token_k, params.ef_search)? {
                    if let Some(ext) = handle.int_to_ext.get(neighbor.id as usize)
                        && let Some((doc, _)) = parse_token_id(ext)
                    {
                        set.insert(doc.to_owned());
                    }
                }
            }
            set.into_iter().collect()
        };

        // Re-rank the candidate documents by exact MaxSim over all their tokens.
        let handle = self.handle(collection)?;
        let cid = handle.id;
        let metric = to_index_metric(handle.descriptor.metric);
        let mut scored: Vec<ScoredDocument> = Vec::new();
        for doc in &candidates {
            let count = handle
                .docs
                .as_ref()
                .and_then(|d| d.get(doc))
                .copied()
                .unwrap_or(0) as usize;
            let (tokens, payload) = self.gather_document(cid, doc, count)?;
            if tokens.is_empty() {
                continue;
            }
            if let Some(filter) = &params.filter {
                let value = payload.clone().unwrap_or(Value::Null);
                if !filter.matches(&value) {
                    continue;
                }
            }
            let score = max_sim(metric, query_tokens, &tokens);
            let vectors = params.with_vector.then_some(tokens);
            scored.push((score, doc.clone(), payload, vectors));
        }
        // Higher MaxSim first; ties broken by id for a deterministic order.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.truncate(params.k);
        Ok(scored
            .into_iter()
            .map(|(score, id, payload, vectors)| DocumentMatch {
                id,
                score,
                payload: params.with_payload.then_some(payload).flatten(),
                vectors,
            })
            .collect())
    }

    /// Fetch a multi-vector document by id: its anchor payload and, if
    /// `with_vectors`, its token vectors. `None` if the document does not exist.
    pub fn get_document(
        &self,
        collection: &str,
        doc_id: &str,
        with_vectors: bool,
    ) -> Result<Option<DocumentMatch>> {
        let handle = self.handle(collection)?;
        require_multivector(handle)?;
        let Some(&count) = handle.docs.as_ref().and_then(|d| d.get(doc_id)) else {
            return Ok(None);
        };
        let (tokens, payload) = self.gather_document(handle.id, doc_id, count as usize)?;
        if tokens.is_empty() {
            return Ok(None);
        }
        Ok(Some(DocumentMatch {
            id: doc_id.to_owned(),
            score: 0.0,
            payload,
            vectors: with_vectors.then_some(tokens),
        }))
    }

    /// Delete a multi-vector document and all of its token rows. Returns whether it
    /// existed.
    pub fn delete_document(&mut self, collection: &str, doc_id: &str) -> Result<bool> {
        let handle = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::CollectionNotFound(collection.to_owned()))?;
        require_multivector(handle)?;
        let Some(count) = handle.docs.as_ref().and_then(|d| d.get(doc_id)).copied() else {
            return Ok(false);
        };
        for j in 0..count as usize {
            self.store.delete(handle.id, &token_id(doc_id, j))?;
            // Tombstone each token row in the ANN index incrementally (ADR-0034).
            index_delete_point(handle, &token_id(doc_id, j));
        }
        if let Some(docs) = handle.docs.as_mut() {
            docs.remove(doc_id);
        }
        Ok(true)
    }

    /// The number of documents in a multi-vector collection. Errors if the
    /// collection is single-vector.
    pub fn document_count(&self, collection: &str) -> Result<usize> {
        let handle = self.handle(collection)?;
        require_multivector(handle)?;
        Ok(handle.docs.as_ref().map_or(0, BTreeMap::len))
    }

    // Read a document's token vectors (in ordinal order) and its anchor payload
    // from the store. Missing token rows are skipped, so a torn document yields a
    // short token list the caller treats as empty.
    fn gather_document(
        &self,
        cid: CollectionId,
        doc_id: &str,
        count: usize,
    ) -> Result<(Vec<Vec<f32>>, Option<Value>)> {
        let mut tokens = Vec::with_capacity(count);
        let mut payload: Option<Value> = None;
        for j in 0..count {
            let Some(record) = self.store.get(cid, &token_id(doc_id, j))? else {
                continue;
            };
            if j == 0 && !record.payload.is_empty() {
                payload = Some(serde_json::from_slice(&record.payload)?);
            }
            tokens.push(record.vector);
        }
        Ok((tokens, payload))
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

// The byte separating a multi-vector document id from a token ordinal in a token
// row's external id (`<doc-id><US><ordinal>`): the ASCII Unit Separator, which is
// disallowed in user document ids (ADR-0028).
const DOC_TOKEN_SEP: char = '\u{1f}';

// At or below this document count a multi-vector search scores every document
// exactly; above it, nearest-neighbour candidate generation over the token pool
// kicks in (mirrors the single-vector planner's full-scan threshold).
const MULTIVECTOR_EXACT_DOC_THRESHOLD: usize = 10_000;

// Per-query-token candidate breadth for the large-corpus path: each query token
// retrieves about `k × this` nearest token rows before the documents are unioned.
const MULTIVECTOR_CANDIDATE_FACTOR: usize = 4;

// The external id of a multi-vector document's `ordinal`-th token row.
fn token_id(doc_id: &str, ordinal: usize) -> String {
    format!("{doc_id}{DOC_TOKEN_SEP}{ordinal}")
}

// Split a token row's external id back into its document id and ordinal, or `None`
// if it is not a token id. Splits from the right, so a document id (which cannot
// contain the separator) is recovered intact.
fn parse_token_id(ext: &str) -> Option<(&str, u32)> {
    let (doc, ordinal) = ext.rsplit_once(DOC_TOKEN_SEP)?;
    Some((doc, ordinal.parse().ok()?))
}

// Reject the single-vector API on a multi-vector collection.
fn require_single_vector(handle: &CollectionHandle) -> Result<()> {
    if handle.descriptor.multivector {
        Err(Error::Unsupported(
            "collection is multi-vector; use upsert_document / search_multi_vector",
        ))
    } else {
        Ok(())
    }
}

// Reject the document API on a single-vector collection.
fn require_multivector(handle: &CollectionHandle) -> Result<()> {
    if handle.descriptor.multivector {
        Ok(())
    } else {
        Err(Error::Unsupported(
            "collection is single-vector; use upsert / search",
        ))
    }
}

// Reject server-side ranked search on a client-side-encrypted collection: the
// server holds only opaque ciphertext and cannot rank it. The client fetches the
// entitled set (see `Database::fetch`) and ranks locally (ADR-0032).
fn require_server_searchable(handle: &CollectionHandle) -> Result<()> {
    if handle.descriptor.vector_encryption == VectorEncryption::ClientSide {
        Err(Error::Unsupported(
            "collection is client-side encrypted; the server cannot rank opaque vectors — \
             fetch points and rank client-side",
        ))
    } else {
        Ok(())
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
    // Late interaction (MaxSim) is a similarity, so multi-vector collections need a
    // similarity metric.
    if descriptor.multivector && descriptor.metric == DistanceMetric::L2 {
        return Err(Error::Unsupported(
            "multi-vector collections require a similarity metric (cosine or dot)",
        ));
    }
    // Client-side opaque encryption (ADR-0032) is searched by the client, not the
    // server, so it has no metric or index constraints — but it cannot combine with
    // the multi-vector document layout.
    if descriptor.vector_encryption == VectorEncryption::ClientSide {
        if descriptor.multivector {
            return Err(Error::Unsupported(
                "client-side vector encryption is not supported for multi-vector collections",
            ));
        }
        return Ok(());
    }
    // DCPE (ADR-0031) preserves Euclidean distance comparison; the secret scaling
    // changes vector norms, so cosine and dot orderings are not preserved.
    if descriptor.vector_encryption == VectorEncryption::Dcpe
        && descriptor.metric != DistanceMetric::L2
    {
        return Err(Error::Unsupported(
            "dcpe-encrypted collections require the l2 metric",
        ));
    }
    // ColBERT is a late-interaction token-pool index (ADR-0034): valid only for a
    // multi-vector collection (which already requires a similarity metric).
    if descriptor.index.kind == IndexKind::Colbert && !descriptor.multivector {
        return Err(Error::Unsupported(
            "the colbert index is only for multi-vector collections",
        ));
    }
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
    if descriptor.vector_encryption == VectorEncryption::ClientSide {
        return CollectionIndex::None;
    }
    match descriptor.index.kind {
        IndexKind::Vamana => CollectionIndex::Vamana(None),
        IndexKind::DiskVamana => CollectionIndex::Disk(None),
        IndexKind::Ivf => CollectionIndex::Ivf(None),
        IndexKind::Colbert => CollectionIndex::Colbert(None),
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
    // Client-side-encrypted collections have no server-side index (ADR-0032): the
    // server stores opaque ciphertext it never ranks.
    if descriptor.vector_encryption == VectorEncryption::ClientSide {
        return Ok(CollectionIndex::None);
    }
    let dim = descriptor.dim as usize;
    let metric = to_index_metric(descriptor.metric);
    Ok(match descriptor.index.kind {
        IndexKind::Vamana => CollectionIndex::Vamana(Some(FreshVamana::new(Vamana::build(
            ids,
            flat,
            dim,
            metric,
            VamanaConfig::default(),
        )?)?)),
        IndexKind::DiskVamana => CollectionIndex::Disk(Some(FreshDiskVamana::new(
            build_disk_index(store, cid, descriptor, ids, flat)?,
        )?)),
        IndexKind::Ivf => {
            let cfg = IvfConfig {
                quantization: descriptor.index.pq_subspaces.map(|m| m as usize),
                ..IvfConfig::default()
            };
            CollectionIndex::Ivf(Some(Ivf::build(ids, flat, dim, metric, cfg)?))
        }
        IndexKind::Colbert => {
            // Coarse centroids scale ~√(tokens); probe a quarter of them (PLAID),
            // and PQ the residual with the same subspace default as the disk path.
            let n = ids.len();
            let n_centroids = ((n as f64).sqrt().ceil() as usize).clamp(1, 4096);
            let cfg = ColbertConfig {
                n_centroids,
                n_probe: n_centroids.div_ceil(4).clamp(1, n_centroids),
                pq_subspaces: descriptor
                    .index
                    .pq_subspaces
                    .map_or_else(|| default_pq_m(dim), |m| m as usize),
                seed: PQ_SEED,
            };
            CollectionIndex::Colbert(Some(ColbertIndex::build(ids, flat, dim, metric, cfg)?))
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
    // Multi-vector collections always rebuild on open, so the document grouping is
    // derived from the live rows; the IVF snapshot fast-path stays single-vector.
    if !handle.descriptor.multivector
        && handle.descriptor.index.kind == IndexKind::Ivf
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

// Fold one point write (`ext_id` → `vector`) into a built index incrementally,
// or mark the handle stale to defer to a rebuild when the kind cannot absorb it
// in place. HNSW absorbs a brand-new id but cannot update one (a known id falls
// to a rebuild); a built/trained IVF inserts or replaces any id (ADR-0023); a
// built FreshDiskANN graph appends to its delta and tombstones the prior copy on
// an update, consolidating past its churn threshold (ADR-0033). Shared by the
// single-vector `upsert` and each token row of a multi-vector `upsert_document`
// (ADR-0034). A no-op once the handle is stale (a rebuild is already pending).
fn index_upsert_point(handle: &mut CollectionHandle, ext_id: &str, vector: &[f32]) -> Result<()> {
    if handle.stale {
        return Ok(());
    }
    let known = handle.ext_to_int.contains_key(ext_id);
    let is_hnsw = matches!(handle.index, CollectionIndex::Hnsw(_));
    let is_live_ivf = matches!(&handle.index, CollectionIndex::Ivf(Some(ivf)) if !ivf.is_empty());
    let is_live_graph = matches!(
        handle.index,
        CollectionIndex::Vamana(Some(_)) | CollectionIndex::Disk(Some(_))
    );
    let is_live_colbert = matches!(handle.index, CollectionIndex::Colbert(Some(_)));
    if is_hnsw && !known {
        let internal = handle.int_to_ext.len() as u64;
        if let CollectionIndex::Hnsw(h) = &mut handle.index {
            h.insert(internal, vector)?;
        }
        handle.ext_to_int.insert(ext_id.to_owned(), internal);
        handle.int_to_ext.push(ext_id.to_owned());
    } else if is_live_ivf {
        // Reuse the internal id for an in-place update; allocate a fresh, dense one
        // for a new id (so `int_to_ext` stays index-addressable).
        let internal = if known {
            handle.ext_to_int[ext_id]
        } else {
            let i = handle.int_to_ext.len() as u64;
            handle.ext_to_int.insert(ext_id.to_owned(), i);
            handle.int_to_ext.push(ext_id.to_owned());
            i
        };
        if let CollectionIndex::Ivf(Some(ivf)) = &mut handle.index {
            ivf.insert(internal, vector)?;
        }
    } else if is_live_graph {
        // A graph cannot update a node in place: append a new delta node under a
        // fresh internal id and tombstone the prior copy on an update (ADR-0033).
        let old = handle.ext_to_int.get(ext_id).copied();
        let internal = handle.int_to_ext.len() as u64;
        let mut pending = 0.0;
        match &mut handle.index {
            CollectionIndex::Vamana(Some(fresh)) => {
                if let Some(o) = old {
                    fresh.mark_deleted(o);
                }
                fresh.insert(internal, vector)?;
                pending = fresh.pending_fraction();
            }
            CollectionIndex::Disk(Some(fresh)) => {
                if let Some(o) = old {
                    fresh.mark_deleted(o);
                }
                fresh.insert(internal, vector)?;
                pending = fresh.pending_fraction();
            }
            _ => {}
        }
        handle.ext_to_int.insert(ext_id.to_owned(), internal);
        handle.int_to_ext.push(ext_id.to_owned());
        if pending >= GRAPH_REBUILD_PENDING_FRACTION {
            handle.stale = true;
        }
    } else if is_live_colbert {
        // ColBERT appends a new token and tombstones the prior copy on an update —
        // its centroids are fixed until a rebuild (ADR-0034); the deletion fraction
        // drives consolidation, as for HNSW.
        let old = handle.ext_to_int.get(ext_id).copied();
        let internal = handle.int_to_ext.len() as u64;
        let mut crowded = false;
        if let CollectionIndex::Colbert(Some(c)) = &mut handle.index {
            if let Some(o) = old {
                c.mark_deleted(o);
            }
            c.insert(internal, vector)?;
            crowded = c.deleted_fraction() >= HNSW_REBUILD_DELETED_FRACTION;
        }
        handle.ext_to_int.insert(ext_id.to_owned(), internal);
        handle.int_to_ext.push(ext_id.to_owned());
        if crowded {
            handle.stale = true;
        }
    } else {
        handle.stale = true;
    }
    Ok(())
}

// Tombstone one point (`ext_id`) from a built index incrementally, or mark the
// handle stale to defer to a rebuild. A built IVF removes in place (ADR-0023), a
// built HNSW soft-deletes (ADR-0026), and a built FreshDiskANN graph tombstones in
// its deletion set (ADR-0033); each amortizes a rebuild when tombstones dominate.
// The id→internal mapping is kept so a later re-insert allocates afresh. Shared by
// the single-vector `delete` and each token row of `delete_document` (ADR-0034).
fn index_delete_point(handle: &mut CollectionHandle, ext_id: &str) {
    if handle.stale {
        return;
    }
    let internal = handle.ext_to_int.get(ext_id).copied();
    let live_ivf = matches!(handle.index, CollectionIndex::Ivf(Some(_)));
    let live_hnsw = matches!(handle.index, CollectionIndex::Hnsw(_));
    let live_graph = matches!(
        handle.index,
        CollectionIndex::Vamana(Some(_)) | CollectionIndex::Disk(Some(_))
    );
    let live_colbert = matches!(handle.index, CollectionIndex::Colbert(Some(_)));
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
        Some(internal) if live_graph => {
            let mut crowded = false;
            match &mut handle.index {
                CollectionIndex::Vamana(Some(fresh)) => {
                    fresh.mark_deleted(internal);
                    crowded = fresh.pending_fraction() >= GRAPH_REBUILD_PENDING_FRACTION;
                }
                CollectionIndex::Disk(Some(fresh)) => {
                    fresh.mark_deleted(internal);
                    crowded = fresh.pending_fraction() >= GRAPH_REBUILD_PENDING_FRACTION;
                }
                _ => {}
            }
            if crowded {
                handle.stale = true;
            }
        }
        Some(internal) if live_colbert => {
            let mut crowded = false;
            if let CollectionIndex::Colbert(Some(c)) = &mut handle.index {
                c.mark_deleted(internal);
                crowded = c.deleted_fraction() >= HNSW_REBUILD_DELETED_FRACTION;
            }
            if crowded {
                handle.stale = true;
            }
        }
        _ => handle.stale = true,
    }
}

// Rebuild a collection's index from the store's current live rows. For a
// multi-vector collection it also rebuilds the document grouping (doc id → token
// count) authoritatively from those live rows.
fn rebuild_index(store: &Store, handle: &mut CollectionHandle) -> Result<()> {
    let multivector = handle.descriptor.multivector;
    let mut int_to_ext = Vec::new();
    let mut ext_to_int = HashMap::new();
    let mut flat: Vec<f32> = Vec::new();
    let mut docs: BTreeMap<String, u32> = BTreeMap::new();
    // Rebuild the derived sparse inverted index from the same scan (ADR-0045); only
    // single-vector, server-searchable collections carry one, and only non-empty
    // payloads are parsed, so non-sparse collections pay nothing here.
    let mut sparse = uses_sparse_index(&handle.descriptor).then(SparseInvertedIndex::new);
    for (ext_id, record) in store.scan(handle.id)? {
        let internal = int_to_ext.len() as u64;
        flat.extend_from_slice(&record.vector);
        if multivector && let Some((doc, _)) = parse_token_id(&ext_id) {
            *docs.entry(doc.to_owned()).or_insert(0) += 1;
        }
        if let Some(idx) = sparse.as_mut()
            && let Some(sv) = sparse_vector_from_payload(&record.payload)
        {
            idx.upsert(&ext_id, &sv);
        }
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
    handle.docs = multivector.then_some(docs);
    handle.sparse = sparse;
    handle.stale = false;
    Ok(())
}

// Extract a point's sparse vector from its serialized payload, if it carries one
// under `__quiver_sparse__` and the value deserializes. An empty payload or a
// missing/malformed sparse vector yields `None`.
fn sparse_vector_from_payload(payload: &[u8]) -> Option<SparseVector> {
    if payload.is_empty() {
        return None;
    }
    let value = serde_json::from_slice::<Value>(payload).ok()?;
    sparse_vector_from_value(&value)
}

// As [`sparse_vector_from_payload`] but over an already-parsed payload `Value`
// (the upsert path has the payload before it is serialized).
fn sparse_vector_from_value(payload: &Value) -> Option<SparseVector> {
    let raw = payload.get(SPARSE_KEY)?;
    serde_json::from_value::<SparseVector>(raw.clone()).ok()
}

// Maintain the derived sparse inverted index for one point write (ADR-0045): index
// the point's sparse vector, or drop any prior entry when the new payload no longer
// carries one (an update that removed it). A no-op when the collection has no
// inverted index, or when a rebuild is pending — the rebuild repopulates the index
// authoritatively from the store, exactly as it does the dense index.
fn sparse_index_upsert_point(handle: &mut CollectionHandle, ext_id: &str, payload: &Value) {
    if handle.stale {
        return;
    }
    let Some(idx) = handle.sparse.as_mut() else {
        return;
    };
    match sparse_vector_from_value(payload) {
        Some(sv) => idx.upsert(ext_id, &sv),
        None => {
            idx.remove(ext_id);
        }
    }
}

// Drop one point from the derived sparse inverted index. A no-op when the
// collection has no inverted index.
fn sparse_index_delete_point(handle: &mut CollectionHandle, ext_id: &str) {
    if let Some(idx) = handle.sparse.as_mut() {
        idx.remove(ext_id);
    }
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
    fn hybrid_search_fuses_dense_and_sparse() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("kb", desc()).unwrap();
        // "a" is the nearest dense neighbour of the query; "b" shares the query's
        // sparse terms; "c" is good on neither.
        db.upsert(
            "kb",
            "a",
            &[1.0, 0.0, 0.0, 0.0],
            &json!({ "__quiver_sparse__": { "indices": [100], "values": [0.1] } }),
        )
        .unwrap();
        db.upsert(
            "kb",
            "b",
            &[0.0, 1.0, 0.0, 0.0],
            &json!({ "__quiver_sparse__": { "indices": [1, 2], "values": [5.0, 5.0] } }),
        )
        .unwrap();
        db.upsert(
            "kb",
            "c",
            &[0.0, 0.0, 0.0, 1.0],
            &json!({ "__quiver_sparse__": { "indices": [9], "values": [1.0] } }),
        )
        .unwrap();

        let dense_q = [1.0, 0.0, 0.0, 0.0];
        let sparse_q = SparseVector {
            indices: vec![1, 2],
            values: vec![1.0, 1.0],
        };
        let params = SearchParams {
            k: 3,
            ..SearchParams::default()
        };

        // Hybrid: "a" (dense) and "b" (sparse) both rank above "c" (neither).
        let hits = db
            .hybrid_search(
                "kb",
                Some(&dense_q),
                Some(&sparse_q),
                &params,
                DEFAULT_RRF_K0,
            )
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"b"), "got {ids:?}");
        assert_eq!(ids[2], "c", "c is worst on both sides; got {ids:?}");

        // Pure sparse: only "b" shares the query's terms.
        let sparse_only = db
            .hybrid_search("kb", None, Some(&sparse_q), &params, DEFAULT_RRF_K0)
            .unwrap();
        assert_eq!(sparse_only[0].id, "b");

        // Pure dense: "a" is nearest.
        let dense_only = db
            .hybrid_search("kb", Some(&dense_q), None, &params, DEFAULT_RRF_K0)
            .unwrap();
        assert_eq!(dense_only[0].id, "a");

        // Neither query is an error.
        assert!(
            db.hybrid_search("kb", None, None, &params, DEFAULT_RRF_K0)
                .is_err()
        );
    }

    // Pure-sparse hybrid result ids, in fused order.
    fn sparse_ids(db: &mut Database, q: &SparseVector) -> Vec<String> {
        let params = SearchParams {
            k: 10,
            ..SearchParams::default()
        };
        db.hybrid_search("kb", None, Some(q), &params, DEFAULT_RRF_K0)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    #[test]
    fn sparse_index_equals_the_store_scan_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("kb", desc()).unwrap();
        let z = [0.0f32, 0.0, 0.0, 0.0];
        for (id, dims, vals) in [
            ("a", vec![1u32, 2], vec![5.0f32, 1.0]),
            ("b", vec![2u32, 3], vec![3.0f32, 4.0]),
            ("c", vec![1u32, 3], vec![2.0f32, 2.0]),
            ("d", vec![9u32], vec![1.0f32]), // shares no query term
        ] {
            db.upsert(
                "kb",
                id,
                &z,
                &json!({ "__quiver_sparse__": { "indices": dims, "values": vals } }),
            )
            .unwrap();
        }
        let q = SparseVector {
            indices: vec![1, 2, 3],
            values: vec![1.0, 1.0, 1.0],
        };

        // The derived index is present and used.
        assert!(db.collections.get("kb").unwrap().sparse.is_some());
        let via_index = sparse_ids(&mut db, &q);
        assert!(!via_index.contains(&"d".to_owned()), "d shares no term");

        // Drop the index (not stale, so no rebuild) → the store-scan fallback runs
        // and must return the identical ranking.
        db.collections.get_mut("kb").unwrap().sparse = None;
        let via_scan = sparse_ids(&mut db, &q);
        assert_eq!(via_index, via_scan);
    }

    #[test]
    fn sparse_index_reflects_updates_and_deletes_like_a_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("kb", desc()).unwrap();
        let z = [0.0f32, 0.0, 0.0, 0.0];
        db.upsert(
            "kb",
            "a",
            &z,
            &json!({ "__quiver_sparse__": { "indices": [1, 2], "values": [5.0, 5.0] } }),
        )
        .unwrap();
        db.upsert(
            "kb",
            "b",
            &z,
            &json!({ "__quiver_sparse__": { "indices": [2], "values": [3.0] } }),
        )
        .unwrap();
        db.upsert(
            "kb",
            "c",
            &z,
            &json!({ "__quiver_sparse__": { "indices": [1], "values": [9.0] } }),
        )
        .unwrap();
        // Update "a" onto a disjoint term; delete "b".
        db.upsert(
            "kb",
            "a",
            &z,
            &json!({ "__quiver_sparse__": { "indices": [7], "values": [1.0] } }),
        )
        .unwrap();
        assert!(db.delete("kb", "b").unwrap());

        let q = SparseVector {
            indices: vec![1, 2],
            values: vec![1.0, 1.0],
        };
        // Only "c" still shares a query term ("a" moved to 7, "b" is gone).
        let incremental = sparse_ids(&mut db, &q);
        assert_eq!(incremental, vec!["c".to_owned()]);

        // A full rebuild from the store must agree with the incremental state.
        db.collections.get_mut("kb").unwrap().stale = true;
        let rebuilt = sparse_ids(&mut db, &q);
        assert_eq!(incremental, rebuilt);
    }

    #[test]
    fn sparse_index_is_rebuilt_on_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path());
            db.create_collection("kb", desc()).unwrap();
            db.upsert(
                "kb",
                "a",
                &[0.0, 0.0, 0.0, 0.0],
                &json!({ "__quiver_sparse__": { "indices": [1], "values": [1.0] } }),
            )
            .unwrap();
        }
        let mut db = open(tmp.path());
        assert!(db.collections.get("kb").unwrap().sparse.is_some());
        let q = SparseVector {
            indices: vec![1],
            values: vec![1.0],
        };
        assert_eq!(sparse_ids(&mut db, &q), vec!["a".to_owned()]);
    }

    #[test]
    fn hybrid_sparse_honours_the_payload_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("kb", desc()).unwrap();
        let z = [0.0f32, 0.0, 0.0, 0.0];
        db.upsert(
            "kb",
            "a",
            &z,
            &json!({ "lang": "en", "__quiver_sparse__": { "indices": [1], "values": [5.0] } }),
        )
        .unwrap();
        db.upsert(
            "kb",
            "b",
            &z,
            &json!({ "lang": "fr", "__quiver_sparse__": { "indices": [1], "values": [9.0] } }),
        )
        .unwrap();
        let q = SparseVector {
            indices: vec![1],
            values: vec![1.0],
        };
        let params = SearchParams {
            k: 10,
            filter: Some(Filter::Eq {
                field: "lang".to_owned(),
                value: json!("en"),
            }),
            ..SearchParams::default()
        };
        let hits: Vec<String> = db
            .hybrid_search("kb", None, Some(&q), &params, DEFAULT_RRF_K0)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        // "b" scores higher but is filtered out by lang == "en".
        assert_eq!(hits, vec!["a".to_owned()]);
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
    fn upsert_batch_produces_same_search_results_as_sequential() {
        let tmp_seq = tempfile::tempdir().unwrap();
        let tmp_bat = tempfile::tempdir().unwrap();

        let vectors: Vec<[f32; 4]> = (0..20u32).map(|i| [i as f32, 0.0, 0.0, 0.0]).collect();
        let ids: Vec<String> = (0..20u32).map(|i| format!("p{i}")).collect();
        let payload = json!({});

        // Sequential upserts
        {
            let mut db = open(tmp_seq.path());
            db.create_collection("c", desc()).unwrap();
            for (id, vec) in ids.iter().zip(vectors.iter()) {
                db.upsert("c", id, vec, &payload).unwrap();
            }
        }
        // Batch upsert
        {
            let mut db = open(tmp_bat.path());
            db.create_collection("c", desc()).unwrap();
            let pts: Vec<(&str, &[f32], &serde_json::Value)> = ids
                .iter()
                .zip(vectors.iter())
                .map(|(id, v)| (id.as_str(), v.as_slice(), &payload))
                .collect();
            let n = db.upsert_batch("c", &pts).unwrap();
            assert_eq!(n, 20);
        }

        let query = [10.0f32, 0.0, 0.0, 0.0];
        let params = SearchParams {
            k: 5,
            ..Default::default()
        };

        let mut seq_db = open(tmp_seq.path());
        let mut bat_db = open(tmp_bat.path());
        let seq: Vec<String> = seq_db
            .search("c", &query, &params)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        let bat: Vec<String> = bat_db
            .search("c", &query, &params)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            seq, bat,
            "batch and sequential produce different search results"
        );
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

    #[test]
    fn dcpe_collections_require_the_l2_metric() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        // DCPE preserves Euclidean distance, so cosine/dot are rejected.
        for metric in [DistanceMetric::Cosine, DistanceMetric::Dot] {
            let bad = Descriptor::new(4, Dtype::F32, metric)
                .with_vector_encryption(VectorEncryption::Dcpe);
            assert!(matches!(
                db.create_collection("bad", bad),
                Err(Error::Unsupported(_))
            ));
        }
        // L2 is accepted, and the flag persists on the descriptor.
        let good = Descriptor::new(4, Dtype::F32, DistanceMetric::L2)
            .with_vector_encryption(VectorEncryption::Dcpe);
        db.create_collection("enc", good)
            .expect("l2 dcpe collection");
        assert_eq!(
            db.descriptor("enc").expect("descriptor").vector_encryption,
            VectorEncryption::Dcpe
        );
    }

    #[test]
    fn client_side_collections_are_fetch_only_and_reject_search() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        // Any metric is allowed (the server never ranks), so there is no L2
        // restriction as there is for DCPE.
        let desc = Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine)
            .with_vector_encryption(VectorEncryption::ClientSide);
        db.create_collection("vault", desc)
            .expect("create client-side collection");
        // No server-side index is built for a client-side collection (ADR-0032).
        assert!(matches!(
            db.collections["vault"].index,
            CollectionIndex::None
        ));

        // Upsert opaque placeholder points: a zero vector plus a payload carrying a
        // stand-in sealed vector blob and a cleartext, server-filterable field.
        for i in 0..5 {
            let tier = if i < 2 { "vip" } else { "std" };
            db.upsert(
                "vault",
                &format!("p{i}"),
                &[0.0; 4],
                &serde_json::json!({ "__quiver_vec__": format!("ct-{i}"), "tier": tier }),
            )
            .expect("upsert");
        }
        assert_eq!(db.len("vault").unwrap(), 5);
        // Still no index after writes — it never goes stale or rebuilds.
        assert!(matches!(
            db.collections["vault"].index,
            CollectionIndex::None
        ));

        // Ranked search is rejected: the server cannot rank opaque vectors.
        assert!(matches!(
            db.search("vault", &[0.0; 4], &SearchParams::default()),
            Err(Error::Unsupported(_))
        ));

        // Fetch returns the entitled set; each payload carries the blob the client
        // would decrypt and rank locally, and vectors are omitted when not asked for.
        let all = db.fetch("vault", None, 100, true, false).unwrap();
        assert_eq!(all.len(), 5);
        assert!(
            all.iter()
                .all(|m| m.payload.is_some() && m.vector.is_none())
        );

        // A cleartext payload filter narrows the set server-side.
        let vip = db
            .fetch(
                "vault",
                Some(&Filter::Eq {
                    field: "tier".to_owned(),
                    value: serde_json::json!("vip"),
                }),
                100,
                false,
                false,
            )
            .unwrap();
        assert_eq!(vip.len(), 2);
        // A limit bounds the returned set.
        assert_eq!(db.fetch("vault", None, 2, false, false).unwrap().len(), 2);

        // get returns the stored placeholder + blob payload by id; delete works
        // through the store with no index to update.
        assert_eq!(db.get("vault", "p0").unwrap().unwrap().id, "p0");
        assert!(db.delete("vault", "p0").unwrap());
        assert_eq!(db.len("vault").unwrap(), 4);
    }

    #[test]
    fn client_side_encryption_rejects_multivector() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        let desc = Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine)
            .with_multivector(true)
            .with_vector_encryption(VectorEncryption::ClientSide);
        assert!(matches!(
            db.create_collection("bad", desc),
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

    #[test]
    fn graph_collections_maintain_writes_incrementally() {
        // FreshDiskANN incremental maintenance (ADR-0033): after the base graph is
        // built, inserts land in the delta, deletes tombstone, and updates move —
        // all without a rebuild or a reopen, for both the in-memory and disk graphs.
        for kind in [IndexKind::Vamana, IndexKind::DiskVamana] {
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
            // The first search builds the read-only base graph.
            let res = db
                .search("c", &[7.0, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert_eq!(res[0].id, "p7", "{kind:?} base nearest");

            // A brand-new point lands in the in-memory delta and is immediately
            // findable — no rebuild, no reopen.
            db.upsert("c", "p7b", &[7.4, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
            let res = db
                .search("c", &[7.45, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert_eq!(res[0].id, "p7b", "{kind:?} delta insert not found");

            // A delete tombstones the point: it is never returned again.
            assert!(db.delete("c", "p7").unwrap());
            let res = db
                .search("c", &[7.0, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert!(
                res.iter().all(|m| m.id != "p7"),
                "{kind:?} deleted id returned"
            );

            // An update moves a vector: it is found at the new position and no
            // longer dominates the old one.
            db.upsert("c", "p20", &[500.0, 0.0, 0.0, 0.0], &json!({}))
                .unwrap();
            let res = db
                .search("c", &[500.0, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert_eq!(res[0].id, "p20", "{kind:?} updated vector not at new spot");
            let res = db
                .search("c", &[20.0, 0.0, 0.0, 0.0], &SearchParams::default())
                .unwrap();
            assert_ne!(
                res[0].id, "p20",
                "{kind:?} stale copy still nearest old spot"
            );
        }
    }

    #[test]
    fn graph_consolidates_under_heavy_churn() {
        // Churn past the 20% pending threshold forces a consolidation (a derived
        // rebuild from the store); results stay correct throughout and across a
        // reopen, since the store is the source of truth (ADR-0033).
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("c", desc_with(IndexKind::Vamana))
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
        let _ = db.search("c", &[0.0; 4], &SearchParams::default()).unwrap();

        let deleted: Vec<String> = (0..15u32).map(|i| format!("p{i}")).collect();
        for i in 0..15u32 {
            assert!(db.delete("c", &format!("p{i}")).unwrap());
            db.upsert(
                "c",
                &format!("q{i}"),
                &[1000.0 + i as f32, 0.0, 0.0, 0.0],
                &json!({}),
            )
            .unwrap();
        }

        let near_origin = db
            .search("c", &[5.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert!(
            near_origin.iter().all(|m| !deleted.contains(&m.id)),
            "a churned-out id was returned"
        );
        let near_q = db
            .search("c", &[1007.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(near_q[0].id, "q7", "new point not found after churn");

        db.checkpoint().unwrap();
        drop(db);
        let mut db = open(tmp.path());
        let near_q = db
            .search("c", &[1007.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(near_q[0].id, "q7", "new point lost across reopen");
        let near_origin = db
            .search("c", &[5.0, 0.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert!(
            near_origin.iter().all(|m| !deleted.contains(&m.id)),
            "a churned-out id resurfaced after reopen"
        );
    }

    #[test]
    fn multivector_writes_are_incremental_and_match_a_rebuild() {
        // Token rows fold into the ANN index incrementally instead of stale->rebuild
        // (ADR-0034); the document API stays correct under interleaved
        // upsert/delete/re-upsert with no reopen, and a reopen (the authoritative
        // rebuild) yields the identical ranking. Docs lie on an arc of increasing
        // angle to the query (cosine, so direction alone ranks them), making the
        // expected order unambiguous. (Token-pool candidate generation only engages
        // above the exact-scan threshold; the incremental token maintenance it
        // relies on is the same code the single-vector index tests exercise.)
        let dir = |theta: f32| vec![theta.cos(), theta.sin(), 0.0, 0.0];
        let doc = |theta: f32| vec![dir(theta), dir(theta)];
        for kind in [
            IndexKind::Ivf,
            IndexKind::Hnsw,
            IndexKind::Vamana,
            IndexKind::Colbert,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let desc = Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine)
                .with_multivector(true)
                .with_index(IndexSpec {
                    kind,
                    pq_subspaces: None,
                });
            let mut db = open(tmp.path());
            db.create_collection("m", desc).unwrap();
            // d1 is most aligned with the query (smallest angle), d10 least.
            for i in 1..=10u32 {
                db.upsert_document(
                    "m",
                    &format!("d{i}"),
                    &doc(0.1 * i as f32),
                    &json!({ "i": i }),
                )
                .unwrap();
            }
            let q = vec![dir(0.0)];
            let top = |db: &mut Database| {
                db.search_multi_vector(
                    "m",
                    &q,
                    &SearchParams {
                        k: 3,
                        ..Default::default()
                    },
                )
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect::<Vec<_>>()
            };
            assert_eq!(top(&mut db), vec!["d1", "d2", "d3"], "{kind:?} initial");

            // Delete the top document — gone without a reopen.
            assert!(db.delete_document("m", "d1").unwrap());
            assert_eq!(
                top(&mut db),
                vec!["d2", "d3", "d4"],
                "{kind:?} after delete"
            );

            // Re-upsert the least-aligned doc onto the query — the update is live.
            db.upsert_document("m", "d10", &doc(0.0), &json!({ "i": 10 }))
                .unwrap();
            assert_eq!(top(&mut db)[0], "d10", "{kind:?} after update");

            // A new, near-aligned document is immediately findable, second only to d10.
            db.upsert_document("m", "d11", &doc(0.05), &json!({ "i": 11 }))
                .unwrap();
            let r = top(&mut db);
            assert_eq!(r[0], "d10", "{kind:?}");
            assert_eq!(r[1], "d11", "{kind:?} new doc not ranked");

            // A shorter re-upsert drops the trailing token row.
            db.upsert_document("m", "d8", &[dir(0.8)], &json!({ "i": 8 }))
                .unwrap();
            let d8 = db.get_document("m", "d8", true).unwrap().unwrap();
            assert_eq!(d8.vectors.unwrap().len(), 1, "{kind:?} trailing token kept");

            // The incremental in-memory state matches an authoritative rebuild.
            let before = top(&mut db);
            drop(db);
            let mut db = open(tmp.path());
            assert_eq!(top(&mut db), before, "{kind:?} incremental != rebuild");
            assert!(
                db.get_document("m", "d1", false).unwrap().is_none(),
                "{kind:?} deleted doc resurfaced"
            );
        }
    }

    #[test]
    fn colbert_index_requires_multivector() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        // ColBERT is a late-interaction token-pool index — invalid for a
        // single-vector collection (ADR-0034).
        let single = Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine).with_index(IndexSpec {
            kind: IndexKind::Colbert,
            pq_subspaces: None,
        });
        assert!(matches!(
            db.create_collection("c", single),
            Err(Error::Unsupported(_))
        ));
        // ...but valid for a multi-vector collection.
        let multi = Descriptor::new(4, Dtype::F32, DistanceMetric::Cosine)
            .with_multivector(true)
            .with_index(IndexSpec {
                kind: IndexKind::Colbert,
                pq_subspaces: None,
            });
        assert!(db.create_collection("m", multi).is_ok());
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

    // ---- Multi-vector / late interaction (ColBERT, ADR-0028) ----

    fn mv_desc() -> Descriptor {
        Descriptor::new(3, Dtype::F32, DistanceMetric::Cosine).with_multivector(true)
    }

    // Brute-force MaxSim ranking over a corpus: the reference the pipeline must
    // reproduce (score desc, ties by id), using the same shared scorer.
    fn bf_rank(query: &[Vec<f32>], corpus: &[(&str, Vec<Vec<f32>>)]) -> Vec<(String, f32)> {
        let mut v: Vec<(String, f32)> = corpus
            .iter()
            .map(|(id, toks)| ((*id).to_owned(), max_sim(Metric::Cosine, query, toks)))
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    #[test]
    fn multivector_search_ranks_documents_by_maxsim() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("docs", mv_desc()).unwrap();
        let corpus: Vec<(&str, Vec<Vec<f32>>)> = vec![
            ("d_cat", vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]),
            ("d_dog", vec![vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]]),
            (
                "d_mix",
                vec![
                    vec![1.0, 1.0, 0.0],
                    vec![0.0, 0.0, 1.0],
                    vec![1.0, 0.0, 1.0],
                ],
            ),
        ];
        for (id, toks) in &corpus {
            db.upsert_document("docs", id, toks, &json!({ "id": id }))
                .unwrap();
        }
        assert_eq!(db.document_count("docs").unwrap(), 3);

        let query = vec![vec![1.0, 0.0, 0.0], vec![0.0, 0.0, 1.0]];
        let params = SearchParams {
            k: 3,
            with_payload: false,
            ..SearchParams::default()
        };
        let got = db.search_multi_vector("docs", &query, &params).unwrap();
        let expected = bf_rank(&query, &corpus);

        assert_eq!(got.len(), 3);
        for (g, (eid, escore)) in got.iter().zip(expected.iter()) {
            assert_eq!(&g.id, eid, "ranking matches brute force");
            assert!(
                (g.score - escore).abs() < 1e-5,
                "{} score {} vs {escore}",
                g.id,
                g.score
            );
        }
    }

    #[test]
    fn multivector_search_truncates_to_k() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("docs", mv_desc()).unwrap();
        for i in 0..5 {
            let v = vec![vec![1.0, i as f32, 0.0]];
            db.upsert_document("docs", &format!("d{i}"), &v, &json!({}))
                .unwrap();
        }
        let params = SearchParams {
            k: 2,
            ..SearchParams::default()
        };
        let got = db
            .search_multi_vector("docs", &[vec![1.0, 0.0, 0.0]], &params)
            .unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn multivector_filter_selects_documents_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("docs", mv_desc()).unwrap();
        // Identical token sets, so only the filter distinguishes the documents.
        db.upsert_document("docs", "a", &[vec![1.0, 0.0, 0.0]], &json!({"lang":"en"}))
            .unwrap();
        db.upsert_document("docs", "b", &[vec![1.0, 0.0, 0.0]], &json!({"lang":"fr"}))
            .unwrap();
        let params = SearchParams {
            k: 10,
            filter: Some(Filter::Eq {
                field: "lang".into(),
                value: json!("fr"),
            }),
            ..SearchParams::default()
        };
        let got = db
            .search_multi_vector("docs", &[vec![1.0, 0.0, 0.0]], &params)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "b");
        assert_eq!(got[0].payload, Some(json!({"lang":"fr"})));
    }

    #[test]
    fn multivector_reopen_rebuilds_grouping_and_ranking() {
        let tmp = tempfile::tempdir().unwrap();
        let query = vec![vec![1.0, 0.0, 0.0], vec![0.0, 0.0, 1.0]];
        let corpus: Vec<(&str, Vec<Vec<f32>>)> = vec![
            ("x", vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]),
            ("y", vec![vec![0.0, 0.0, 1.0], vec![1.0, 0.0, 1.0]]),
        ];
        {
            let mut db = open(tmp.path());
            db.create_collection("docs", mv_desc()).unwrap();
            for (id, toks) in &corpus {
                db.upsert_document("docs", id, toks, &json!({})).unwrap();
            }
            db.checkpoint().unwrap();
        }
        // Reopen: the document grouping is rebuilt from the live rows.
        let mut db = open(tmp.path());
        assert_eq!(db.document_count("docs").unwrap(), 2);
        let params = SearchParams {
            k: 2,
            ..SearchParams::default()
        };
        let got = db.search_multi_vector("docs", &query, &params).unwrap();
        let expected = bf_rank(&query, &corpus);
        assert_eq!(
            got.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn multivector_delete_document_removes_all_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("docs", mv_desc()).unwrap();
        db.upsert_document(
            "docs",
            "a",
            &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
            &json!({}),
        )
        .unwrap();
        db.upsert_document("docs", "b", &[vec![0.0, 0.0, 1.0]], &json!({}))
            .unwrap();
        assert_eq!(db.document_count("docs").unwrap(), 2);
        assert_eq!(db.len("docs").unwrap(), 3);

        assert!(db.delete_document("docs", "a").unwrap());
        assert_eq!(db.document_count("docs").unwrap(), 1);
        assert_eq!(db.len("docs").unwrap(), 1);
        assert!(db.get_document("docs", "a", false).unwrap().is_none());
        let params = SearchParams {
            k: 10,
            ..SearchParams::default()
        };
        let got = db
            .search_multi_vector("docs", &[vec![1.0, 0.0, 0.0]], &params)
            .unwrap();
        assert!(got.iter().all(|m| m.id != "a"));
        assert!(!db.delete_document("docs", "a").unwrap());
    }

    #[test]
    fn multivector_reupsert_replaces_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("docs", mv_desc()).unwrap();
        db.upsert_document(
            "docs",
            "a",
            &[
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ],
            &json!({"v":1}),
        )
        .unwrap();
        assert_eq!(db.len("docs").unwrap(), 3);
        // Re-upsert with a single token: the trailing two must be gone.
        db.upsert_document("docs", "a", &[vec![0.0, 0.0, 1.0]], &json!({"v":2}))
            .unwrap();
        assert_eq!(db.document_count("docs").unwrap(), 1);
        assert_eq!(db.len("docs").unwrap(), 1);
        let doc = db.get_document("docs", "a", true).unwrap().unwrap();
        assert_eq!(doc.payload, Some(json!({"v":2})));
        assert_eq!(doc.vectors, Some(vec![vec![0.0, 0.0, 1.0]]));
    }

    #[test]
    fn single_and_multi_vector_apis_are_mutually_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        db.create_collection("mv", mv_desc()).unwrap();
        db.create_collection("sv", Descriptor::new(3, Dtype::F32, DistanceMetric::Cosine))
            .unwrap();
        // Single-vector ops on a multi-vector collection are rejected.
        assert!(matches!(
            db.upsert("mv", "a", &[1.0, 0.0, 0.0], &json!({})),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            db.search("mv", &[1.0, 0.0, 0.0], &SearchParams::default()),
            Err(Error::Unsupported(_))
        ));
        // Document ops on a single-vector collection are rejected.
        assert!(matches!(
            db.upsert_document("sv", "a", &[vec![1.0, 0.0, 0.0]], &json!({})),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            db.search_multi_vector("sv", &[vec![1.0, 0.0, 0.0]], &SearchParams::default()),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            db.document_count("sv"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn multivector_rejects_l2_metric_and_bad_documents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = open(tmp.path());
        let l2 = Descriptor::new(3, Dtype::F32, DistanceMetric::L2).with_multivector(true);
        assert!(matches!(
            db.create_collection("bad", l2),
            Err(Error::Unsupported(_))
        ));

        db.create_collection("docs", mv_desc()).unwrap();
        // A document id may not contain the reserved separator.
        assert!(matches!(
            db.upsert_document("docs", "a\u{1f}b", &[vec![1.0, 0.0, 0.0]], &json!({})),
            Err(Error::Unsupported(_))
        ));
        // A document needs at least one vector, of the right dimensionality.
        assert!(matches!(
            db.upsert_document("docs", "a", &[], &json!({})),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            db.upsert_document("docs", "a", &[vec![1.0, 0.0]], &json!({})),
            Err(Error::Unsupported(_))
        ));
    }
}
