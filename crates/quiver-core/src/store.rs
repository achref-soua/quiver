// SPDX-License-Identifier: AGPL-3.0-only
//! The storage engine: a durable, crash-safe vector store per collection.
//!
//! A [`Store`] ties the [`crate::wal`] and [`crate::manifest`] primitives,
//! together with immutable `segment`s in the row-addressed on-disk
//! format (ADR-0004), into a recoverable engine. The durability contract
//! (ADR-0005): a mutation is acknowledged only after its WAL record is `fsync`'d,
//! so an acknowledged write survives `kill -9`.
//!
//! ## Memory model
//! Vectors and payloads live on disk in sealed segments and are read through an
//! `mmap`, decrypted on demand — only the working set is resident. The engine
//! keeps in RAM a **primary index** (external id → row location) per collection,
//! plus the **active buffer**: the rows upserted since the last checkpoint, which
//! are also durable in the WAL. A read resolves the id to either an active row or
//! a `(segment, row)` and fetches the bytes from the active buffer or the segment.
//!
//! ## Write path
//! `upsert`/`delete`/`create_collection`/`drop_collection` append a WAL record,
//! `fsync` it (acknowledgement), then update in-memory state. `checkpoint` seals
//! the active buffer (and the window's deletes, as tombstones) into a new
//! immutable segment per collection, atomically swaps in a manifest that
//! references them, rotates the WAL, and garbage-collects superseded files.
//!
//! ## Recovery (on open)
//! Read `CURRENT` → load the manifest → for each referenced segment, read its
//! row directory and rebuild the primary index (a later segment shadows an
//! earlier one for the same id; a segment's tombstones remove ids) → replay every
//! WAL record with `lsn > last_checkpointed_lsn` idempotently into the active
//! buffer → garbage-collect orphan segment files a crash left between a flush and
//! the manifest swap. A torn trailing WAL record fails its frame check and is
//! dropped; it was never acknowledged.
//!
//! ## Concurrency
//! Phase 1/2 is a single-writer engine: mutations take `&mut self`, reads take
//! `&self`. The lock-free MVCC snapshot model (ADR-0006) arrives with the
//! server integration; until then a server wraps the store in a lock.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::descriptor::Descriptor;
use crate::error::{CoreError, Result};
use crate::ids::{CollectionId, Lsn};
use crate::manifest::{self, CollectionEntry, MANIFEST_FORMAT_VERSION, Manifest, SegmentRef};
use crate::page::{PageCodec, PlainCodec};
use crate::paged::fsync_dir;
use crate::segment::{self, SealRow, SealedSegment};
use crate::wal::{self, WalEntry, WalOp, WalWriter};

/// A stored record returned by reads: the decoded vector and opaque payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The vector, decoded from its on-disk little-endian bytes.
    pub vector: Vec<f32>,
    /// The opaque payload bytes (validated UTF-8 JSON at the API edge).
    pub payload: Vec<u8>,
}

// Where a live row's bytes are: in the in-RAM active buffer, or in a sealed
// segment at `(segment index, row)`.
#[derive(Debug, Clone, Copy)]
enum Loc {
    Active(u32),
    Sealed { seg: u32, row: u32 },
}

// A row buffered in RAM since the last checkpoint. Also durable in the WAL until
// the checkpoint seals it to disk.
#[derive(Debug, Clone)]
struct ActiveRow {
    vector: Vec<u8>,
    payload: Vec<u8>,
}

// In-memory state of one collection.
struct CollectionState {
    id: CollectionId,
    name: String,
    descriptor: Descriptor,
    // Bytes per vector (`dim × dtype size`), cached from the descriptor.
    stride: usize,
    // Live external id → location. The authority for `get`/`len`/`scan`; ordered
    // so `scan` yields ids deterministically.
    primary: BTreeMap<String, Loc>,
    // Sealed segments in creation order; `Loc::Sealed.seg` indexes this.
    sealed: Vec<SealedSegment>,
    // Manifest segment refs, parallel to `sealed`.
    segments_meta: Vec<SegmentRef>,
    // Rows upserted since the last checkpoint; index = `Loc::Active` row.
    active: Vec<ActiveRow>,
    // Live external id → its latest active row, for sealing at the next checkpoint.
    active_index: BTreeMap<String, u32>,
    // Ids deleted since the last checkpoint, sealed as tombstones next checkpoint.
    deleted: BTreeSet<String>,
}

impl CollectionState {
    fn new(id: CollectionId, name: String, descriptor: Descriptor) -> Self {
        let stride = descriptor.stride();
        Self {
            id,
            name,
            descriptor,
            stride,
            primary: BTreeMap::new(),
            sealed: Vec::new(),
            segments_meta: Vec::new(),
            active: Vec::new(),
            active_index: BTreeMap::new(),
            deleted: BTreeSet::new(),
        }
    }

    fn has_pending(&self) -> bool {
        !self.active_index.is_empty() || !self.deleted.is_empty()
    }
}

// A segment written during a checkpoint, opened and ready to install after the
// manifest swap commits.
struct PendingSegment {
    seg_ref: SegmentRef,
    sealed: SealedSegment,
    // External ids in row order, used to repoint the primary index.
    ext_ids: Vec<String>,
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

        // 2. Rebuild the primary index from the sealed segments the manifest
        //    references. Segments are applied oldest-to-newest: a later row
        //    shadows an earlier one for the same id, and tombstones remove ids.
        let mut collections: HashMap<CollectionId, CollectionState> = HashMap::new();
        let mut name_index: HashMap<String, CollectionId> = HashMap::new();
        for entry in &mfst.collections {
            let descriptor = Descriptor::decode(&entry.descriptor)?;
            let mut state = CollectionState::new(entry.id, entry.name.clone(), descriptor);
            state.segments_meta = entry.segments.clone();
            let seg_dir = segments_dir(dir, entry.id);
            for seg in &entry.segments {
                let (sealed, ext_ids, tombstones) =
                    segment::open_segment(&seg_dir, seg.id, codec.as_ref())?;
                let seg_idx = state.sealed.len() as u32;
                for (row, ext_id) in ext_ids.into_iter().enumerate() {
                    state.primary.insert(
                        ext_id,
                        Loc::Sealed {
                            seg: seg_idx,
                            row: row as u32,
                        },
                    );
                }
                for t in &tombstones {
                    state.primary.remove(t);
                }
                state.sealed.push(sealed);
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
            let replay = wal::read_all(path, codec.as_ref())?;
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
        self.wal.append_sync(
            self.codec.as_ref(),
            &WalEntry {
                lsn,
                op: WalOp::CreateCollection {
                    collection_id: id,
                    name: name.to_owned(),
                    descriptor: descriptor_bytes,
                },
            },
        )?;
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
        self.wal.append_sync(
            self.codec.as_ref(),
            &WalEntry {
                lsn,
                op: WalOp::DropCollection { collection_id: id },
            },
        )?;
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
        self.wal.append_sync(
            self.codec.as_ref(),
            &WalEntry {
                lsn,
                op: WalOp::Upsert {
                    collection_id: collection,
                    external_id: external_id.to_owned(),
                    vector: vector_bytes.clone(),
                    payload: payload.to_vec(),
                },
            },
        )?;
        self.next_lsn = lsn.next();
        let state = self
            .collections
            .get_mut(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        let row = state.active.len() as u32;
        state.active.push(ActiveRow {
            vector: vector_bytes,
            payload: payload.to_vec(),
        });
        state.active_index.insert(external_id.to_owned(), row);
        state
            .primary
            .insert(external_id.to_owned(), Loc::Active(row));
        state.deleted.remove(external_id);
        Ok(lsn)
    }

    /// Delete a point by external id. Returns whether it existed.
    pub fn delete(&mut self, collection: CollectionId, external_id: &str) -> Result<bool> {
        let existed = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .primary
            .contains_key(external_id);
        if !existed {
            return Ok(false);
        }
        let lsn = self.next_lsn;
        self.wal.append_sync(
            self.codec.as_ref(),
            &WalEntry {
                lsn,
                op: WalOp::Delete {
                    collection_id: collection,
                    external_id: external_id.to_owned(),
                },
            },
        )?;
        self.next_lsn = lsn.next();
        let state = self
            .collections
            .get_mut(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        state.primary.remove(external_id);
        state.active_index.remove(external_id);
        state.deleted.insert(external_id.to_owned());
        Ok(true)
    }

    /// Fetch a point by external id.
    pub fn get(&self, collection: CollectionId, external_id: &str) -> Result<Option<Record>> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        match state.primary.get(external_id).copied() {
            Some(loc) => Ok(Some(self.record_at(state, loc)?)),
            None => Ok(None),
        }
    }

    /// Iterate every live `(external_id, record)` in a collection, in id order.
    /// Used to build the in-memory index and for brute-force scans.
    pub fn scan(&self, collection: CollectionId) -> Result<Vec<(String, Record)>> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        let mut out = Vec::with_capacity(state.primary.len());
        for (id, &loc) in &state.primary {
            out.push((id.clone(), self.record_at(state, loc)?));
        }
        Ok(out)
    }

    // Materialize the record at `loc`, reading from the active buffer or the
    // sealed segment (decrypting and CRC-checking the touched pages).
    fn record_at(&self, state: &CollectionState, loc: Loc) -> Result<Record> {
        match loc {
            Loc::Active(r) => {
                let row = state
                    .active
                    .get(r as usize)
                    .ok_or_else(|| CoreError::MalformedPage(format!("dangling active row {r}")))?;
                Ok(Record {
                    vector: le_bytes_to_f32(&row.vector),
                    payload: row.payload.clone(),
                })
            }
            Loc::Sealed { seg, row } => {
                let segment = state.sealed.get(seg as usize).ok_or_else(|| {
                    CoreError::MalformedPage(format!("dangling segment index {seg}"))
                })?;
                let vector_bytes = segment.read_vector(self.codec.as_ref(), row, state.stride)?;
                let payload = segment.read_payload(self.codec.as_ref(), row)?;
                Ok(Record {
                    vector: le_bytes_to_f32(&vector_bytes),
                    payload,
                })
            }
        }
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

    /// A borrow of the page codec, for sealing one-off bytes with the store's
    /// key (e.g. writing a disk-resident index artifact, ADR-0019).
    #[must_use]
    pub fn codec_ref(&self) -> &dyn PageCodec {
        self.codec.as_ref()
    }

    /// A clone of the page codec, for a component that needs to own its own
    /// handle (e.g. opening a disk-resident index that `mmap`s its files).
    #[must_use]
    pub fn codec_clone(&self) -> Box<dyn PageCodec> {
        self.codec.clone_box()
    }

    /// The directory that holds a collection's index artifacts
    /// (`<data_dir>/collections/<id>/index`). Not created by this call.
    #[must_use]
    pub fn index_dir(&self, collection: CollectionId) -> PathBuf {
        collection_dir(&self.dir, collection).join("index")
    }

    /// The number of live rows in a collection.
    pub fn len(&self, collection: CollectionId) -> Result<usize> {
        Ok(self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .primary
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
        let segment_lsn_low = self.last_checkpointed_lsn.next();

        // Phase A: write a new segment file set for each collection with pending
        // changes, then re-open it (mmap) ready to install after the swap.
        let mut pending: HashMap<CollectionId, PendingSegment> = HashMap::new();
        for &cid in &cids {
            if !self.collections[&cid].has_pending() {
                continue;
            }
            let seg_id = self.next_segment_id;
            self.next_segment_id += 1;
            let seg_dir = segments_dir(&self.dir, cid);
            fs::create_dir_all(&seg_dir).map_err(|e| CoreError::io(&seg_dir, e))?;

            // Seal the active rows (in deterministic id order) and the window's
            // deletes (as tombstones). The borrow of `self.collections` ends with
            // this block, before the commit phase mutates it.
            let row_count = {
                let state = &self.collections[&cid];
                let seal_rows: Vec<SealRow<'_>> = state
                    .active_index
                    .iter()
                    .map(|(id, &row)| SealRow {
                        external_id: id,
                        vector: &state.active[row as usize].vector,
                        payload: &state.active[row as usize].payload,
                    })
                    .collect();
                let tombstones: Vec<String> = state.deleted.iter().cloned().collect();
                segment::write_segment(
                    &seg_dir,
                    seg_id,
                    self.codec.as_ref(),
                    &seal_rows,
                    &tombstones,
                )?;
                seal_rows.len() as u64
            };

            // Make the new files and their parent directories durable before the
            // manifest references them.
            fsync_dir(&seg_dir)?;
            fsync_dir(&collection_dir(&self.dir, cid))?;
            fsync_dir(&self.dir.join("collections"))?;
            fsync_dir(&self.dir)?;

            let (sealed, ext_ids, _tombstones) =
                segment::open_segment(&seg_dir, seg_id, self.codec.as_ref())?;
            pending.insert(
                cid,
                PendingSegment {
                    seg_ref: SegmentRef {
                        id: seg_id,
                        row_count,
                        lsn_low: segment_lsn_low,
                        lsn_high: last_lsn,
                    },
                    sealed,
                    ext_ids,
                },
            );
        }

        // Phase B: build and atomically install the new manifest.
        let new_version = self.manifest_version + 1;
        let mut entries = Vec::with_capacity(cids.len());
        for &cid in &cids {
            let state = &self.collections[&cid];
            let mut segs = state.segments_meta.clone();
            if let Some(p) = pending.get(&cid) {
                segs.push(p.seg_ref.clone());
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
            if let Some(p) = pending.remove(&cid)
                && let Some(state) = self.collections.get_mut(&cid)
            {
                // Every active id now lives in the new segment; repoint it. Old
                // sealed rows for updated ids are left shadowed (compaction
                // reclaims them, PR 2).
                let seg_idx = state.sealed.len() as u32;
                for (row, ext_id) in p.ext_ids.into_iter().enumerate() {
                    state.primary.insert(
                        ext_id,
                        Loc::Sealed {
                            seg: seg_idx,
                            row: row as u32,
                        },
                    );
                }
                state.sealed.push(p.sealed);
                state.segments_meta.push(p.seg_ref);
                state.active.clear();
                state.active_index.clear();
                state.deleted.clear();
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

// Apply a recovered WAL record to the in-memory state during open. Upserts land
// in the active buffer (and are re-sealed at the next checkpoint); deletes remove
// from the primary index and are recorded for tombstoning.
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
            let descriptor = Descriptor::decode(descriptor)?;
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
                let row = state.active.len() as u32;
                state.active.push(ActiveRow {
                    vector: vector.clone(),
                    payload: payload.clone(),
                });
                state.active_index.insert(external_id.clone(), row);
                state.primary.insert(external_id.clone(), Loc::Active(row));
                state.deleted.remove(external_id);
            }
        }
        WalOp::Delete {
            collection_id,
            external_id,
        } => {
            if let Some(state) = collections.get_mut(collection_id) {
                state.primary.remove(external_id);
                state.active_index.remove(external_id);
                state.deleted.insert(external_id.clone());
            }
        }
        // The manifest is the authoritative checkpoint record; explicit
        // Checkpoint WAL records are not emitted and are a no-op here.
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
            let Some(seg_id) = seg.file_name().to_str().and_then(segment::seg_id_of_file) else {
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
        Descriptor::new(4, Dtype::F32, DistanceMetric::L2)
    }

    fn open(dir: &Path) -> Store {
        Store::open(dir).unwrap()
    }

    // Path to a segment's row-directory file, for corruption/orphan tests.
    fn seg_dir_file(dir: &Path, cid: CollectionId, seg_id: u64) -> PathBuf {
        segments_dir(dir, cid).join(format!("seg-{seg_id:010}.dir"))
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
    fn update_within_one_window_seals_latest() {
        // Re-upsert the same id several times before any checkpoint: only the
        // latest active row must be sealed and recoverable.
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0; 4], b"v1").unwrap();
            s.upsert(c, "a", &[2.0; 4], b"v2").unwrap();
            s.upsert(c, "a", &[3.0; 4], b"v3").unwrap();
            s.checkpoint().unwrap();
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 1);
        let got = s.get(c, "a").unwrap().unwrap();
        assert_eq!(got.vector, vec![3.0; 4]);
        assert_eq!(got.payload, b"v3");
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
        let stray = segments_dir(tmp.path(), cid).join("seg-0000009999.vec");
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
        // Corrupt the sealed segment's row directory (read and verified at open).
        // Flip a byte in page 0's live body (the 8-byte length prefix), which the
        // CRC covers — a small directory's postcard body does not reach far into
        // the 16 KiB page, so a deep offset would land in uncovered padding.
        let path = seg_dir_file(tmp.path(), cid, 0);
        let mut bytes = fs::read(&path).unwrap();
        bytes[33] ^= 0xFF;
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

    #[test]
    fn reads_served_from_disk_after_checkpoint() {
        // After a checkpoint the active buffer is cleared, so a get must come
        // from the sealed segment's mmap'd columns — exercising the disk path.
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], br#"{"k":1}"#)
            .unwrap();
        s.checkpoint().unwrap();
        let got = s.get(c, "a").unwrap().unwrap();
        assert_eq!(got.vector, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(got.payload, br#"{"k":1}"#);
    }

    #[test]
    fn high_dim_vectors_straddle_pages() {
        // A dimensionality whose stride does not divide the page body, forcing
        // vectors to straddle 16 KiB block boundaries in the .vec column.
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let dim = 1000usize; // stride = 4000 B; ~4 vectors per 16352-B page body
        let c = s
            .create_collection(
                "c",
                Descriptor::new(dim as u32, Dtype::F32, DistanceMetric::L2),
            )
            .unwrap();
        for i in 0..20u32 {
            let v: Vec<f32> = (0..dim).map(|j| (i as f32) * 1000.0 + j as f32).collect();
            s.upsert(c, &format!("k{i}"), &v, b"{}").unwrap();
        }
        s.checkpoint().unwrap();
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        for i in 0..20u32 {
            let got = s.get(c, &format!("k{i}")).unwrap().unwrap();
            let want: Vec<f32> = (0..dim).map(|j| (i as f32) * 1000.0 + j as f32).collect();
            assert_eq!(
                got.vector, want,
                "vector k{i} mismatch after straddling read"
            );
        }
    }
}
