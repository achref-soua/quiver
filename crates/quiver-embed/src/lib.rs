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
//! the store on open. HNSW applies new-id inserts incrementally; an update, a
//! delete, or any write to a batch index (Vamana / IVF, built over the whole
//! collection) marks the index stale, and the next search rebuilds it — so batch
//! indexes suit bulk-load-then-query. In-place incremental update for the disk
//! graph is Phase 4 (SpFresh).
//!
//! ## Concurrency (Phase 1)
//! Single-writer: every operation takes `&mut self` (a search may rebuild a
//! stale index). A server serializes access behind a lock; the lock-free MVCC
//! snapshot model (ADR-0006) arrives with Phase 2.

use std::collections::HashMap;
use std::path::Path;

use quiver_core::{CollectionId, Store};
use quiver_index::{
    Hnsw, HnswConfig, Index, Ivf, IvfConfig, Metric, Neighbor, Vamana, VamanaConfig,
};
use serde_json::Value;
use thiserror::Error;

pub use quiver_core::page::PageCodec;
pub use quiver_core::{Descriptor, DistanceMetric, Dtype, IndexKind, IndexSpec};
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
    /// A payload could not be (de)serialized as JSON.
    #[error("payload json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The named collection is not loaded in this database.
    #[error("collection not found: {0}")]
    CollectionNotFound(String),
    /// The requested index / metric combination is not supported.
    #[error("unsupported configuration: {0}")]
    Unsupported(&'static str),
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
    /// Optional payload predicate (post-filtered in Phase 1).
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

// The vector index backing one collection. HNSW is incremental; Vamana and IVF
// are batch-built from the store (the `Option` is `None` until first build), so
// they suit bulk-load-then-query and rebuild lazily after writes.
enum CollectionIndex {
    Hnsw(Hnsw),
    Vamana(Option<Vamana>),
    Ivf(Option<Ivf>),
}

impl CollectionIndex {
    // Search, mapping the generic `ef` knob onto each index's search width.
    fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
    ) -> std::result::Result<Vec<Neighbor>, quiver_index::IndexError> {
        match self {
            CollectionIndex::Hnsw(h) => h.search(query, k, ef),
            CollectionIndex::Vamana(Some(g)) => g.search(query, k, ef),
            CollectionIndex::Ivf(Some(i)) => i.search(query, k, ef),
            CollectionIndex::Vamana(None) | CollectionIndex::Ivf(None) => Ok(Vec::new()),
        }
    }
}

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
            rebuild_index(&store, &mut handle)?;
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
        // Only an in-memory HNSW can absorb a brand-new id incrementally; an
        // existing id (no in-place update), a batch index, or an already-stale
        // index defers to a lazy rebuild on the next search.
        let new_id = !handle.ext_to_int.contains_key(id);
        let incremental =
            !handle.stale && new_id && matches!(handle.index, CollectionIndex::Hnsw(_));
        if incremental {
            let internal = handle.int_to_ext.len() as u64;
            if let CollectionIndex::Hnsw(h) = &mut handle.index {
                h.insert(internal, vector)?;
            }
            handle.ext_to_int.insert(id.to_owned(), internal);
            handle.int_to_ext.push(id.to_owned());
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
        if existed {
            handle.stale = true;
        }
        Ok(existed)
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

    /// Flush a durable checkpoint of all collections.
    pub fn checkpoint(&mut self) -> Result<()> {
        Ok(self.store.checkpoint()?)
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
        IndexKind::DiskVamana => Err(Error::Unsupported(
            "the disk-resident index is not yet available in this build; use vamana or ivf",
        )),
        IndexKind::Vamana | IndexKind::Ivf if descriptor.metric == DistanceMetric::Dot => Err(
            Error::Unsupported("vamana and ivf support l2 and cosine; use hnsw for dot"),
        ),
        _ => Ok(()),
    }
}

// An empty index of the kind the descriptor selects. Batch kinds start unbuilt.
fn empty_index(descriptor: &Descriptor) -> CollectionIndex {
    match descriptor.index.kind {
        IndexKind::Vamana | IndexKind::DiskVamana => CollectionIndex::Vamana(None),
        IndexKind::Ivf => CollectionIndex::Ivf(None),
        _ => CollectionIndex::Hnsw(Hnsw::new(
            descriptor.dim as usize,
            to_index_metric(descriptor.metric),
            HnswConfig::default(),
        )),
    }
}

// Build the descriptor's index over `ids` (internal 0..n) and their flat vectors.
fn build_index(descriptor: &Descriptor, ids: &[u64], flat: &[f32]) -> Result<CollectionIndex> {
    let dim = descriptor.dim as usize;
    let metric = to_index_metric(descriptor.metric);
    Ok(match descriptor.index.kind {
        // DiskVamana is gated at creation; until its disk wiring lands it would
        // build the in-memory Vamana graph (same recall, not yet frugal).
        IndexKind::Vamana | IndexKind::DiskVamana => CollectionIndex::Vamana(Some(Vamana::build(
            ids,
            flat,
            dim,
            metric,
            VamanaConfig::default(),
        )?)),
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
    handle.index = build_index(&handle.descriptor, &ids, &flat)?;
    handle.int_to_ext = int_to_ext;
    handle.ext_to_int = ext_to_int;
    handle.stale = false;
    Ok(())
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
        // The disk-resident index is gated until its wiring lands.
        assert!(matches!(
            db.create_collection("b", desc_with(IndexKind::DiskVamana)),
            Err(Error::Unsupported(_))
        ));
    }
}
