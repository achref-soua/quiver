// SPDX-License-Identifier: AGPL-3.0-only
//! The storage engine: a durable, crash-safe key/vector store per collection.
//!
//! A [`Store`] ties the [`crate::wal`] and [`crate::manifest`] primitives,
//! together with internal sealed segments, into a recoverable engine. The
//! durability contract (ADR-0005): a mutation is acknowledged only after its WAL
//! record is `fsync`'d, so an acknowledged write survives `kill -9`.
//!
//! ## Write path
//! `upsert`/`delete`/`create_collection`/`drop_collection` append a WAL record,
//! `fsync` it (acknowledgement), then update in-memory state. `checkpoint` seals
//! the rows changed since the last checkpoint into a new immutable segment per
//! collection, atomically swaps in a new manifest that references them, rotates
//! the WAL, and garbage-collects superseded files.
//!
//! ## Recovery (on open)
//! Read `CURRENT` → load the manifest → rebuild live rows from the referenced
//! segments → replay every WAL record with `lsn > last_checkpointed_lsn`
//! idempotently → garbage-collect orphan segment files a crash left between a
//! flush and the manifest swap. A torn trailing WAL record fails its frame check
//! and is dropped; it was never acknowledged.
//!
//! ## Concurrency
//! Phase 1 is a single-writer engine: mutations take `&mut self`, reads take
//! `&self`. The lock-free MVCC snapshot model (ADR-0006) is introduced with the
//! index/server integration; until then a server wraps the store in a lock.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::descriptor::Descriptor;
use crate::error::{CoreError, Result};
use crate::ids::{CollectionId, Lsn};
use crate::manifest::{self, CollectionEntry, MANIFEST_FORMAT_VERSION, Manifest, SegmentRef};
use crate::page::{PageCodec, PlainCodec};
use crate::paged::fsync_dir;
use crate::segment::{self, SEGMENT_FORMAT_VERSION, SegmentData, SegmentRow};
use crate::wal::{self, WalEntry, WalOp, WalWriter};

/// A stored record returned by reads: the decoded vector and opaque payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The vector, decoded from its on-disk little-endian bytes.
    pub vector: Vec<f32>,
    /// The opaque payload bytes (validated UTF-8 JSON at the API edge).
    pub payload: Vec<u8>,
}

// A live row in memory: raw little-endian vector bytes plus payload bytes.
#[derive(Debug, Clone)]
struct Row {
    vector: Vec<u8>,
    payload: Vec<u8>,
}

// In-memory state of one collection.
struct CollectionState {
    id: CollectionId,
    name: String,
    descriptor: Descriptor,
    // Authoritative live rows, used by reads.
    live: BTreeMap<String, Row>,
    // Ids upserted since the last checkpoint (each still present in `live`).
    dirty: BTreeSet<String>,
    // Ids deleted since the last checkpoint (to tombstone older segments).
    deleted: BTreeSet<String>,
    // Sealed segments, in creation order (mirrors the manifest entry).
    segments: Vec<SegmentRef>,
}

impl CollectionState {
    fn new(id: CollectionId, name: String, descriptor: Descriptor) -> Self {
        Self {
            id,
            name,
            descriptor,
            live: BTreeMap::new(),
            dirty: BTreeSet::new(),
            deleted: BTreeSet::new(),
            segments: Vec::new(),
        }
    }

    fn has_pending(&self) -> bool {
        !self.dirty.is_empty() || !self.deleted.is_empty()
    }
}

/// The durable storage engine for one data directory.
pub struct Store {
    dir: PathBuf,
    codec: Box<dyn PageCodec>,
    collections: HashMap<CollectionId, CollectionState>,
    name_index: HashMap<String, CollectionId>,
    next_lsn: Lsn,
    next_collection_id: u64,
    next_segment_id: u64,
    manifest_version: u64,
    last_checkpointed_lsn: Lsn,
    wal: WalWriter,
    wal_seq: u64,
}

impl Store {
    /// Open (creating if absent) the store at `dir` with encryption-at-rest
    /// disabled (the [`PlainCodec`]). Runs full crash recovery.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_codec(dir, Box::new(PlainCodec))
    }

    /// Open the store with a specific [`PageCodec`] — used by `quiver-crypto` to
    /// enable encryption-at-rest. Runs full crash recovery.
    pub fn open_with_codec(dir: &Path, codec: Box<dyn PageCodec>) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        let wal_dir = dir.join("wal");
        fs::create_dir_all(&wal_dir).map_err(|e| CoreError::io(&wal_dir, e))?;
        fsync_dir(dir)?;
        fsync_dir(&wal_dir)?;

        // 1. Load the manifest (or start empty).
        let mfst = manifest::read_current(dir, codec.as_ref())?.unwrap_or_default();

        // 2. Rebuild live rows from the sealed segments the manifest references.
        let mut collections: HashMap<CollectionId, CollectionState> = HashMap::new();
        let mut name_index: HashMap<String, CollectionId> = HashMap::new();
        for entry in &mfst.collections {
            let descriptor: Descriptor = postcard::from_bytes(&entry.descriptor)?;
            let mut state = CollectionState::new(entry.id, entry.name.clone(), descriptor);
            state.segments = entry.segments.clone();
            for seg in &entry.segments {
                let path = segment_path(dir, entry.id, seg.id);
                let data = segment::read_segment(&path, codec.as_ref())?;
                for row in data.rows {
                    state.live.insert(
                        row.external_id,
                        Row {
                            vector: row.vector,
                            payload: row.payload,
                        },
                    );
                }
                for id in data.tombstones {
                    state.live.remove(&id);
                }
            }
            name_index.insert(state.name.clone(), state.id);
            collections.insert(state.id, state);
        }

        // 3. Replay the WAL tail (records past the checkpoint), idempotently.
        let floor = mfst.last_checkpointed_lsn;
        let mut max_lsn = floor;
        let wal_files = list_wal_files(&wal_dir)?;
        let mut max_seq = 0u64;
        let mut keep_seqs: HashSet<u64> = HashSet::new();
        for (seq, path) in &wal_files {
            max_seq = (*seq).max(max_seq);
            let replay = wal::read_all(path)?;
            let mut had_live = false;
            for entry in replay.entries {
                if entry.lsn.value() <= floor.value() {
                    continue; // already captured in a segment
                }
                had_live = true;
                if entry.lsn > max_lsn {
                    max_lsn = entry.lsn;
                }
                apply_wal_entry(&mut collections, &mut name_index, &entry)?;
            }
            if had_live {
                keep_seqs.insert(*seq);
            }
        }
        let next_lsn = max_lsn.next();

        // 4. GC orphan segment files not referenced by the manifest (a crash
        //    between a segment flush and the manifest swap).
        gc_orphan_segments(dir, &mfst)?;

        // 5. Start a fresh WAL segment for new appends, then drop superseded WAL
        //    files (empty or fully below the checkpoint).
        let wal_seq = max_seq + 1;
        let wal = WalWriter::create(&wal_file_path(&wal_dir, wal_seq), next_lsn)?;
        fsync_dir(&wal_dir)?;
        for (seq, path) in &wal_files {
            if !keep_seqs.contains(seq) {
                remove_file_if_present(path)?;
            }
        }
        fsync_dir(&wal_dir)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            codec,
            collections,
            name_index,
            next_lsn,
            next_collection_id: mfst.next_collection_id,
            next_segment_id: mfst.next_segment_id,
            manifest_version: mfst.version,
            last_checkpointed_lsn: floor,
            wal,
            wal_seq,
        })
    }

    /// Create a collection. Fails if the name is already taken.
    pub fn create_collection(
        &mut self,
        name: &str,
        descriptor: Descriptor,
    ) -> Result<CollectionId> {
        if self.name_index.contains_key(name) {
            return Err(CoreError::AlreadyExists(format!("collection {name}")));
        }
        if descriptor.dim == 0 {
            return Err(CoreError::InvalidArgument(
                "dim must be non-zero".to_owned(),
            ));
        }
        let id = CollectionId(self.next_collection_id);
        let descriptor_bytes = postcard::to_allocvec(&descriptor)?;
        let lsn = self.next_lsn;
        self.wal.append_sync(&WalEntry {
            lsn,
            op: WalOp::CreateCollection {
                collection_id: id,
                name: name.to_owned(),
                descriptor: descriptor_bytes,
            },
        })?;
        self.next_lsn = lsn.next();
        self.next_collection_id += 1;
        self.collections
            .insert(id, CollectionState::new(id, name.to_owned(), descriptor));
        self.name_index.insert(name.to_owned(), id);
        Ok(id)
    }

    /// Drop a collection and all of its data. Its segment files are reclaimed at
    /// the next checkpoint or the next open. Returns whether it existed.
    pub fn drop_collection(&mut self, name: &str) -> Result<bool> {
        let Some(&id) = self.name_index.get(name) else {
            return Ok(false);
        };
        let lsn = self.next_lsn;
        self.wal.append_sync(&WalEntry {
            lsn,
            op: WalOp::DropCollection { collection_id: id },
        })?;
        self.next_lsn = lsn.next();
        self.collections.remove(&id);
        self.name_index.remove(name);
        Ok(true)
    }

    /// Insert or replace a point. The vector length must equal the collection's
    /// dimensionality; the payload is stored opaquely. Returns the assigned LSN
    /// once the write is durable.
    pub fn upsert(
        &mut self,
        collection: CollectionId,
        external_id: &str,
        vector: &[f32],
        payload: &[u8],
    ) -> Result<Lsn> {
        let dim = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .descriptor
            .dim as usize;
        if vector.len() != dim {
            return Err(CoreError::InvalidArgument(format!(
                "vector has {} dims, collection expects {dim}",
                vector.len()
            )));
        }
        let vector_bytes = f32_to_le_bytes(vector);
        let lsn = self.next_lsn;
        self.wal.append_sync(&WalEntry {
            lsn,
            op: WalOp::Upsert {
                collection_id: collection,
                external_id: external_id.to_owned(),
                vector: vector_bytes.clone(),
                payload: payload.to_vec(),
            },
        })?;
        self.next_lsn = lsn.next();
        let state = self
            .collections
            .get_mut(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        state.live.insert(
            external_id.to_owned(),
            Row {
                vector: vector_bytes,
                payload: payload.to_vec(),
            },
        );
        state.dirty.insert(external_id.to_owned());
        state.deleted.remove(external_id);
        Ok(lsn)
    }

    /// Delete a point by external id. Returns whether it existed.
    pub fn delete(&mut self, collection: CollectionId, external_id: &str) -> Result<bool> {
        let existed = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .live
            .contains_key(external_id);
        if !existed {
            return Ok(false);
        }
        let lsn = self.next_lsn;
        self.wal.append_sync(&WalEntry {
            lsn,
            op: WalOp::Delete {
                collection_id: collection,
                external_id: external_id.to_owned(),
            },
        })?;
        self.next_lsn = lsn.next();
        let state = self
            .collections
            .get_mut(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        state.live.remove(external_id);
        state.dirty.remove(external_id);
        state.deleted.insert(external_id.to_owned());
        Ok(true)
    }

    /// Fetch a point by external id.
    pub fn get(&self, collection: CollectionId, external_id: &str) -> Result<Option<Record>> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        Ok(state.live.get(external_id).map(|row| Record {
            vector: le_bytes_to_f32(&row.vector),
            payload: row.payload.clone(),
        }))
    }

    /// Iterate every live `(external_id, record)` in a collection, in id order.
    /// Used to build the in-memory index and for brute-force scans.
    pub fn scan(&self, collection: CollectionId) -> Result<Vec<(String, Record)>> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        Ok(state
            .live
            .iter()
            .map(|(id, row)| {
                (
                    id.clone(),
                    Record {
                        vector: le_bytes_to_f32(&row.vector),
                        payload: row.payload.clone(),
                    },
                )
            })
            .collect())
    }

    /// The id of a collection by name, if it exists.
    #[must_use]
    pub fn collection_id(&self, name: &str) -> Option<CollectionId> {
        self.name_index.get(name).copied()
    }

    /// The descriptor of a collection, if it exists.
    #[must_use]
    pub fn descriptor(&self, collection: CollectionId) -> Option<&Descriptor> {
        self.collections.get(&collection).map(|s| &s.descriptor)
    }

    /// The number of live rows in a collection.
    pub fn len(&self, collection: CollectionId) -> Result<usize> {
        Ok(self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .live
            .len())
    }

    /// Whether a collection has no live rows.
    pub fn is_empty(&self, collection: CollectionId) -> Result<bool> {
        Ok(self.len(collection)? == 0)
    }

    /// Names of all collections, sorted.
    #[must_use]
    pub fn collection_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.name_index.keys().cloned().collect();
        names.sort();
        names
    }

    /// Seal everything changed since the last checkpoint into new immutable
    /// segments, install a new manifest atomically, rotate the WAL, and reclaim
    /// superseded files. A no-op if nothing has changed since the last
    /// checkpoint. Crash-safe at every step (see the module docs).
    pub fn checkpoint(&mut self) -> Result<()> {
        let last_lsn = Lsn(self.next_lsn.value().saturating_sub(1));
        if last_lsn.value() <= self.last_checkpointed_lsn.value() {
            return Ok(()); // nothing new since the last checkpoint
        }
        let mut cids: Vec<CollectionId> = self.collections.keys().copied().collect();
        cids.sort();

        // Phase A: write new segment files for collections with pending changes.
        let segment_lsn_low = self.last_checkpointed_lsn.next();
        let mut new_segments: HashMap<CollectionId, SegmentRef> = HashMap::new();
        for &cid in &cids {
            if !self.collections[&cid].has_pending() {
                continue;
            }
            let seg_id = self.next_segment_id;
            self.next_segment_id += 1;

            let (rows, tombstones): (Vec<SegmentRow>, Vec<String>) = {
                let state = &self.collections[&cid];
                let rows = state
                    .dirty
                    .iter()
                    .filter_map(|id| {
                        state.live.get(id).map(|row| SegmentRow {
                            external_id: id.clone(),
                            vector: row.vector.clone(),
                            payload: row.payload.clone(),
                        })
                    })
                    .collect();
                let tombstones = state.deleted.iter().cloned().collect();
                (rows, tombstones)
            };
            let row_count = rows.len() as u64;
            let data = SegmentData {
                format_version: SEGMENT_FORMAT_VERSION,
                segment_id: seg_id,
                rows,
                tombstones,
            };
            let seg_dir = segments_dir(&self.dir, cid);
            fs::create_dir_all(&seg_dir).map_err(|e| CoreError::io(&seg_dir, e))?;
            let path = seg_dir.join(segment_file_name(seg_id));
            segment::write_segment(&path, self.codec.as_ref(), &data)?;
            // Make the new segment file and its parent directories durable
            // before the manifest references it.
            fsync_dir(&seg_dir)?;
            fsync_dir(&collection_dir(&self.dir, cid))?;
            fsync_dir(&self.dir.join("collections"))?;
            fsync_dir(&self.dir)?;
            new_segments.insert(
                cid,
                SegmentRef {
                    id: seg_id,
                    row_count,
                    lsn_low: segment_lsn_low,
                    lsn_high: last_lsn,
                },
            );
        }

        // Phase B: build and atomically install the new manifest.
        let new_version = self.manifest_version + 1;
        let mut entries = Vec::with_capacity(cids.len());
        for &cid in &cids {
            let state = &self.collections[&cid];
            let mut segs = state.segments.clone();
            if let Some(seg) = new_segments.get(&cid) {
                segs.push(seg.clone());
            }
            entries.push(CollectionEntry {
                id: state.id,
                name: state.name.clone(),
                descriptor: postcard::to_allocvec(&state.descriptor)?,
                segments: segs,
            });
        }
        let new_manifest = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            version: new_version,
            last_checkpointed_lsn: last_lsn,
            next_collection_id: self.next_collection_id,
            next_segment_id: self.next_segment_id,
            collections: entries,
        };
        manifest::write_manifest(&self.dir, &new_manifest, self.codec.as_ref())?;

        // Phase C: commit in-memory state, rotate the WAL, GC superseded files.
        self.manifest_version = new_version;
        self.last_checkpointed_lsn = last_lsn;
        for &cid in &cids {
            if let Some(seg) = new_segments.remove(&cid) {
                if let Some(state) = self.collections.get_mut(&cid) {
                    state.segments.push(seg);
                    state.dirty.clear();
                    state.deleted.clear();
                }
            }
        }
        self.rotate_wal()?;
        gc_orphan_segments(&self.dir, &new_manifest)?;
        Ok(())
    }

    // Start a new WAL segment and delete every older one (all of their records
    // are now <= last_checkpointed_lsn and captured in segments).
    fn rotate_wal(&mut self) -> Result<()> {
        let wal_dir = self.dir.join("wal");
        let old_seq = self.wal_seq;
        let new_seq = old_seq + 1;
        let new_wal = WalWriter::create(&wal_file_path(&wal_dir, new_seq), self.next_lsn)?;
        fsync_dir(&wal_dir)?;
        self.wal = new_wal;
        self.wal_seq = new_seq;
        for (seq, path) in list_wal_files(&wal_dir)? {
            if seq <= old_seq {
                remove_file_if_present(&path)?;
            }
        }
        fsync_dir(&wal_dir)?;
        Ok(())
    }
}

// Apply a recovered WAL record to the in-memory state during open. Upserts and
// deletes are marked dirty/deleted so the next checkpoint re-seals them.
fn apply_wal_entry(
    collections: &mut HashMap<CollectionId, CollectionState>,
    name_index: &mut HashMap<String, CollectionId>,
    entry: &WalEntry,
) -> Result<()> {
    match &entry.op {
        WalOp::CreateCollection {
            collection_id,
            name,
            descriptor,
        } => {
            let descriptor: Descriptor = postcard::from_bytes(descriptor)?;
            name_index.insert(name.clone(), *collection_id);
            collections.insert(
                *collection_id,
                CollectionState::new(*collection_id, name.clone(), descriptor),
            );
        }
        WalOp::DropCollection { collection_id } => {
            if let Some(state) = collections.remove(collection_id) {
                name_index.remove(&state.name);
            }
        }
        WalOp::Upsert {
            collection_id,
            external_id,
            vector,
            payload,
        } => {
            if let Some(state) = collections.get_mut(collection_id) {
                state.live.insert(
                    external_id.clone(),
                    Row {
                        vector: vector.clone(),
                        payload: payload.clone(),
                    },
                );
                state.dirty.insert(external_id.clone());
                state.deleted.remove(external_id);
            }
        }
        WalOp::Delete {
            collection_id,
            external_id,
        } => {
            if let Some(state) = collections.get_mut(collection_id) {
                state.live.remove(external_id);
                state.dirty.remove(external_id);
                state.deleted.insert(external_id.clone());
            }
        }
        // The manifest is the authoritative checkpoint record; explicit
        // Checkpoint WAL records are not emitted in Phase 1 and are a no-op here.
        WalOp::Checkpoint { .. } => {}
    }
    Ok(())
}

// Delete superseded segment files (and whole dropped-collection directories)
// that the manifest no longer references.
fn gc_orphan_segments(dir: &Path, mfst: &Manifest) -> Result<()> {
    let collections_dir = dir.join("collections");
    if !collections_dir.exists() {
        return Ok(());
    }
    let mut referenced: HashSet<(u64, u64)> = HashSet::new();
    let mut live_collections: HashSet<u64> = HashSet::new();
    for c in &mfst.collections {
        live_collections.insert(c.id.value());
        for s in &c.segments {
            referenced.insert((c.id.value(), s.id));
        }
    }
    for entry in fs::read_dir(&collections_dir).map_err(|e| CoreError::io(&collections_dir, e))? {
        let entry = entry.map_err(|e| CoreError::io(&collections_dir, e))?;
        let cdir = entry.path();
        let Some(cid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if !live_collections.contains(&cid) {
            // A dropped collection: reclaim its whole directory.
            if cdir.is_dir() {
                fs::remove_dir_all(&cdir).map_err(|e| CoreError::io(&cdir, e))?;
            }
            continue;
        }
        let seg_dir = cdir.join("segments");
        if !seg_dir.is_dir() {
            continue;
        }
        for seg in fs::read_dir(&seg_dir).map_err(|e| CoreError::io(&seg_dir, e))? {
            let seg = seg.map_err(|e| CoreError::io(&seg_dir, e))?;
            let path = seg.path();
            let Some(seg_id) = seg.file_name().to_str().and_then(parse_segment_file_name) else {
                continue;
            };
            if !referenced.contains(&(cid, seg_id)) {
                remove_file_if_present(&path)?;
            }
        }
    }
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CoreError::io(path, e)),
    }
}

fn collection_dir(dir: &Path, cid: CollectionId) -> PathBuf {
    dir.join("collections").join(format!("{:010}", cid.value()))
}

fn segments_dir(dir: &Path, cid: CollectionId) -> PathBuf {
    collection_dir(dir, cid).join("segments")
}

fn segment_file_name(seg_id: u64) -> String {
    format!("seg-{seg_id:010}.seg")
}

fn parse_segment_file_name(name: &str) -> Option<u64> {
    name.strip_prefix("seg-")
        .and_then(|s| s.strip_suffix(".seg"))
        .and_then(|s| s.parse::<u64>().ok())
}

fn segment_path(dir: &Path, cid: CollectionId, seg_id: u64) -> PathBuf {
    segments_dir(dir, cid).join(segment_file_name(seg_id))
}

fn wal_file_path(wal_dir: &Path, seq: u64) -> PathBuf {
    wal_dir.join(format!("wal-{seq:010}.log"))
}

fn list_wal_files(wal_dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(wal_dir).map_err(|e| CoreError::io(wal_dir, e))? {
        let entry = entry.map_err(|e| CoreError::io(wal_dir, e))?;
        if let Some(seq) = entry.file_name().to_str().and_then(parse_wal_file_name) {
            out.push((seq, entry.path()));
        }
    }
    out.sort_by_key(|(seq, _)| *seq);
    Ok(out)
}

fn parse_wal_file_name(name: &str) -> Option<u64> {
    name.strip_prefix("wal-")
        .and_then(|s| s.strip_suffix(".log"))
        .and_then(|s| s.parse::<u64>().ok())
}

fn f32_to_le_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn le_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DistanceMetric, Dtype};

    fn desc() -> Descriptor {
        Descriptor {
            dim: 4,
            dtype: Dtype::F32,
            metric: DistanceMetric::L2,
        }
    }

    fn open(dir: &Path) -> Store {
        Store::open(dir).unwrap()
    }

    #[test]
    fn upsert_get_delete_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
        let got = s.get(c, "a").unwrap().unwrap();
        assert_eq!(got.vector, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(got.payload, b"{}");
        assert!(s.delete(c, "a").unwrap());
        assert!(s.get(c, "a").unwrap().is_none());
        assert!(!s.delete(c, "a").unwrap());
    }

    #[test]
    fn dim_mismatch_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        assert!(matches!(
            s.upsert(c, "a", &[1.0, 2.0], b"{}"),
            Err(CoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn duplicate_collection_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        s.create_collection("c", desc()).unwrap();
        assert!(matches!(
            s.create_collection("c", desc()),
            Err(CoreError::AlreadyExists(_))
        ));
    }

    #[test]
    fn recovers_without_checkpoint_via_wal_replay() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            for i in 0..10u32 {
                let v = [i as f32; 4];
                s.upsert(c, &format!("k{i}"), &v, b"{}").unwrap();
            }
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 10);
        let got = s.get(c, "k7").unwrap().unwrap();
        assert_eq!(got.vector, vec![7.0; 4]);
    }

    #[test]
    fn recovers_across_checkpoint_and_wal_tail() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            for i in 0..5u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap();
            // Post-checkpoint writes live only in the WAL until recovery.
            for i in 5..8u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.delete(c, "k0").unwrap();
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 7); // k1..k7
        assert!(s.get(c, "k0").unwrap().is_none());
        assert_eq!(s.get(c, "k6").unwrap().unwrap().vector, vec![6.0; 4]);
    }

    #[test]
    fn delete_survives_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0; 4], b"{}").unwrap();
            s.upsert(c, "b", &[2.0; 4], b"{}").unwrap();
            s.checkpoint().unwrap();
            s.delete(c, "a").unwrap();
            s.checkpoint().unwrap(); // tombstone sealed into a new segment
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert!(s.get(c, "a").unwrap().is_none());
        assert!(s.get(c, "b").unwrap().is_some());
        assert_eq!(s.len(c).unwrap(), 1);
    }

    #[test]
    fn reopen_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0; 4], b"{}").unwrap();
            s.checkpoint().unwrap();
            s.upsert(c, "b", &[2.0; 4], b"{}").unwrap();
        }
        let snapshot = |dir: &Path| {
            let s = open(dir);
            let c = s.collection_id("c").unwrap();
            s.scan(c).unwrap()
        };
        assert_eq!(snapshot(tmp.path()), snapshot(tmp.path()));
    }

    #[test]
    fn update_then_checkpoint_keeps_latest_value() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0; 4], b"v1").unwrap();
            s.checkpoint().unwrap();
            s.upsert(c, "a", &[9.0; 4], b"v2").unwrap(); // shadow the sealed row
            s.checkpoint().unwrap();
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        let got = s.get(c, "a").unwrap().unwrap();
        assert_eq!(got.vector, vec![9.0; 4]);
        assert_eq!(got.payload, b"v2");
        assert_eq!(s.len(c).unwrap(), 1);
    }

    #[test]
    fn dropped_collection_is_gone_after_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0; 4], b"{}").unwrap();
            s.checkpoint().unwrap();
            assert!(s.drop_collection("c").unwrap());
            s.checkpoint().unwrap();
        }
        let s = open(tmp.path());
        assert!(s.collection_id("c").is_none());
        assert!(s.collection_names().is_empty());
    }

    #[test]
    fn orphan_segment_is_garbage_collected() {
        let tmp = tempfile::tempdir().unwrap();
        let cid;
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            cid = c;
            s.upsert(c, "a", &[1.0; 4], b"{}").unwrap();
            s.checkpoint().unwrap();
        }
        // Drop a stray segment file the manifest does not reference.
        let stray = segment_path(tmp.path(), cid, 9999);
        fs::write(&stray, b"junk").unwrap();
        assert!(stray.exists());
        let _s = open(tmp.path());
        assert!(!stray.exists(), "orphan segment should be GC'd on open");
    }

    #[test]
    fn corrupt_segment_is_detected_not_served() {
        let tmp = tempfile::tempdir().unwrap();
        let cid;
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            cid = c;
            s.upsert(c, "a", &[1.0; 4], b"{}").unwrap();
            s.checkpoint().unwrap();
        }
        // Corrupt the sealed segment file (segment id 0).
        let path = segment_path(tmp.path(), cid, 0);
        let mut bytes = fs::read(&path).unwrap();
        bytes[64] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Store::open(tmp.path()),
            Err(CoreError::PageCorrupt { .. })
        ));
    }

    #[test]
    fn torn_wal_tail_drops_only_unacked_record() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_path;
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            for i in 0..3u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            wal_path = wal_file_path(&tmp.path().join("wal"), s.wal_seq);
        }
        // Append a torn (partial) frame to the tail of the active WAL.
        {
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().append(true).open(&wal_path).unwrap();
            f.write_all(&[0xAA, 0xBB, 0xCC]).unwrap();
            f.sync_data().unwrap();
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 3); // the 3 acked upserts recovered intact
    }
}
