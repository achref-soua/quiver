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
//! the active buffer into a new immutable segment per collection, persists the
//! window's deletes and shadowed rows into the affected segments' `.del`
//! tombstone bitmaps, atomically swaps in a manifest, rotates the WAL, and
//! garbage-collects superseded files.
//!
//! ## Recovery (on open)
//! Read `CURRENT` → load the manifest → for each referenced segment, read its
//! row directory and tombstone bitmap and rebuild the primary index (a row marked
//! dead in its segment is skipped, so each id is live in exactly one segment) →
//! replay every WAL record with `lsn > last_checkpointed_lsn` idempotently into
//! the active buffer → garbage-collect orphan segment files a crash left between a
//! flush and the manifest swap. A torn trailing WAL record fails its frame check
//! and is dropped; it was never acknowledged.
//!
//! ## Concurrency
//! Phase 1/2 is a single-writer engine: mutations take `&mut self`, reads take
//! `&self`. The lock-free MVCC snapshot model (ADR-0006) arrives with the
//! server integration; until then a server wraps the store in a lock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use roaring::RoaringBitmap;

use crate::descriptor::Descriptor;
use crate::error::{CoreError, Result};
use crate::ids::{CollectionId, Lsn};
use crate::keyring::{KeyRing, SingleCodecKeyRing};
use crate::manifest::{
    self, CollectionEntry, IndexSnapshotRef, MANIFEST_FORMAT_VERSION, Manifest, SegmentRef,
};
use crate::page::{PageCodec, PageType};
use crate::paged::{fsync_dir, read_paged, write_paged};
use crate::sec::{self, SecPredicate};
use crate::segment::{self, SealRow, SealedSegment};
use crate::wal::{self, WalEntry, WalOp, WalWriter};

/// Number of sealed segments at which a checkpoint auto-compacts a collection,
/// merging them to keep reads and recovery from fanning out across many files.
const COMPACT_MIN_SEGMENTS: usize = 8;

/// Maximum length of a collection name, in bytes. A name is addressed as a single
/// URL path segment by the REST layer, so it is kept short and path-safe.
pub const MAX_COLLECTION_NAME_LEN: usize = 255;

/// Validate a collection name at creation: non-empty, at most
/// [`MAX_COLLECTION_NAME_LEN`] bytes, and every character an ASCII letter, digit,
/// `-`, `_`, or `.`. That charset makes a name always a safe single URL path
/// segment — no `/`, control characters, whitespace, or non-ASCII — since the REST
/// gateway addresses a collection as one path segment. Rejected names error with
/// [`CoreError::InvalidArgument`] and no collection is created.
fn validate_collection_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(CoreError::InvalidArgument(
            "collection name must not be empty".to_owned(),
        ));
    }
    if name.len() > MAX_COLLECTION_NAME_LEN {
        return Err(CoreError::InvalidArgument(format!(
            "collection name must be at most {MAX_COLLECTION_NAME_LEN} bytes, got {}",
            name.len()
        )));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(CoreError::InvalidArgument(format!(
            "collection name {name:?} contains an invalid character {bad:?}; \
             allowed: ASCII letters, digits, '-', '_', '.'"
        )));
    }
    Ok(())
}

/// A stored record returned by reads: the decoded vector and opaque payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The vector, decoded from its on-disk little-endian bytes.
    pub vector: Vec<f32>,
    /// The opaque payload bytes (validated UTF-8 JSON at the API edge).
    pub payload: Vec<u8>,
}

/// The post-checkpoint mutations a restored index snapshot must replay to reach
/// the current state — the WAL tail `open` already applied to the store
/// (ADR-0025). Both lists are bounded by the checkpoint cadence, not the
/// collection size.
#[derive(Debug, Default)]
pub struct RecoveryTail {
    /// Live rows upserted since the last checkpoint (the active buffer).
    pub upserts: Vec<(String, Record)>,
    /// External ids whose pre-checkpoint row died this window (deleted, or
    /// shadowed by a re-upsert) and so must be removed from a restored index.
    pub deleted: Vec<String>,
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
    // The codec that seals this collection's segments and index artifacts —
    // its own data-encryption key under an envelope key-ring, or the shared
    // codec under a single-codec key-ring. Built once from the key-ring at
    // create/open and held so reads need no per-call key derivation.
    codec: Box<dyn PageCodec>,
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
    // Sealed-segment rows that died this window (deleted or shadowed), keyed by
    // segment index; merged into each segment's `.del` at the next checkpoint.
    dead_this_window: BTreeMap<u32, RoaringBitmap>,
    // The durable index snapshot reference (ADR-0025): loaded from the manifest on
    // open, refreshed at each checkpoint; `None` if the index is rebuilt on open.
    index_snapshot: Option<IndexSnapshotRef>,
}

impl CollectionState {
    fn new(
        id: CollectionId,
        name: String,
        descriptor: Descriptor,
        codec: Box<dyn PageCodec>,
    ) -> Self {
        let stride = descriptor.stride();
        Self {
            id,
            name,
            descriptor,
            codec,
            stride,
            primary: BTreeMap::new(),
            sealed: Vec::new(),
            segments_meta: Vec::new(),
            active: Vec::new(),
            active_index: BTreeMap::new(),
            dead_this_window: BTreeMap::new(),
            index_snapshot: None,
        }
    }

    fn has_pending(&self) -> bool {
        !self.active_index.is_empty() || !self.dead_this_window.is_empty()
    }

    // Apply an upsert to in-memory state (shared by the write path and WAL
    // replay). If the id currently lives in a sealed segment, that row is now
    // shadowed and recorded for tombstoning at the next checkpoint.
    fn apply_upsert(&mut self, external_id: &str, vector: Vec<u8>, payload: Vec<u8>) {
        if let Some(Loc::Sealed { seg, row }) = self.primary.get(external_id).copied() {
            self.dead_this_window.entry(seg).or_default().insert(row);
        }
        let row = self.active.len() as u32;
        self.active.push(ActiveRow { vector, payload });
        self.active_index.insert(external_id.to_owned(), row);
        self.primary
            .insert(external_id.to_owned(), Loc::Active(row));
    }

    // Apply a delete to in-memory state (shared by the write path and WAL
    // replay). Returns whether the id existed. A deleted sealed row is recorded
    // for tombstoning; a deleted active row is simply dropped from the buffer.
    fn apply_delete(&mut self, external_id: &str) -> bool {
        match self.primary.remove(external_id) {
            Some(Loc::Sealed { seg, row }) => {
                self.dead_this_window.entry(seg).or_default().insert(row);
                self.active_index.remove(external_id);
                true
            }
            Some(Loc::Active(_)) => {
                self.active_index.remove(external_id);
                true
            }
            None => false,
        }
    }
}

// A segment written during a checkpoint, opened and ready to install after the
// manifest swap commits. The repointing ids come from `sealed.row_ids()`.
struct PendingSegment {
    seg_ref: SegmentRef,
    sealed: SealedSegment,
}

/// A synchronous hook invoked with each committed [`WalEntry`], in commit order.
/// Leader-follower replication (ADR-0030) installs one to publish each op to its
/// replication stream. A plain `Fn` keeps the engine runtime-agnostic — no async
/// dependency leaks into `quiver-core`.
pub type CommitObserver = Arc<dyn Fn(&WalEntry) + Send + Sync>;

/// The durable storage engine for one data directory.
pub struct Store {
    dir: PathBuf,
    keyring: Box<dyn KeyRing>,
    collections: HashMap<CollectionId, CollectionState>,
    name_index: HashMap<String, CollectionId>,
    next_lsn: Lsn,
    next_collection_id: u64,
    next_segment_id: u64,
    manifest_version: u64,
    last_checkpointed_lsn: Lsn,
    wal: WalWriter,
    wal_seq: u64,
    commit_observer: Option<CommitObserver>,
}

impl Store {
    /// Open (creating if absent) the store at `dir` with encryption-at-rest
    /// disabled (the plaintext codec). Runs full crash recovery.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_keyring(dir, Box::new(SingleCodecKeyRing::plaintext()))
    }

    /// Open the store sealing every byte — catalog and all collections — with a
    /// single [`PageCodec`]. Used by `quiver-crypto` to enable encryption-at-rest
    /// under one root key (no per-collection envelope). Runs full crash recovery.
    pub fn open_with_codec(dir: &Path, codec: Box<dyn PageCodec>) -> Result<Self> {
        Self::open_with_keyring(dir, Box::new(SingleCodecKeyRing::new(codec)))
    }

    /// Open the store with a [`KeyRing`] supplying a catalog codec (manifest and
    /// WAL) and a per-collection codec (segments and index artifacts). This is
    /// the seam `quiver-crypto`'s envelope key-ring uses to seal each collection
    /// under its own data-encryption key, enabling crypto-shredding. Runs full
    /// crash recovery.
    pub fn open_with_keyring(dir: &Path, keyring: Box<dyn KeyRing>) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        let wal_dir = dir.join("wal");
        fs::create_dir_all(&wal_dir).map_err(|e| CoreError::io(&wal_dir, e))?;
        fsync_dir(dir)?;
        fsync_dir(&wal_dir)?;

        // 1. Load the manifest (or start empty).
        let mfst = manifest::read_current(dir, keyring.catalog_codec())?.unwrap_or_default();

        // 2. Rebuild the primary index from the sealed segments the manifest
        //    references. A row tombstoned in its segment's `.del` is skipped, so
        //    each external id is added from the single segment in which it is live.
        let mut collections: HashMap<CollectionId, CollectionState> = HashMap::new();
        let mut name_index: HashMap<String, CollectionId> = HashMap::new();
        for entry in &mfst.collections {
            let descriptor = Descriptor::decode(&entry.descriptor)?;
            let codec = keyring.collection_codec(entry.id)?;
            let mut state = CollectionState::new(entry.id, entry.name.clone(), descriptor, codec);
            state.segments_meta = entry.segments.clone();
            state.index_snapshot = entry.index_snapshot.clone();
            let seg_dir = segments_dir(dir, entry.id);
            for seg in &entry.segments {
                let sealed = segment::open_segment(&seg_dir, seg.id, state.codec.as_ref())?;
                let seg_idx = state.sealed.len() as u32;
                for (row, ext_id) in sealed.row_ids().iter().enumerate() {
                    let row = row as u32;
                    if !sealed.is_dead(row) {
                        state
                            .primary
                            .insert(ext_id.clone(), Loc::Sealed { seg: seg_idx, row });
                    }
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
            let replay = wal::read_all(path, keyring.catalog_codec())?;
            let mut had_live = false;
            for entry in replay.entries {
                if entry.lsn.value() <= floor.value() {
                    continue; // already captured in a segment
                }
                had_live = true;
                if entry.lsn > max_lsn {
                    max_lsn = entry.lsn;
                }
                apply_wal_entry(&mut collections, &mut name_index, &entry, keyring.as_ref())?;
            }
            if had_live {
                keep_seqs.insert(*seq);
            }
        }
        let next_lsn = max_lsn.next();

        // 4. GC orphan segment files not referenced by the manifest (a crash
        //    between a segment flush and the manifest swap), then the analogous
        //    orphan/superseded index snapshots (ADR-0025).
        gc_orphan_segments(dir, &mfst, keyring.as_ref())?;
        gc_orphan_index_snapshots(dir, &mfst)?;

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
            keyring,
            collections,
            name_index,
            next_lsn,
            next_collection_id: mfst.next_collection_id,
            next_segment_id: mfst.next_segment_id,
            manifest_version: mfst.version,
            last_checkpointed_lsn: floor,
            wal,
            wal_seq,
            commit_observer: None,
        })
    }

    /// Install a hook invoked with each committed [`WalEntry`], in commit order
    /// (ADR-0030). Used by the server to drive a leader's replication stream;
    /// replaces any previous observer.
    pub fn set_commit_observer(&mut self, observer: CommitObserver) {
        self.commit_observer = Some(observer);
    }

    // Notify the commit observer (if any) of a durably-committed entry.
    fn publish(&self, entry: &WalEntry) {
        if let Some(observer) = &self.commit_observer {
            observer(entry);
        }
    }

    /// The operations that recreate the store's current logical state, for a
    /// replication follower to bootstrap from (ADR-0030): a `CreateCollection`
    /// per collection, each followed by an `Upsert` per live point. Collections
    /// are emitted before their points so a follower can apply the stream in
    /// order.
    pub fn replication_snapshot(&self) -> Result<Vec<WalOp>> {
        let mut ops = Vec::new();
        for (&id, state) in &self.collections {
            ops.push(WalOp::CreateCollection {
                collection_id: id,
                name: state.name.clone(),
                descriptor: postcard::to_allocvec(&state.descriptor)?,
            });
            for (external_id, record) in self.scan(id)? {
                ops.push(WalOp::Upsert {
                    collection_id: id,
                    external_id,
                    vector: f32_to_le_bytes(&record.vector),
                    payload: record.payload,
                });
            }
        }
        Ok(ops)
    }

    /// Apply a replicated operation received from a leader (ADR-0030). The op is
    /// persisted to *this* node's WAL under a locally-assigned LSN — preserving
    /// the leader's collection id so later ops resolve — then applied to in-memory
    /// state through the same path crash recovery uses. `Checkpoint` ops are a
    /// per-node concern and are ignored; followers checkpoint themselves.
    pub fn apply_replicated(&mut self, op: WalOp) -> Result<()> {
        if let WalOp::Checkpoint { .. } = op {
            return Ok(());
        }
        if let WalOp::CreateCollection { collection_id, .. } = &op {
            // Provision key material before the collection's codec is needed, and
            // keep the local id allocator ahead of the leader's ids.
            self.keyring.provision_collection(*collection_id)?;
            self.next_collection_id = self.next_collection_id.max(collection_id.0 + 1);
        }
        let lsn = self.next_lsn;
        let entry = WalEntry { lsn, op };
        self.wal.append_sync(self.keyring.catalog_codec(), &entry)?;
        self.next_lsn = lsn.next();
        apply_wal_entry(
            &mut self.collections,
            &mut self.name_index,
            &entry,
            self.keyring.as_ref(),
        )?;
        self.publish(&entry);
        Ok(())
    }

    /// Create a collection. Fails if the name is already taken.
    pub fn create_collection(
        &mut self,
        name: &str,
        descriptor: Descriptor,
    ) -> Result<CollectionId> {
        validate_collection_name(name)?;
        if self.name_index.contains_key(name) {
            return Err(CoreError::AlreadyExists(format!("collection {name}")));
        }
        if descriptor.dim == 0 {
            return Err(CoreError::InvalidArgument(
                "dim must be non-zero".to_owned(),
            ));
        }
        let id = CollectionId(self.next_collection_id);
        // Provision the collection's key material before its first durable record
        // references it, so WAL replay on recovery can always open what it needs.
        self.keyring.provision_collection(id)?;
        let descriptor_bytes = postcard::to_allocvec(&descriptor)?;
        let lsn = self.next_lsn;
        let entry = WalEntry {
            lsn,
            op: WalOp::CreateCollection {
                collection_id: id,
                name: name.to_owned(),
                descriptor: descriptor_bytes,
            },
        };
        self.wal.append_sync(self.keyring.catalog_codec(), &entry)?;
        self.next_lsn = lsn.next();
        self.publish(&entry);
        self.next_collection_id += 1;
        let codec = self.keyring.collection_codec(id)?;
        self.collections.insert(
            id,
            CollectionState::new(id, name.to_owned(), descriptor, codec),
        );
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
        let entry = WalEntry {
            lsn,
            op: WalOp::DropCollection { collection_id: id },
        };
        self.wal.append_sync(self.keyring.catalog_codec(), &entry)?;
        self.next_lsn = lsn.next();
        self.publish(&entry);
        self.collections.remove(&id);
        self.name_index.remove(name);
        Ok(true)
    }

    /// Crypto-shred a collection: drop it, checkpoint so the manifest no longer
    /// references it and its files are reclaimed, then destroy its key material.
    /// After this its sealed segments and index are unrecoverable even to the
    /// master-key holder (ADR-0010); with a single-codec key-ring there is no
    /// per-collection key, so this is `drop` plus a checkpoint. Returns whether
    /// the collection existed.
    pub fn shred_collection(&mut self, name: &str) -> Result<bool> {
        let Some(id) = self.collection_id(name) else {
            return Ok(false);
        };
        self.drop_collection(name)?;
        // Seal any un-checkpointed rows into DEK-protected segments and rotate
        // the WAL, so no live catalog-keyed copy of the collection survives; the
        // checkpoint's GC then reclaims its files and shreds its key.
        self.checkpoint()?;
        // Destroy the key explicitly too, covering a collection that never
        // reached a segment directory for GC to find. Idempotent.
        self.keyring.shred_collection(id)?;
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
        let entry = WalEntry {
            lsn,
            op: WalOp::Upsert {
                collection_id: collection,
                external_id: external_id.to_owned(),
                vector: vector_bytes.clone(),
                payload: payload.to_vec(),
            },
        };
        self.wal.append_sync(self.keyring.catalog_codec(), &entry)?;
        self.next_lsn = lsn.next();
        self.publish(&entry);
        let state = self
            .collections
            .get_mut(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        state.apply_upsert(external_id, vector_bytes, payload.to_vec());
        Ok(lsn)
    }

    /// Upsert a batch of points with a **single** `fdatasync` instead of one
    /// per point — the standard batch-commit pattern used by every production
    /// database.
    ///
    /// ## Durability (standard WAL semantics)
    /// The batch is appended as one WAL frame per record followed by a single
    /// `sync()`. The call is **acknowledged** (returns `Ok`) only after that
    /// `sync()` returns, at which point the whole batch is durable. If the
    /// process crashes before the `sync()` returns the batch was never
    /// acknowledged — but recovery is point-in-time, not all-or-nothing: WAL
    /// replay keeps every intact frame up to the first torn one (see
    /// [`wal::read_all`]), so an un-acknowledged batch may leave a durable
    /// **prefix** rather than nothing. A caller that sees no response should
    /// retry the *whole* batch; that is safe because upserts are idempotent by
    /// `external_id` — a replayed row shadows any earlier copy and the shadowed
    /// bytes are reclaimed by compaction. If you need true all-or-nothing batch
    /// atomicity across a crash, that requires a WAL commit-marker frame (a
    /// format change) and is not provided here.
    ///
    /// `records` is `(external_id, vector, payload_bytes)` slices; the vectors
    /// must match the collection's dimensionality or the call returns an error
    /// before writing anything.
    pub fn upsert_batch(
        &mut self,
        collection: CollectionId,
        records: &[(&str, &[f32], &[u8])],
    ) -> Result<u64> {
        if records.is_empty() {
            return Ok(0);
        }
        let dim = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .descriptor
            .dim as usize;
        for (_, vector, _) in records {
            if vector.len() != dim {
                return Err(CoreError::InvalidArgument(format!(
                    "vector has {} dims, collection expects {dim}",
                    vector.len()
                )));
            }
        }

        // Build one WalEntry per record, advancing the LSN for each.
        let mut entries: Vec<WalEntry> = Vec::with_capacity(records.len());
        for (ext_id, vector, payload) in records {
            let lsn = self.next_lsn;
            self.next_lsn = lsn.next();
            entries.push(WalEntry {
                lsn,
                op: WalOp::Upsert {
                    collection_id: collection,
                    external_id: ext_id.to_string(),
                    vector: f32_to_le_bytes(vector),
                    payload: payload.to_vec(),
                },
            });
        }

        // Append all records without syncing, then ONE fdatasync.
        for entry in &entries {
            self.wal.append(self.keyring.catalog_codec(), entry)?;
        }
        self.wal.sync()?;

        // Publish and apply in commit order.
        for entry in &entries {
            self.publish(entry);
            if let WalOp::Upsert {
                external_id,
                vector,
                payload,
                ..
            } = &entry.op
            {
                let state = self
                    .collections
                    .get_mut(&collection)
                    .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
                state.apply_upsert(external_id, vector.clone(), payload.clone());
            }
        }
        Ok(records.len() as u64)
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
        let entry = WalEntry {
            lsn,
            op: WalOp::Delete {
                collection_id: collection,
                external_id: external_id.to_owned(),
            },
        };
        self.wal.append_sync(self.keyring.catalog_codec(), &entry)?;
        self.next_lsn = lsn.next();
        self.publish(&entry);
        let state = self
            .collections
            .get_mut(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        state.apply_delete(external_id);
        Ok(true)
    }

    /// Build the validated [`WalOp`] that [`Store::create_collection`] would log,
    /// **without** applying it. The per-shard Raft write path (ADR-0067) proposes
    /// this op through consensus so a quorum commits it before any member applies
    /// it (via [`Store::apply_replicated`]). The new collection's id is assigned
    /// here, on the leader, and carried in the op exactly as a direct create would
    /// — so every member applies the same id; the caller serializes concurrent
    /// creates so two cannot claim the same `next_collection_id`.
    pub fn prepare_create_collection(&self, name: &str, descriptor: &Descriptor) -> Result<WalOp> {
        if self.name_index.contains_key(name) {
            return Err(CoreError::AlreadyExists(format!("collection {name}")));
        }
        if descriptor.dim == 0 {
            return Err(CoreError::InvalidArgument(
                "dim must be non-zero".to_owned(),
            ));
        }
        Ok(WalOp::CreateCollection {
            collection_id: CollectionId(self.next_collection_id),
            name: name.to_owned(),
            descriptor: postcard::to_allocvec(descriptor)?,
        })
    }

    /// Build the validated [`WalOp::Upsert`] that [`Store::upsert`] would log,
    /// without applying it (the Raft write path; see
    /// [`Store::prepare_create_collection`]). The vector is encoded identically to
    /// the direct path, so a member applying the proposed op reaches the same state
    /// a direct upsert would.
    pub fn prepare_upsert(
        &self,
        collection: CollectionId,
        external_id: &str,
        vector: &[f32],
        payload: &[u8],
    ) -> Result<WalOp> {
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
        Ok(WalOp::Upsert {
            collection_id: collection,
            external_id: external_id.to_owned(),
            vector: f32_to_le_bytes(vector),
            payload: payload.to_vec(),
        })
    }

    /// Build the [`WalOp::Delete`] that [`Store::delete`] would log, or `None` if
    /// the point does not exist, without applying it (the Raft write path).
    pub fn prepare_delete(
        &self,
        collection: CollectionId,
        external_id: &str,
    ) -> Result<Option<WalOp>> {
        let existed = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?
            .primary
            .contains_key(external_id);
        Ok(existed.then(|| WalOp::Delete {
            collection_id: collection,
            external_id: external_id.to_owned(),
        }))
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
                let vector_bytes = segment.read_vector(state.codec.as_ref(), row, state.stride)?;
                let payload = segment.read_payload(state.codec.as_ref(), row)?;
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

    /// A clone of a collection's page codec, for a component that seals its own
    /// files with that collection's key — e.g. a disk-resident index artifact
    /// (ADR-0019). The same owned handle can both write and `mmap`-open the
    /// artifact, so it shares the collection's data-encryption key.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] if the collection is unknown.
    pub fn collection_codec_clone(&self, collection: CollectionId) -> Result<Box<dyn PageCodec>> {
        self.collections
            .get(&collection)
            .map(|s| s.codec.clone_box())
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))
    }

    /// The store's root data directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The current manifest version — the catalog generation a snapshot of this
    /// store captures (ADR-0050).
    #[must_use]
    pub fn manifest_version(&self) -> u64 {
        self.manifest_version
    }

    /// The directory that holds a collection's index artifacts
    /// (`<data_dir>/collections/<id>/index`). Not created by this call.
    #[must_use]
    pub fn index_dir(&self, collection: CollectionId) -> PathBuf {
        collection_dir(&self.dir, collection).join("index")
    }

    /// Read and decrypt the current durable index snapshot for a collection, if
    /// one is referenced by the manifest (ADR-0025). Returns the opaque blob the
    /// index layer wrote at the last checkpoint, or `None` if the index must be
    /// rebuilt (no snapshot, or a store written before v2).
    ///
    /// # Errors
    /// [`CoreError::NotFound`] if the collection is unknown, or an I/O / decrypt /
    /// page-integrity error reading the snapshot file.
    pub fn read_index_snapshot(&self, collection: CollectionId) -> Result<Option<Vec<u8>>> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        let Some(snap) = &state.index_snapshot else {
            return Ok(None);
        };
        let path = self
            .index_dir(collection)
            .join(index_snapshot_file_name(snap.id));
        let body = read_paged(&path, state.codec.as_ref(), PageType::IndexBlock)?;
        Ok(Some(body))
    }

    /// The post-checkpoint mutations a restored index snapshot must replay to
    /// catch up to the current state (ADR-0025): the active-buffer upserts and the
    /// external ids whose checkpointed row died this window. Both are bounded by
    /// the checkpoint cadence, not the collection size.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] if the collection is unknown.
    pub fn recovery_tail(&self, collection: CollectionId) -> Result<RecoveryTail> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        let mut upserts = Vec::with_capacity(state.active_index.len());
        for (ext_id, &row) in &state.active_index {
            let ar = &state.active[row as usize];
            upserts.push((
                ext_id.clone(),
                Record {
                    vector: le_bytes_to_f32(&ar.vector),
                    payload: ar.payload.clone(),
                },
            ));
        }
        let mut deleted = Vec::new();
        for (&seg_idx, bitmap) in &state.dead_this_window {
            if let Some(seg) = state.sealed.get(seg_idx as usize) {
                let row_ids = seg.row_ids();
                for row in bitmap.iter() {
                    if let Some(ext) = row_ids.get(row as usize) {
                        deleted.push(ext.clone());
                    }
                }
            }
        }
        Ok(RecoveryTail { upserts, deleted })
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

    /// The live external ids whose payload satisfies an indexable `predicate`,
    /// resolved through the sealed segments' secondary indexes (`.sec`) plus a
    /// scan of the active buffer. The result is sorted and de-duplicated. This is
    /// the pre-filter primitive the query planner builds hybrid search on.
    ///
    /// # Errors
    /// [`CoreError::NotFound`] if the collection is unknown, or
    /// [`CoreError::InvalidArgument`] if the predicate's field is not declared
    /// filterable in the collection schema.
    pub fn matching_ids(
        &self,
        collection: CollectionId,
        predicate: &SecPredicate,
    ) -> Result<Vec<String>> {
        let state = self
            .collections
            .get(&collection)
            .ok_or_else(|| CoreError::NotFound(format!("collection {collection}")))?;
        let field_type = state
            .descriptor
            .filterable
            .iter()
            .find(|f| f.path == predicate.field())
            .map(|f| f.field_type)
            .ok_or_else(|| {
                CoreError::InvalidArgument(format!("field {} is not filterable", predicate.field()))
            })?;

        let mut out: Vec<String> = Vec::new();
        // Sealed segments: query each `.sec`, keeping rows still live here (a row
        // dead or shadowed no longer has the primary index pointing at it).
        for (seg_idx, segment) in state.sealed.iter().enumerate() {
            let seg_idx = seg_idx as u32;
            let Some(rows) = segment.sec_query(predicate)? else {
                continue;
            };
            for row in rows {
                if segment.is_dead(row) {
                    continue;
                }
                let Some(ext_id) = segment.row_ids().get(row as usize) else {
                    continue;
                };
                if matches!(
                    state.primary.get(ext_id),
                    Some(Loc::Sealed { seg: s, row: r }) if *s == seg_idx && *r == row
                ) {
                    out.push(ext_id.clone());
                }
            }
        }
        // Active (un-checkpointed) rows: evaluate the predicate directly.
        for (ext_id, &row) in &state.active_index {
            if let Some(active) = state.active.get(row as usize)
                && sec::payload_matches(predicate, field_type, &active.payload)
            {
                out.push(ext_id.clone());
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Seal everything changed since the last checkpoint into new immutable
    /// segments, install a new manifest atomically, rotate the WAL, and reclaim
    /// superseded files. A no-op if nothing has changed since the last
    /// checkpoint. Crash-safe at every step (see the module docs).
    ///
    /// Equivalent to [`Store::checkpoint_with_index_snapshots`] with no index
    /// snapshots (any existing snapshot references are cleared).
    pub fn checkpoint(&mut self) -> Result<()> {
        self.checkpoint_with_index_snapshots(&HashMap::new())
    }

    /// Like [`Store::checkpoint`], but also durably captures the supplied
    /// per-collection index snapshots (ADR-0025): each opaque blob is sealed with
    /// its collection's codec, fsync'd, and referenced by the same atomic manifest
    /// swap that publishes the segments — so the `(segments, index)` pair is
    /// consistent at one LSN. The map is the *complete* set for this checkpoint; a
    /// collection absent from it has any existing snapshot cleared, so a
    /// referenced snapshot's LSN always equals the new checkpoint's LSN.
    pub fn checkpoint_with_index_snapshots(
        &mut self,
        index_snapshots: &HashMap<CollectionId, Vec<u8>>,
    ) -> Result<()> {
        let last_lsn = Lsn(self.next_lsn.value().saturating_sub(1));
        if last_lsn.value() <= self.last_checkpointed_lsn.value() {
            return Ok(()); // nothing new since the last checkpoint
        }
        let mut cids: Vec<CollectionId> = self.collections.keys().copied().collect();
        cids.sort();
        let segment_lsn_low = self.last_checkpointed_lsn.next();
        let new_version = self.manifest_version + 1;

        // Phase A: for each collection with pending changes, persist the window's
        // dead rows into the affected segments' `.del` bitmaps, seal the active
        // buffer into a new segment (if any), and re-open it ready to install.
        let mut pending: HashMap<CollectionId, PendingSegment> = HashMap::new();
        for &cid in &cids {
            if !self.collections[&cid].has_pending() {
                continue;
            }
            let seg_dir = segments_dir(&self.dir, cid);
            fs::create_dir_all(&seg_dir).map_err(|e| CoreError::io(&seg_dir, e))?;
            // This collection's own codec (its data-encryption key under an
            // envelope key-ring) seals its segments and tombstone bitmaps.
            let codec = self.collections[&cid].codec.clone_box();

            // Merge the window's dead rows into each affected segment's tombstone
            // bitmap and rewrite it atomically (temp + rename).
            {
                let state = &self.collections[&cid];
                for (&seg_idx, newly_dead) in &state.dead_this_window {
                    if let Some(seg) = state.sealed.get(seg_idx as usize) {
                        let mut merged = seg.dead_bitmap();
                        merged |= newly_dead;
                        segment::write_del(&seg_dir, seg.seg_id, codec.as_ref(), &merged)?;
                    }
                }
            }

            // Seal the active buffer (in deterministic id order) into a new
            // segment, if there is anything to seal. The borrow of
            // `self.collections` ends with this block, before the commit phase.
            let new_seg = if self.collections[&cid].active_index.is_empty() {
                None
            } else {
                let seg_id = self.next_segment_id;
                self.next_segment_id += 1;
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
                    segment::write_segment(
                        &seg_dir,
                        seg_id,
                        codec.as_ref(),
                        &seal_rows,
                        &state.descriptor.filterable,
                    )?;
                    seal_rows.len() as u64
                };
                Some((seg_id, row_count))
            };

            // Make the new files and their parent directories durable before the
            // manifest references them.
            fsync_dir(&seg_dir)?;
            fsync_dir(&collection_dir(&self.dir, cid))?;
            fsync_dir(&self.dir.join("collections"))?;
            fsync_dir(&self.dir)?;

            if let Some((seg_id, row_count)) = new_seg {
                let sealed = segment::open_segment(&seg_dir, seg_id, codec.as_ref())?;
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
                    },
                );
            }
        }

        // Phase A2: persist the supplied index snapshots (ADR-0025), each sealed
        // with its collection's codec and fsync'd, so the manifest swap can
        // reference a durable file. The snapshot file id is the new manifest
        // version, unique per checkpoint.
        let mut new_index_refs: HashMap<CollectionId, IndexSnapshotRef> = HashMap::new();
        for &cid in &cids {
            let Some(blob) = index_snapshots.get(&cid) else {
                continue;
            };
            let index_dir = self.index_dir(cid);
            fs::create_dir_all(&index_dir).map_err(|e| CoreError::io(&index_dir, e))?;
            let codec = self.collections[&cid].codec.clone_box();
            let path = index_dir.join(index_snapshot_file_name(new_version));
            write_paged(
                &path,
                codec.as_ref(),
                PageType::IndexBlock,
                new_version,
                blob,
            )?;
            fsync_dir(&index_dir)?;
            fsync_dir(&collection_dir(&self.dir, cid))?;
            new_index_refs.insert(
                cid,
                IndexSnapshotRef {
                    id: new_version,
                    lsn: last_lsn,
                },
            );
        }

        // Phase B: build and atomically install the new manifest.
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
                index_snapshot: new_index_refs.get(&cid).cloned(),
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
        manifest::write_manifest(&self.dir, &new_manifest, self.keyring.catalog_codec())?;

        // Phase C: commit in-memory state, rotate the WAL, GC superseded files.
        self.manifest_version = new_version;
        self.last_checkpointed_lsn = last_lsn;
        for &cid in &cids {
            let Some(state) = self.collections.get_mut(&cid) else {
                continue;
            };
            // Fold this window's dead rows into the in-memory segment bitmaps
            // (the `.del` files were already persisted in Phase A).
            let dead_window = std::mem::take(&mut state.dead_this_window);
            for (seg_idx, bitmap) in dead_window {
                if let Some(seg) = state.sealed.get_mut(seg_idx as usize) {
                    seg.mark_dead(&bitmap);
                }
            }
            // Install the new segment, if any, repointing its now-sealed ids.
            if let Some(p) = pending.remove(&cid) {
                let seg_idx = state.sealed.len() as u32;
                for (row, ext_id) in p.sealed.row_ids().iter().enumerate() {
                    state.primary.insert(
                        ext_id.clone(),
                        Loc::Sealed {
                            seg: seg_idx,
                            row: row as u32,
                        },
                    );
                }
                state.sealed.push(p.sealed);
                state.segments_meta.push(p.seg_ref);
            }
            state.active.clear();
            state.active_index.clear();
            state.index_snapshot = new_index_refs.get(&cid).cloned();
        }
        self.rotate_wal()?;
        gc_orphan_segments(&self.dir, &new_manifest, self.keyring.as_ref())?;
        gc_orphan_index_snapshots(&self.dir, &new_manifest)?;
        self.auto_compact()?;
        Ok(())
    }

    /// Compact every collection with reclaimable space: merge its sealed segments,
    /// dropping dead (deleted or shadowed) rows, into a single fresh segment. Each
    /// collection commits via its own atomic manifest swap and is crash-safe like
    /// a checkpoint — the old segments stay valid until the swap, so a crash
    /// before it leaves the pre-compaction state intact.
    pub fn compact(&mut self) -> Result<()> {
        for cid in self.sorted_cids() {
            if self.reclaimable(cid) {
                self.compact_collection(cid)?;
            }
        }
        Ok(())
    }

    // Compact at the end of a checkpoint, but bound the work so compaction stays
    // off the checkpoint's critical path: at most **one** collection is compacted
    // per checkpoint (the first over-threshold in id order), so a checkpoint's added
    // latency is a single collection's streamed (memory-bounded, ADR-0068) merge
    // rather than a fan-out across every over-threshold collection. The rest compact
    // on subsequent checkpoints, so multi-collection compaction amortizes across the
    // checkpoint stream. Building the merged segment fully off the write lock — a
    // background worker mirroring ADR-0062's plan→build→commit-or-abort-on-race — is
    // the documented next step (ADR-0068); today's engine is single-writer.
    fn auto_compact(&mut self) -> Result<()> {
        if let Some(cid) = self
            .sorted_cids()
            .into_iter()
            .find(|&cid| self.needs_compaction(cid))
        {
            self.compact_collection(cid)?;
        }
        Ok(())
    }

    fn sorted_cids(&self) -> Vec<CollectionId> {
        let mut cids: Vec<CollectionId> = self.collections.keys().copied().collect();
        cids.sort();
        cids
    }

    // Whether a collection has any space to reclaim: more than one segment to
    // merge, or any dead rows in a segment.
    fn reclaimable(&self, cid: CollectionId) -> bool {
        self.collections.get(&cid).is_some_and(|s| {
            s.sealed.len() > 1
                || s.sealed
                    .iter()
                    .any(|seg| seg.live_count() < u64::from(seg.row_count()))
        })
    }

    // Whether a collection has crossed the automatic compaction threshold: many
    // segments to merge, or at least half of its sealed rows dead.
    fn needs_compaction(&self, cid: CollectionId) -> bool {
        let Some(s) = self.collections.get(&cid) else {
            return false;
        };
        if s.sealed.is_empty() {
            return false;
        }
        let total: u64 = s.sealed.iter().map(|seg| u64::from(seg.row_count())).sum();
        let live: u64 = s.sealed.iter().map(SealedSegment::live_count).sum();
        s.sealed.len() >= COMPACT_MIN_SEGMENTS || (total > 0 && (total - live) * 2 >= total)
    }

    // Merge one collection's sealed segments into a single fresh segment holding
    // only its live rows, install it atomically, and reclaim the old files.
    fn compact_collection(&mut self, cid: CollectionId) -> Result<()> {
        // This collection's own codec (its DEK under an envelope key-ring) seals
        // both the rows read from the old segments and the merged one written.
        let codec = self
            .collections
            .get(&cid)
            .ok_or_else(|| CoreError::NotFound(format!("collection {cid}")))?
            .codec
            .clone_box();
        // Plan the merge from directory metadata only — the ordered (segment, row)
        // of every live sealed row (active rows are untouched). `primary` is
        // ordered, so the rewritten segment is deterministic. This holds O(rows) of
        // 8-byte locations, never the vectors or payloads — those stream one row at
        // a time into the writer below, so a large collection compacts within a
        // bounded memory envelope (ADR-0068).
        let (plan, row_count, stride) = {
            let state = self
                .collections
                .get(&cid)
                .ok_or_else(|| CoreError::NotFound(format!("collection {cid}")))?;
            let mut plan: Vec<(u32, u32)> = Vec::new();
            for &loc in state.primary.values() {
                if let Loc::Sealed { seg, row } = loc {
                    plan.push((seg, row));
                }
            }
            let row_count = plan.len();
            (plan, row_count, state.stride)
        };

        // The merged segment spans the full lsn range of its inputs.
        let (lsn_low, lsn_high) = {
            let state = &self.collections[&cid];
            let low = state
                .segments_meta
                .iter()
                .map(|s| s.lsn_low.value())
                .min()
                .map(Lsn)
                .unwrap_or(Lsn::ZERO);
            let high = state
                .segments_meta
                .iter()
                .map(|s| s.lsn_high.value())
                .max()
                .map(Lsn)
                .unwrap_or(self.last_checkpointed_lsn);
            (low, high)
        };

        let seg_id = self.next_segment_id;
        self.next_segment_id += 1;
        let seg_dir = segments_dir(&self.dir, cid);
        fs::create_dir_all(&seg_dir).map_err(|e| CoreError::io(&seg_dir, e))?;
        // Stream the planned rows straight from the source segments' mmaps into the
        // new segment; only one row's vector + payload is resident at a time.
        {
            let state = &self.collections[&cid];
            let mut rows = plan.into_iter();
            segment::write_segment_streaming(
                &seg_dir,
                seg_id,
                codec.as_ref(),
                row_count,
                &state.descriptor.filterable,
                || match rows.next() {
                    None => Ok(None),
                    Some((seg, row)) => {
                        let segment = state.sealed.get(seg as usize).ok_or_else(|| {
                            CoreError::MalformedPage(format!("dangling segment index {seg}"))
                        })?;
                        let ext_id = segment
                            .row_ids()
                            .get(row as usize)
                            .ok_or_else(|| {
                                CoreError::MalformedPage(format!("segment {seg} has no row {row}"))
                            })?
                            .clone();
                        let vector = segment.read_vector(codec.as_ref(), row, stride)?;
                        let payload = segment.read_payload(codec.as_ref(), row)?;
                        Ok(Some((ext_id, vector, payload)))
                    }
                },
            )?;
        }
        fsync_dir(&seg_dir)?;
        fsync_dir(&collection_dir(&self.dir, cid))?;
        fsync_dir(&self.dir.join("collections"))?;
        fsync_dir(&self.dir)?;
        let new_ref = SegmentRef {
            id: seg_id,
            row_count: row_count as u64,
            lsn_low,
            lsn_high,
        };
        let sealed = segment::open_segment(&seg_dir, seg_id, codec.as_ref())?;

        // New manifest: this collection now has exactly one segment; others are
        // unchanged. The atomic swap is the commit point.
        let new_version = self.manifest_version + 1;
        let mut entries = Vec::with_capacity(self.collections.len());
        for &other in &self.sorted_cids() {
            let state = &self.collections[&other];
            let segs = if other == cid {
                vec![new_ref.clone()]
            } else {
                state.segments_meta.clone()
            };
            entries.push(CollectionEntry {
                id: state.id,
                name: state.name.clone(),
                descriptor: postcard::to_allocvec(&state.descriptor)?,
                segments: segs,
                index_snapshot: state.index_snapshot.clone(),
            });
        }
        let new_manifest = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            version: new_version,
            last_checkpointed_lsn: self.last_checkpointed_lsn,
            next_collection_id: self.next_collection_id,
            next_segment_id: self.next_segment_id,
            collections: entries,
        };
        manifest::write_manifest(&self.dir, &new_manifest, self.keyring.catalog_codec())?;

        // Commit: replace the segments (dropping the old mmaps before the files
        // are reclaimed), repoint the now-merged ids, and drop pending tombstones
        // (their rows no longer exist).
        self.manifest_version = new_version;
        let row_ids: Vec<String> = sealed.row_ids().to_vec();
        if let Some(state) = self.collections.get_mut(&cid) {
            state.sealed = vec![sealed];
            state.segments_meta = vec![new_ref];
            state.dead_this_window.clear();
            for (row, ext_id) in row_ids.into_iter().enumerate() {
                state.primary.insert(
                    ext_id,
                    Loc::Sealed {
                        seg: 0,
                        row: row as u32,
                    },
                );
            }
        }
        gc_orphan_segments(&self.dir, &new_manifest, self.keyring.as_ref())?;
        gc_orphan_index_snapshots(&self.dir, &new_manifest)?;
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
    keyring: &dyn KeyRing,
) -> Result<()> {
    match &entry.op {
        WalOp::CreateCollection {
            collection_id,
            name,
            descriptor,
        } => {
            let descriptor = Descriptor::decode(descriptor)?;
            // The key material was provisioned before this record was made
            // durable, so the collection's codec is available on replay.
            let codec = keyring.collection_codec(*collection_id)?;
            name_index.insert(name.clone(), *collection_id);
            collections.insert(
                *collection_id,
                CollectionState::new(*collection_id, name.clone(), descriptor, codec),
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
                state.apply_upsert(external_id, vector.clone(), payload.clone());
            }
        }
        WalOp::Delete {
            collection_id,
            external_id,
        } => {
            if let Some(state) = collections.get_mut(collection_id) {
                state.apply_delete(external_id);
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
fn gc_orphan_segments(dir: &Path, mfst: &Manifest, keyring: &dyn KeyRing) -> Result<()> {
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
            // A dropped collection: crypto-shred its key first (so a crash before
            // the files are reclaimed still leaves them unrecoverable), then
            // reclaim its whole directory.
            keyring.shred_collection(CollectionId(cid))?;
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
            let Some(name) = seg.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // A crash-leftover temp (an interrupted `.del` rewrite) is always junk.
            if segment::is_temp_file(&name) {
                remove_file_if_present(&path)?;
                continue;
            }
            let Some(seg_id) = segment::seg_id_of_file(&name) else {
                continue;
            };
            if !referenced.contains(&(cid, seg_id)) {
                remove_file_if_present(&path)?;
            }
        }
    }
    Ok(())
}

// Delete stale or orphaned index snapshot files (`idx-*`) that a live
// collection's manifest entry no longer references — a superseded snapshot, or
// one written by a checkpoint that crashed before its manifest swap (ADR-0025).
// Non-snapshot index artifacts (e.g. the disk graph) are left untouched; dropped
// collections are reclaimed wholesale by `gc_orphan_segments`.
fn gc_orphan_index_snapshots(dir: &Path, mfst: &Manifest) -> Result<()> {
    for c in &mfst.collections {
        let index_dir = collection_dir(dir, c.id).join("index");
        if !index_dir.is_dir() {
            continue;
        }
        let keep = c.index_snapshot.as_ref().map(|r| r.id);
        for entry in fs::read_dir(&index_dir).map_err(|e| CoreError::io(&index_dir, e))? {
            let entry = entry.map_err(|e| CoreError::io(&index_dir, e))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(id) = index_snapshot_id_of_file(&name) else {
                continue; // not a snapshot file
            };
            if Some(id) != keep {
                remove_file_if_present(&entry.path())?;
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

// Name of a collection's index snapshot file for snapshot id `id` (ADR-0025);
// zero-padded so lexical order matches numeric order.
fn index_snapshot_file_name(id: u64) -> String {
    format!("idx-{id:010}")
}

// Parse a snapshot id from an `idx-NNNNNNNNNN` file name, or `None` for any other
// file (so non-snapshot index artifacts are ignored by snapshot GC).
fn index_snapshot_id_of_file(name: &str) -> Option<u64> {
    name.strip_prefix("idx-")
        .and_then(|s| s.parse::<u64>().ok())
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
    fn upsert_batch_commits_all_on_sync() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            let vecs: Vec<([f32; 4], String)> = (0..8u32)
                .map(|i| ([i as f32; 4], format!("k{i}")))
                .collect();
            let payload = b"{}";
            let records: Vec<(&str, &[f32], &[u8])> = vecs
                .iter()
                .map(|(v, id)| (id.as_str(), v.as_slice(), payload.as_slice()))
                .collect();
            let n = s.upsert_batch(c, &records).unwrap();
            assert_eq!(n, 8);
            // All points readable from in-memory state immediately.
            for (_, id) in &vecs {
                assert!(s.get(c, id).unwrap().is_some(), "missing {id}");
            }
        }
        // Re-open: WAL replay must restore all 8 points.
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 8);
        for i in 0..8u32 {
            let got = s.get(c, &format!("k{i}")).unwrap().unwrap();
            assert_eq!(got.vector, vec![i as f32; 4]);
        }
    }

    #[test]
    fn upsert_batch_dim_mismatch_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        // First record correct, second has wrong dim — the whole batch must fail.
        let bad: &[(&str, &[f32], &[u8])] = &[
            ("a", &[1.0, 2.0, 3.0, 4.0], b"{}"),
            ("b", &[1.0, 2.0], b"{}"), // wrong dim
        ];
        assert!(matches!(
            s.upsert_batch(c, bad),
            Err(CoreError::InvalidArgument(_))
        ));
        // Neither point was written.
        assert!(s.get(c, "a").unwrap().is_none());
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
    fn open_with_keyring_round_trips_through_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s =
                Store::open_with_keyring(tmp.path(), Box::new(SingleCodecKeyRing::plaintext()))
                    .unwrap();
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
            s.checkpoint().unwrap();
            s.upsert(c, "b", &[5.0; 4], b"{}").unwrap();
        }
        // Reopen through the same key-ring: data recovers from the sealed segment
        // and the WAL tail, each opened with the collection's own codec.
        let s = Store::open_with_keyring(tmp.path(), Box::new(SingleCodecKeyRing::plaintext()))
            .unwrap();
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 2);
        assert_eq!(
            s.get(c, "a").unwrap().unwrap().vector,
            vec![1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(s.get(c, "b").unwrap().unwrap().vector, vec![5.0; 4]);
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

    #[test]
    fn delete_persists_via_del_bitmap_across_reopen() {
        // Five rows in one segment; deleting one is 20% dead with a single
        // segment, so auto-compaction does not fire — the delete must survive
        // purely via the persisted `.del` tombstone bitmap.
        let tmp = tempfile::tempdir().unwrap();
        let cid;
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            cid = c;
            for i in 0..5u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap();
            s.delete(c, "k2").unwrap();
            s.checkpoint().unwrap();
            assert_eq!(
                s.collections[&c].sealed.len(),
                1,
                "no new segment for a delete-only window"
            );
        }
        // The tombstone bitmap was written for segment 0.
        assert!(
            segments_dir(tmp.path(), cid)
                .join("seg-0000000000.del")
                .exists(),
            ".del must be persisted for the deleted row"
        );
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert!(s.get(c, "k2").unwrap().is_none());
        assert_eq!(s.len(c).unwrap(), 4);
        for i in [0u32, 1, 3, 4] {
            assert!(s.get(c, &format!("k{i}")).unwrap().is_some());
        }
    }

    #[test]
    fn shadowed_row_is_tombstoned_and_latest_wins() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            for i in 0..5u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"v1")
                    .unwrap();
            }
            s.checkpoint().unwrap(); // seg 0
            s.upsert(c, "k2", &[99.0; 4], b"v2").unwrap();
            s.checkpoint().unwrap(); // seg 1 holds the new k2; seg 0 row tombstoned
        }
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.len(c).unwrap(), 5); // k2 counted once
        let got = s.get(c, "k2").unwrap().unwrap();
        assert_eq!(got.vector, vec![99.0; 4]);
        assert_eq!(got.payload, b"v2");
    }

    #[test]
    fn compaction_merges_segments_reclaims_and_keeps_active_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let cid;
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            cid = c;
            for i in 0..6u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap(); // seg 0: k0..k5
            for i in 6..12u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap(); // seg 1: k6..k11
            s.delete(c, "k0").unwrap();
            s.delete(c, "k6").unwrap();
            s.checkpoint().unwrap(); // tombstones only; still two segments
            assert_eq!(s.collections[&c].sealed.len(), 2);

            // An un-checkpointed row must survive the compaction untouched.
            s.upsert(c, "fresh", &[7.0; 4], b"new").unwrap();
            s.compact().unwrap();
            assert_eq!(s.collections[&c].sealed.len(), 1, "segments merged to one");
            assert!(
                !segments_dir(tmp.path(), cid)
                    .join("seg-0000000000.dir")
                    .exists(),
                "old segment files reclaimed"
            );
            assert_eq!(s.len(c).unwrap(), 11); // 10 live sealed + 1 active
            assert!(s.get(c, "k0").unwrap().is_none());
            assert!(s.get(c, "k6").unwrap().is_none());
            assert_eq!(s.get(c, "k5").unwrap().unwrap().vector, vec![5.0; 4]);
            assert_eq!(s.get(c, "fresh").unwrap().unwrap().payload, b"new");
        }
        // Everything survives a reopen, including the active row via WAL replay.
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.collections[&c].sealed.len(), 1);
        assert_eq!(s.len(c).unwrap(), 11);
        assert!(s.get(c, "k0").unwrap().is_none());
        assert_eq!(s.get(c, "fresh").unwrap().unwrap().vector, vec![7.0; 4]);
        assert_eq!(s.get(c, "k11").unwrap().unwrap().vector, vec![11.0; 4]);
    }

    #[test]
    fn auto_compaction_merges_many_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        // Eight checkpoints create eight segments; the eighth checkpoint's
        // auto-compaction merges them.
        for ck in 0..8u32 {
            for i in 0..3u32 {
                let n = ck * 3 + i;
                s.upsert(c, &format!("k{n}"), &[n as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap();
        }
        assert!(
            s.collections[&c].sealed.len() < COMPACT_MIN_SEGMENTS,
            "auto-compaction should have merged the segments"
        );
        assert_eq!(s.len(c).unwrap(), 24);
        assert_eq!(s.get(c, "k0").unwrap().unwrap().vector, vec![0.0; 4]);
        assert_eq!(s.get(c, "k23").unwrap().unwrap().vector, vec![23.0; 4]);
    }

    #[test]
    fn rejects_pathological_collection_names() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let too_long = "a".repeat(MAX_COLLECTION_NAME_LEN + 1);
        let rejected: &[&str] = &[
            "",          // empty
            "a/b",       // path separator (a name is one REST path segment)
            "/leading",  // path separator
            "has space", // whitespace
            "tab\tname", // control character
            "new\nline", // control character
            "café",      // non-ASCII
            "emoji😀",   // non-ASCII
            &too_long,   // over the length cap
        ];
        for &name in rejected {
            assert!(
                matches!(
                    s.create_collection(name, desc()),
                    Err(CoreError::InvalidArgument(_))
                ),
                "name {name:?} should be rejected"
            );
        }
        // Accepted: the documented charset, up to the length cap.
        let max_len = "a".repeat(MAX_COLLECTION_NAME_LEN);
        for name in ["a", "my-collection", "v2.name_1", max_len.as_str()] {
            s.create_collection(name, desc())
                .unwrap_or_else(|e| panic!("name {name:?} should be accepted: {e}"));
        }
    }

    #[test]
    fn interrupted_compaction_before_manifest_swap_leaves_state_intact() {
        // `compact_collection` writes and fsyncs the merged segment, then the
        // manifest swap is the sole commit point (ADR-0005/0066). A crash in that
        // window leaves the merged segment orphaned — unreferenced by the still-old
        // manifest — so recovery serves the pre-compaction segments and GCs the
        // orphan, exactly like an interrupted checkpoint.
        let tmp = tempfile::tempdir().unwrap();
        let cid;
        {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            cid = c;
            for i in 0..6u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap(); // seg 0: k0..k5
            for i in 6..12u32 {
                s.upsert(c, &format!("k{i}"), &[i as f32; 4], b"{}")
                    .unwrap();
            }
            s.checkpoint().unwrap(); // seg 1: k6..k11
            assert_eq!(s.collections[&c].sealed.len(), 2);
        }

        // Simulate the interrupted compaction: a merged segment's files exist on
        // disk, but the manifest was never swapped to reference it.
        let seg_dir = segments_dir(tmp.path(), cid);
        let orphan_id = 9_999u64;
        for ext in ["vec", "pay", "dir"] {
            std::fs::write(
                seg_dir.join(format!("seg-{orphan_id:010}.{ext}")),
                b"partial",
            )
            .unwrap();
        }

        // Reopen: the pre-compaction two-segment state is intact and the orphan is
        // reclaimed — no data lost, no half-merged segment adopted.
        let s = open(tmp.path());
        assert_eq!(
            s.collections[&cid].sealed.len(),
            2,
            "pre-compaction segments still referenced"
        );
        assert_eq!(s.len(cid).unwrap(), 12);
        assert_eq!(s.get(cid, "k5").unwrap().unwrap().vector, vec![5.0; 4]);
        assert_eq!(s.get(cid, "k11").unwrap().unwrap().vector, vec![11.0; 4]);
        assert!(
            !seg_dir.join(format!("seg-{orphan_id:010}.vec")).exists(),
            "orphan merged segment reclaimed on recovery"
        );
    }

    #[test]
    fn matching_ids_spans_secondary_index_and_active_buffer() {
        use crate::descriptor::FilterableField;
        use crate::sec::SecValue;

        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let descriptor = Descriptor::new(4, Dtype::F32, DistanceMetric::L2).with_filterable(vec![
            FilterableField::keyword("city"),
            FilterableField::numeric("age"),
        ]);
        let c = s.create_collection("c", descriptor).unwrap();
        s.upsert(c, "a", &[0.0; 4], br#"{"city":"paris","age":30}"#)
            .unwrap();
        s.upsert(c, "b", &[0.0; 4], br#"{"city":"lyon","age":25}"#)
            .unwrap();
        s.upsert(c, "d", &[0.0; 4], br#"{"city":"paris","age":40}"#)
            .unwrap();
        s.checkpoint().unwrap();
        // An active (un-checkpointed) row, matched by scanning the buffer.
        s.upsert(c, "e", &[0.0; 4], br#"{"city":"paris","age":22}"#)
            .unwrap();

        let paris = || SecPredicate::Eq {
            field: "city".into(),
            value: SecValue::Keyword("paris".into()),
        };
        assert_eq!(s.matching_ids(c, &paris()).unwrap(), ["a", "d", "e"]);

        // Numeric range [25, 35]: 30 (a, sealed) and 25 (b, sealed); not 40 or 22.
        assert_eq!(
            s.matching_ids(
                c,
                &SecPredicate::Range {
                    field: "age".into(),
                    lo: Some(SecValue::Numeric(25.0)),
                    hi: Some(SecValue::Numeric(35.0)),
                    lo_inclusive: true,
                    hi_inclusive: true,
                }
            )
            .unwrap(),
            ["a", "b"]
        );

        // Deleting a sealed row drops it via the primary-consistency check.
        s.delete(c, "a").unwrap();
        assert_eq!(s.matching_ids(c, &paris()).unwrap(), ["d", "e"]);

        // A non-filterable field is rejected (the planner must post-filter it).
        assert!(matches!(
            s.matching_ids(
                c,
                &SecPredicate::Eq {
                    field: "country".into(),
                    value: SecValue::Keyword("fr".into()),
                }
            ),
            Err(CoreError::InvalidArgument(_))
        ));

        // Checkpoint seals the active row and the deletion; results survive reopen.
        s.checkpoint().unwrap();
        let s = open(tmp.path());
        let c = s.collection_id("c").unwrap();
        assert_eq!(s.matching_ids(c, &paris()).unwrap(), ["d", "e"]);
    }

    // ----- durable index snapshots (ADR-0025) -----

    // The `idx-*` snapshot files currently on disk for a collection, sorted.
    fn index_snapshot_files(dir: &Path, cid: CollectionId) -> Vec<String> {
        let idx = collection_dir(dir, cid).join("index");
        let mut names: Vec<String> = fs::read_dir(&idx)
            .map(|rd| {
                rd.filter_map(std::result::Result::ok)
                    .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                    .filter(|n| n.starts_with("idx-"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn index_snapshot_round_trips_through_checkpoint_and_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let blob = b"opaque-index-bytes".to_vec();
        let cid = {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
            s.checkpoint_with_index_snapshots(&HashMap::from([(c, blob.clone())]))
                .unwrap();
            // Available immediately, exactly one snapshot file on disk.
            assert_eq!(s.read_index_snapshot(c).unwrap(), Some(blob.clone()));
            assert_eq!(index_snapshot_files(tmp.path(), c).len(), 1);
            c
        };
        // Survives reopen, loaded via the manifest reference.
        let s = open(tmp.path());
        assert_eq!(s.read_index_snapshot(cid).unwrap(), Some(blob));
    }

    #[test]
    fn checkpoint_without_a_snapshot_clears_and_reclaims_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
        s.checkpoint_with_index_snapshots(&HashMap::from([(c, b"blob".to_vec())]))
            .unwrap();
        assert!(s.read_index_snapshot(c).unwrap().is_some());

        // A later plain checkpoint (with new data) carries no snapshot → cleared.
        s.upsert(c, "b", &[5.0, 6.0, 7.0, 8.0], b"{}").unwrap();
        s.checkpoint().unwrap();
        assert_eq!(s.read_index_snapshot(c).unwrap(), None);
        assert!(index_snapshot_files(tmp.path(), c).is_empty());

        let s = open(tmp.path());
        assert_eq!(s.read_index_snapshot(c).unwrap(), None);
    }

    #[test]
    fn a_new_snapshot_supersedes_and_reclaims_the_old_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
        s.checkpoint_with_index_snapshots(&HashMap::from([(c, b"first".to_vec())]))
            .unwrap();
        s.upsert(c, "b", &[5.0, 6.0, 7.0, 8.0], b"{}").unwrap();
        s.checkpoint_with_index_snapshots(&HashMap::from([(c, b"second".to_vec())]))
            .unwrap();

        assert_eq!(s.read_index_snapshot(c).unwrap(), Some(b"second".to_vec()));
        assert_eq!(index_snapshot_files(tmp.path(), c).len(), 1);
    }

    #[test]
    fn compaction_preserves_the_index_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open(tmp.path());
        let c = s.create_collection("c", desc()).unwrap();
        s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
        s.checkpoint_with_index_snapshots(&HashMap::from([(c, b"keep".to_vec())]))
            .unwrap();
        // More changes, then re-snapshot at the new floor and compact.
        s.upsert(c, "b", &[5.0, 6.0, 7.0, 8.0], b"{}").unwrap();
        s.delete(c, "a").unwrap();
        s.checkpoint_with_index_snapshots(&HashMap::from([(c, b"keep".to_vec())]))
            .unwrap();
        s.compact().unwrap();
        assert_eq!(s.read_index_snapshot(c).unwrap(), Some(b"keep".to_vec()));

        let s = open(tmp.path());
        assert_eq!(s.read_index_snapshot(c).unwrap(), Some(b"keep".to_vec()));
    }

    #[test]
    fn orphan_index_snapshot_is_reclaimed_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let cid = {
            let mut s = open(tmp.path());
            let c = s.create_collection("c", desc()).unwrap();
            s.upsert(c, "a", &[1.0, 2.0, 3.0, 4.0], b"{}").unwrap();
            s.checkpoint_with_index_snapshots(&HashMap::from([(c, b"live".to_vec())]))
                .unwrap();
            // Simulate a checkpoint that wrote a snapshot file but crashed before
            // the manifest swap: an unreferenced idx-* in the index dir.
            let stray = s.index_dir(c).join("idx-9999999999");
            fs::write(&stray, b"orphan").unwrap();
            c
        };
        let s = open(tmp.path());
        // Recovery reclaims the orphan but keeps the referenced snapshot.
        assert!(!s.index_dir(cid).join("idx-9999999999").exists());
        assert_eq!(s.read_index_snapshot(cid).unwrap(), Some(b"live".to_vec()));
    }
}
