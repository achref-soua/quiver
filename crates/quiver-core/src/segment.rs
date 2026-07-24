// SPDX-License-Identifier: AGPL-3.0-only
//! Sealed, immutable segments in the row-addressed on-disk format (ADR-0004),
//! with per-segment tombstones as roaring bitmaps (ADR-0020).
//!
//! Each checkpoint seals the rows upserted since the previous checkpoint into a
//! new immutable segment, written as three companion files named by a monotonic
//! segment id, plus an optional fourth that records which of its rows have since
//! died:
//!
//! - `seg-NNNNNNNNNN.vec` — the **vector column**: each live row's raw
//!   little-endian vector bytes, packed tightly at `row × stride`, read through
//!   an `mmap` ([`crate::blockfile`]). O(1) random access, cache-friendly scans.
//! - `seg-NNNNNNNNNN.pay` — the **payload heap**: each row's opaque payload bytes
//!   concatenated, also `mmap`-read.
//! - `seg-NNNNNNNNNN.ids` — the **id column** (ADR-0072): each row's external-id
//!   bytes concatenated in row order, `mmap`-read on demand like the payload heap.
//!   Keeping ids off the heap is what lets the primary index leave RAM.
//! - `seg-NNNNNNNNNN.dir` — the **row directory** ([`SegmentDir`]): per row, the
//!   `(offset, length)` of its id in the `.ids` column and of its payload in the
//!   `.pay` heap, plus `sorted_rows` — the row numbers ordered by external id, the
//!   forward-lookup index binary-searched by [`SealedSegment::lookup`]. A paged
//!   `postcard` blob ([`crate::paged`]) with per-page CRC integrity.
//! - `seg-NNNNNNNNNN.del` — the **tombstone bitmap**: a `roaring` bitmap of this
//!   segment's row indices that are no longer live (deleted, or shadowed by a
//!   newer upsert). Written atomically (temp + rename) since, unlike the other
//!   immutable files, it is rewritten as rows die; absent means no dead rows.
//!
//! Vectors, payloads, and ids live on disk and are decrypted on demand; only the
//! row directory (id/payload offsets + the sorted-row index) and the tombstone
//! bitmap are read into RAM. On recovery, a row that is tombstoned in its segment
//! is skipped, so each external id is live in at most one segment; the WAL tail is
//! then applied on top.

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::blockfile::{BlockFile, BlockWriter, write_blocks};
use crate::descriptor::FilterableField;
use crate::error::{CoreError, Result};
use crate::page::{PageCodec, PageType};
use crate::sec::{SecIndex, SecPredicate};

/// Current segment schema version. (v1 was the Phase-1 snapshot-delta `postcard`
/// blob; v2 the row-addressed layout with string tombstones; v3 moves tombstones
/// out of the directory into the roaring `.del` bitmap; v4 (ADR-0072) moves the
/// external ids out of the directory into the on-demand `.ids` column and adds the
/// `sorted_rows` forward index, so the primary index can leave RAM.)
pub(crate) const SEGMENT_FORMAT_VERSION: u16 = 4;

/// One row's entry in the segment directory. The row's vector lives at
/// `row_index × stride` in the `.vec` column; its external id at `(id_off, id_len)`
/// in the `.ids` column; its payload at `(pay_off, pay_len)` in the `.pay` heap.
/// `row_index` is the entry's position in [`SegmentDir::rows`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RowEntry {
    /// Byte offset of this row's external id within the `.ids` column.
    pub id_off: u64,
    /// Byte length of this row's external id.
    pub id_len: u32,
    /// Byte offset of this row's payload within the `.pay` heap.
    pub pay_off: u64,
    /// Byte length of this row's payload.
    pub pay_len: u32,
}

/// The `.dir` file: the row directory of a sealed segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentDir {
    /// Schema version of this segment's files.
    pub format_version: u16,
    /// Segment id (matches its [`crate::manifest::SegmentRef`] and file names).
    pub segment_id: u64,
    /// Rows sealed into this segment, in `.vec`/`.pay`/`.ids` row order.
    pub rows: Vec<RowEntry>,
    /// Row numbers ordered by their external id (ADR-0072): the forward-lookup
    /// index. `sorted_rows[i]` is the row whose id sorts to position `i`, so
    /// [`SealedSegment::lookup`] binary-searches it, decrypting the candidate row's
    /// id from `.ids` to compare. Within a segment each id is unique.
    pub sorted_rows: Vec<u32>,
}

/// A row to seal, borrowing its bytes from the engine's active buffer.
pub(crate) struct SealRow<'a> {
    /// External id.
    pub external_id: &'a str,
    /// Raw little-endian vector bytes; length must equal the collection stride.
    pub vector: &'a [u8],
    /// Opaque payload bytes.
    pub payload: &'a [u8],
}

/// The location of one row's bytes within a `.pay` heap or `.ids` column: a
/// `(offset, length)` into the paged block file, resolved by `read_range`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ByteLoc {
    off: u64,
    len: u32,
}

/// A sealed segment opened for reads: `mmap` handles for the vector column, payload
/// heap, and id column; the row → (id, payload) location directory; the sorted-row
/// forward index; the tombstone bitmap; and the secondary index.
pub(crate) struct SealedSegment {
    /// Segment id; names the files and matches the manifest.
    pub seg_id: u64,
    vec: BlockFile,
    pay: BlockFile,
    // `ids`/`idlocs`/`sorted_rows` back the on-demand `read_id`/`lookup` read path
    // (ADR-0072). External ids are no longer RAM-resident — that was the ~316 B/pt
    // wall Increment C removes; callers read a row's id on demand instead.
    ids: BlockFile,
    paylocs: Vec<ByteLoc>,
    // Where each row's external id lives in the `.ids` column (row order).
    idlocs: Vec<ByteLoc>,
    // Row numbers ordered by external id (ADR-0072): the forward-lookup index.
    sorted_rows: Vec<u32>,
    // Resident min/max id range (ADR-0076): the smallest and largest external id
    // in this segment, i.e. the ids of `sorted_rows` first and last. Derived on
    // open from the on-disk column (two decrypts, no format change), so `lookup`
    // can reject an out-of-range id with one string compare instead of a
    // decrypting binary search. `None` for an empty segment. This is the named
    // upgrade path of ADR-0072 — RAM-neutral (two strings per segment, and
    // segment count is bounded by compaction).
    id_range: Option<(String, String)>,
    // Rows of this segment that are no longer live.
    dead: RoaringBitmap,
    // Secondary index over the collection's filterable fields (empty if none).
    sec: SecIndex,
}

impl SealedSegment {
    /// Read row `row`'s external id from the `.ids` column, decrypting the page(s)
    /// it touches. The on-demand path the forward lookup and live scans use in
    /// place of a RAM-resident id vector (ADR-0072).
    pub(crate) fn read_id(&self, codec: &dyn PageCodec, row: u32) -> Result<String> {
        let loc = self.idlocs.get(row as usize).ok_or_else(|| {
            CoreError::MalformedPage(format!(
                "segment {} has no row {row} (row count {})",
                self.seg_id,
                self.idlocs.len()
            ))
        })?;
        let bytes = self
            .ids
            .read_range(codec, loc.off as usize, loc.len as usize)?;
        String::from_utf8(bytes).map_err(|e| {
            CoreError::MalformedPage(format!(
                "segment {} row {row} id is not valid UTF-8: {e}",
                self.seg_id
            ))
        })
    }

    /// The row carrying external id `id`, or `None` if absent. Binary-searches the
    /// `sorted_rows` forward index, decrypting each candidate row's id from `.ids`
    /// to compare (ADR-0072, decrypt-on-compare). Ignores tombstones — liveness is
    /// the caller's concern, as with the in-RAM primary index. Within a segment
    /// each id is unique, so the match (if any) is the single owning row.
    pub(crate) fn lookup(&self, codec: &dyn PageCodec, id: &str) -> Result<Option<u32>> {
        // Resident-range fast reject (ADR-0076): if the id sorts outside this
        // segment's [min, max], it cannot be here — skip the decrypting binary
        // search. Same lexicographic `str` ordering the search below uses.
        match &self.id_range {
            None => return Ok(None),
            Some((min, max)) if id < min.as_str() || id > max.as_str() => return Ok(None),
            Some(_) => {}
        }
        let mut lo = 0usize;
        let mut hi = self.sorted_rows.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = self.sorted_rows[mid];
            let candidate = self.read_id(codec, row)?;
            match candidate.as_str().cmp(id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(row)),
            }
        }
        Ok(None)
    }

    /// The rows matching a secondary-index predicate, or `None` if the field is
    /// not indexed in this segment.
    pub(crate) fn sec_query(&self, predicate: &SecPredicate) -> Result<Option<RoaringBitmap>> {
        self.sec.query(predicate)
    }

    /// Read row `row`'s raw little-endian vector bytes (`stride` bytes).
    pub(crate) fn read_vector(
        &self,
        codec: &dyn PageCodec,
        row: u32,
        stride: usize,
    ) -> Result<Vec<u8>> {
        self.vec.read_range(codec, row as usize * stride, stride)
    }

    /// Read row `row`'s opaque payload bytes.
    pub(crate) fn read_payload(&self, codec: &dyn PageCodec, row: u32) -> Result<Vec<u8>> {
        let loc = self.paylocs.get(row as usize).ok_or_else(|| {
            CoreError::MalformedPage(format!(
                "segment {} has no row {row} (row count {})",
                self.seg_id,
                self.paylocs.len()
            ))
        })?;
        self.pay
            .read_range(codec, loc.off as usize, loc.len as usize)
    }

    /// Number of rows physically stored in this segment (live and dead).
    pub(crate) fn row_count(&self) -> u32 {
        self.paylocs.len() as u32
    }

    /// Whether row `row` has been tombstoned.
    pub(crate) fn is_dead(&self, row: u32) -> bool {
        self.dead.contains(row)
    }

    /// The number of live (non-tombstoned) rows.
    pub(crate) fn live_count(&self) -> u64 {
        u64::from(self.row_count()) - self.dead.len()
    }

    /// Mark `rows` of this segment as dead, updating the in-memory bitmap. The
    /// caller persists the merged bitmap with [`write_del`].
    pub(crate) fn mark_dead(&mut self, rows: &RoaringBitmap) {
        self.dead |= rows;
    }

    /// A clone of the current tombstone bitmap, for persisting via [`write_del`].
    pub(crate) fn dead_bitmap(&self) -> RoaringBitmap {
        self.dead.clone()
    }
}

/// Row numbers ordered by their external id — the `sorted_rows` forward index
/// (ADR-0072). A stable sort of `0..ids.len()` keyed by `ids[row]`.
fn sorted_rows_by_id<S: AsRef<str>>(ids: &[S]) -> Vec<u32> {
    let mut order: Vec<u32> = (0..ids.len() as u32).collect();
    order.sort_by(|&a, &b| ids[a as usize].as_ref().cmp(ids[b as usize].as_ref()));
    order
}

/// Write a new sealed segment's `.vec`, `.pay`, `.ids`, `.dir`, and (if the
/// collection has `filterable` fields) `.sec` files into `seg_dir` and `fsync`
/// each. A new segment has no tombstones, so no `.del` is written.
///
/// `rows` are sealed in the given order (row `i` → `.vec` slot `i`). The files are
/// *not* directory-`fsync`'d here — the engine sequences directory `fsync`s
/// against the manifest swap.
pub(crate) fn write_segment(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
    rows: &[SealRow<'_>],
    filterable: &[FilterableField],
) -> Result<()> {
    let mut vec_blob = Vec::new();
    let mut pay_blob = Vec::new();
    let mut id_blob = Vec::new();
    let mut dir_rows = Vec::with_capacity(rows.len());
    for row in rows {
        vec_blob.extend_from_slice(row.vector);
        let pay_off = pay_blob.len() as u64;
        pay_blob.extend_from_slice(row.payload);
        let id_off = id_blob.len() as u64;
        id_blob.extend_from_slice(row.external_id.as_bytes());
        dir_rows.push(RowEntry {
            id_off,
            id_len: row.external_id.len() as u32,
            pay_off,
            pay_len: row.payload.len() as u32,
        });
    }
    let ids: Vec<&str> = rows.iter().map(|r| r.external_id).collect();
    let dir = SegmentDir {
        format_version: SEGMENT_FORMAT_VERSION,
        segment_id,
        rows: dir_rows,
        sorted_rows: sorted_rows_by_id(&ids),
    };
    let dir_blob = postcard::to_allocvec(&dir)?;

    write_blocks(
        &vec_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
        &vec_blob,
    )?;
    write_blocks(
        &pay_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
        &pay_blob,
    )?;
    write_blocks(
        &id_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
        &id_blob,
    )?;
    crate::paged::write_paged(
        &dir_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
        &dir_blob,
    )?;
    if !filterable.is_empty() {
        let payloads: Vec<&[u8]> = rows.iter().map(|r| r.payload).collect();
        let sec = SecIndex::build(filterable, &payloads)?;
        crate::paged::write_paged(
            &sec_path(seg_dir, segment_id),
            codec,
            PageType::Segment,
            segment_id,
            &sec.encode()?,
        )?;
    }
    Ok(())
}

/// Write a new sealed segment by **streaming** its rows, holding only one page of
/// each column in memory instead of the whole live set (ADR-0068). `next_row`
/// yields `(external_id, vector_bytes, payload_bytes)` in the final row order (row
/// `i` → `.vec` slot `i`) until it returns `None`; `row_count` is only a capacity
/// hint.
///
/// The `.vec`, `.pay`, and `.ids` columns are streamed page-by-page through a
/// [`BlockWriter`]; the row directory (O(rows) of offsets + the sorted-row index)
/// and, for a collection with filterable fields, the secondary index (which
/// `SecIndex::build` needs over all payloads) are still assembled in memory —
/// those are the small / opt-in costs, while the vector, payload, and id *bytes*
/// (the dominant term) never all reside at once. Produces the same on-disk files
/// as [`write_segment`]; not directory-`fsync`'d here.
pub(crate) fn write_segment_streaming(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
    row_count: usize,
    filterable: &[FilterableField],
    mut next_row: impl FnMut() -> Result<Option<(String, Vec<u8>, Vec<u8>)>>,
) -> Result<()> {
    let mut vec_w = BlockWriter::create(
        &vec_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
    )?;
    let mut pay_w = BlockWriter::create(
        &pay_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
    )?;
    let mut id_w = BlockWriter::create(
        &id_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
    )?;

    let mut dir_rows = Vec::with_capacity(row_count);
    // Ids are kept to build the sorted-row index at finish (O(rows), the same
    // bounded directory cost this streaming path already accepts).
    let mut ids: Vec<String> = Vec::with_capacity(row_count);
    // Only a collection with filterable fields builds a `.sec`, and only that path
    // holds the payloads (SecIndex::build needs them all); otherwise this stays empty.
    let want_sec = !filterable.is_empty();
    let mut sec_payloads: Vec<Vec<u8>> = if want_sec {
        Vec::with_capacity(row_count)
    } else {
        Vec::new()
    };

    let mut pay_off = 0u64;
    let mut id_off = 0u64;
    while let Some((external_id, vector, payload)) = next_row()? {
        vec_w.write(&vector)?;
        pay_w.write(&payload)?;
        id_w.write(external_id.as_bytes())?;
        dir_rows.push(RowEntry {
            id_off,
            id_len: external_id.len() as u32,
            pay_off,
            pay_len: payload.len() as u32,
        });
        pay_off += payload.len() as u64;
        id_off += external_id.len() as u64;
        ids.push(external_id);
        if want_sec {
            sec_payloads.push(payload);
        }
    }
    vec_w.finish()?;
    pay_w.finish()?;
    id_w.finish()?;

    let dir = SegmentDir {
        format_version: SEGMENT_FORMAT_VERSION,
        segment_id,
        rows: dir_rows,
        sorted_rows: sorted_rows_by_id(&ids),
    };
    crate::paged::write_paged(
        &dir_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
        &postcard::to_allocvec(&dir)?,
    )?;
    if want_sec {
        let payloads: Vec<&[u8]> = sec_payloads.iter().map(Vec::as_slice).collect();
        let sec = SecIndex::build(filterable, &payloads)?;
        crate::paged::write_paged(
            &sec_path(seg_dir, segment_id),
            codec,
            PageType::Segment,
            segment_id,
            &sec.encode()?,
        )?;
    }
    Ok(())
}

/// Atomically write a segment's tombstone bitmap to `seg-NNN.del`.
///
/// Unlike the immutable `.vec`/`.pay`/`.dir`, the `.del` is rewritten as rows
/// die, so it is written to a temp file and `rename`d into place — a crash leaves
/// the previous `.del` (or its absence) intact, never a torn bitmap. The segment
/// directory is `fsync`'d so the rename is durable.
pub(crate) fn write_del(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
    dead: &RoaringBitmap,
) -> Result<()> {
    let mut blob = Vec::with_capacity(dead.serialized_size());
    dead.serialize_into(&mut blob)?;
    let tmp = del_tmp_path(seg_dir, segment_id);
    crate::paged::write_paged(&tmp, codec, PageType::Segment, segment_id, &blob)?;
    let final_path = del_path(seg_dir, segment_id);
    std::fs::rename(&tmp, &final_path).map_err(|e| CoreError::io(&final_path, e))?;
    crate::paged::fsync_dir(seg_dir)?;
    Ok(())
}

/// Read a segment's tombstone bitmap, or an empty bitmap if no `.del` exists.
fn read_del(seg_dir: &Path, segment_id: u64, codec: &dyn PageCodec) -> Result<RoaringBitmap> {
    let path = del_path(seg_dir, segment_id);
    if !path.exists() {
        return Ok(RoaringBitmap::new());
    }
    let blob = crate::paged::read_paged(&path, codec, PageType::Segment)?;
    Ok(RoaringBitmap::deserialize_from(&blob[..])?)
}

/// Read a segment's secondary index, or an empty one if no `.sec` exists (the
/// collection has no filterable fields).
fn read_sec(seg_dir: &Path, segment_id: u64, codec: &dyn PageCodec) -> Result<SecIndex> {
    let path = sec_path(seg_dir, segment_id);
    if !path.exists() {
        return Ok(SecIndex::default());
    }
    let blob = crate::paged::read_paged(&path, codec, PageType::Segment)?;
    SecIndex::decode(&blob)
}

/// Open a sealed segment for reads. The returned handle memory-maps the columns
/// and holds only O(rows) of byte offsets, the sorted-row forward index, the
/// tombstone bitmap, and the secondary index — external ids are read on demand
/// from the `.ids` column (ADR-0072), not resident.
pub(crate) fn open_segment(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
) -> Result<SealedSegment> {
    let dir_blob =
        crate::paged::read_paged(&dir_path(seg_dir, segment_id), codec, PageType::Segment)?;
    let dir: SegmentDir = postcard::from_bytes(&dir_blob)?;
    if dir.format_version != SEGMENT_FORMAT_VERSION {
        return Err(CoreError::UnsupportedVersion {
            found: dir.format_version,
            supported: SEGMENT_FORMAT_VERSION,
        });
    }
    let vec = BlockFile::open(&vec_path(seg_dir, segment_id), codec, PageType::Segment)?;
    let pay = BlockFile::open(&pay_path(seg_dir, segment_id), codec, PageType::Segment)?;
    let ids = BlockFile::open(&id_path(seg_dir, segment_id), codec, PageType::Segment)?;
    let dead = read_del(seg_dir, segment_id, codec)?;
    let sec = read_sec(seg_dir, segment_id, codec)?;

    let mut idlocs = Vec::with_capacity(dir.rows.len());
    let mut paylocs = Vec::with_capacity(dir.rows.len());
    for r in &dir.rows {
        idlocs.push(ByteLoc {
            off: r.id_off,
            len: r.id_len,
        });
        paylocs.push(ByteLoc {
            off: r.pay_off,
            len: r.pay_len,
        });
    }
    // External ids stay on disk in the `.ids` column and are read on demand via
    // `read_id`/`lookup` — never materialized here (ADR-0072, Increment C).
    let mut seg = SealedSegment {
        seg_id: segment_id,
        vec,
        pay,
        ids,
        paylocs,
        idlocs,
        sorted_rows: dir.sorted_rows,
        id_range: None,
        dead,
        sec,
    };
    // Resident min/max range fingerprint (ADR-0076): decrypt just the two extreme
    // ids — `sorted_rows` is id-sorted, so its first/last rows are the min/max —
    // and hold them so `lookup` can range-reject without a per-comparison decrypt.
    if let (Some(&first), Some(&last)) = (seg.sorted_rows.first(), seg.sorted_rows.last()) {
        let min = seg.read_id(codec, first)?;
        let max = seg.read_id(codec, last)?;
        seg.id_range = Some((min, max));
    }
    Ok(seg)
}

/// Open just a sealed segment's immutable `.vec` column as a standalone `mmap`,
/// for a lock-free off-lock vector stream (ADR-0070). Cheaper than
/// [`open_segment`], which also loads the payload directory, tombstones, and
/// secondary index a pure vector scan never touches. Rows are read at
/// `row × stride` exactly as [`SealedSegment::read_vector`] does.
pub(crate) fn open_vec_column(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
) -> Result<BlockFile> {
    BlockFile::open(&vec_path(seg_dir, segment_id), codec, PageType::Segment)
}

/// File name of a segment's vector column.
fn vec_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.vec"))
}

/// File name of a segment's payload heap.
fn pay_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.pay"))
}

/// File name of a segment's id column (ADR-0072).
fn id_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.ids"))
}

/// File name of a segment's row directory.
fn dir_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.dir"))
}

/// File name of a segment's secondary index.
fn sec_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.sec"))
}

/// File name of a segment's tombstone bitmap.
fn del_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.del"))
}

/// Temp file name used while atomically rewriting a segment's tombstone bitmap.
fn del_tmp_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.del.tmp"))
}

/// Parse the segment id from any of a segment's companion file names
/// (`seg-NNNNNNNNNN.{vec,pay,dir,del}`), for garbage-collecting orphans.
pub(crate) fn seg_id_of_file(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("seg-")?;
    let dot = stem.find('.')?;
    stem[..dot].parse::<u64>().ok()
}

/// Whether a file name is a crash-leftover temp file that should always be
/// removed on recovery.
pub(crate) fn is_temp_file(name: &str) -> bool {
    name.ends_with(".tmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PlainCodec;

    fn rows() -> Vec<SealRow<'static>> {
        vec![
            SealRow {
                external_id: "a",
                vector: &[0, 1, 2, 3],
                payload: b"{}",
            },
            SealRow {
                external_id: "b",
                vector: &[4, 5, 6, 7],
                payload: b"[1,2,3]",
            },
        ]
    }

    #[test]
    fn segment_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let seg_dir = dir.path();
        write_segment(seg_dir, 1, &PlainCodec, &rows(), &[]).unwrap();
        let seg = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert_eq!(seg.read_id(&PlainCodec, 0).unwrap(), "a");
        assert_eq!(seg.read_id(&PlainCodec, 1).unwrap(), "b");
        assert_eq!(seg.row_count(), 2);
        assert_eq!(seg.live_count(), 2);
        assert!(!seg.is_dead(0));
        assert_eq!(
            seg.read_vector(&PlainCodec, 0, 4).unwrap(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            seg.read_vector(&PlainCodec, 1, 4).unwrap(),
            vec![4, 5, 6, 7]
        );
        assert_eq!(seg.read_payload(&PlainCodec, 0).unwrap(), b"{}");
        assert_eq!(seg.read_payload(&PlainCodec, 1).unwrap(), b"[1,2,3]");
        // No `.sec` is written when there are no filterable fields.
        assert!(!sec_path(seg_dir, 1).exists());
    }

    #[test]
    fn streaming_segment_matches_write_segment() {
        // The streamed writer must produce a segment that reads back identically to
        // the one `write_segment` builds from the same rows (ADR-0068).
        let whole = tempfile::tempdir().unwrap();
        write_segment(whole.path(), 1, &PlainCodec, &rows(), &[]).unwrap();

        let streamed = tempfile::tempdir().unwrap();
        let src = rows();
        let mut i = 0usize;
        write_segment_streaming(streamed.path(), 1, &PlainCodec, src.len(), &[], || {
            if i == src.len() {
                return Ok(None);
            }
            let r = &src[i];
            i += 1;
            Ok(Some((
                r.external_id.to_owned(),
                r.vector.to_vec(),
                r.payload.to_vec(),
            )))
        })
        .unwrap();

        for name in ["vec", "pay", "ids", "dir"] {
            let a = std::fs::read(whole.path().join(format!("seg-0000000001.{name}"))).unwrap();
            let b = std::fs::read(streamed.path().join(format!("seg-0000000001.{name}"))).unwrap();
            assert_eq!(a, b, "streamed .{name} differs from write_segment");
        }
    }

    #[test]
    fn id_column_lookup_finds_every_row_and_rejects_absent() {
        // ADR-0072: ids live in the on-demand `.ids` column and forward lookup
        // binary-searches the sorted-row index. Rows are written in an order that
        // is *not* id-sorted, so a passing lookup proves the sorted index is real.
        let dir = tempfile::tempdir().unwrap();
        let ids = ["m", "a", "z", "d", "b"]; // deliberately unsorted
        let rows: Vec<SealRow> = ids
            .iter()
            .map(|id| SealRow {
                external_id: id,
                vector: &[0, 0, 0, 0],
                payload: b"{}",
            })
            .collect();
        write_segment(dir.path(), 1, &PlainCodec, &rows, &[]).unwrap();
        let seg = open_segment(dir.path(), 1, &PlainCodec).unwrap();

        for (row, id) in ids.iter().enumerate() {
            assert_eq!(seg.lookup(&PlainCodec, id).unwrap(), Some(row as u32));
            assert_eq!(&seg.read_id(&PlainCodec, row as u32).unwrap(), id);
        }
        assert_eq!(seg.lookup(&PlainCodec, "absent").unwrap(), None);
        // Bracketing ids that sort outside the present range still miss.
        assert_eq!(seg.lookup(&PlainCodec, "").unwrap(), None);
        assert_eq!(seg.lookup(&PlainCodec, "zzzz").unwrap(), None);
    }

    #[test]
    fn resident_id_range_is_the_true_min_and_max() {
        // ADR-0076: the resident fingerprint is the id-sorted min/max (from
        // `sorted_rows`), not the write-order first/last, so range-rejection is
        // sound regardless of the order rows were sealed in.
        let dir = tempfile::tempdir().unwrap();
        let ids = ["m", "a", "z", "d", "b"]; // min "a", max "z"; write order differs
        let rows: Vec<SealRow> = ids
            .iter()
            .map(|id| SealRow {
                external_id: id,
                vector: &[0, 0, 0, 0],
                payload: b"{}",
            })
            .collect();
        write_segment(dir.path(), 1, &PlainCodec, &rows, &[]).unwrap();
        let seg = open_segment(dir.path(), 1, &PlainCodec).unwrap();
        assert_eq!(seg.id_range, Some(("a".to_owned(), "z".to_owned())));

        // An empty segment carries no range and rejects every lookup.
        let empty = tempfile::tempdir().unwrap();
        write_segment(empty.path(), 2, &PlainCodec, &[], &[]).unwrap();
        let seg = open_segment(empty.path(), 2, &PlainCodec).unwrap();
        assert_eq!(seg.id_range, None);
        assert_eq!(seg.lookup(&PlainCodec, "a").unwrap(), None);
    }

    #[test]
    fn streaming_segment_consumes_rows_lazily_without_collecting_them() {
        // The writer pulls one row at a time from a generator that never
        // materializes the whole set — evidence that compacting a large collection
        // does not require all its vectors/payloads resident at once (ADR-0068).
        let dir = tempfile::tempdir().unwrap();
        let n = 20_000u32;
        let mut produced = 0u32;
        write_segment_streaming(dir.path(), 1, &PlainCodec, n as usize, &[], || {
            if produced == n {
                return Ok(None);
            }
            let i = produced;
            produced += 1;
            // Each row is generated on demand; no Vec of all rows ever exists.
            Ok(Some((
                format!("k{i}"),
                i.to_le_bytes().to_vec(),
                b"{}".to_vec(),
            )))
        })
        .unwrap();

        let seg = open_segment(dir.path(), 1, &PlainCodec).unwrap();
        assert_eq!(seg.row_count(), n);
        assert_eq!(
            seg.read_vector(&PlainCodec, 0, 4).unwrap(),
            0u32.to_le_bytes()
        );
        assert_eq!(
            seg.read_vector(&PlainCodec, n - 1, 4).unwrap(),
            (n - 1).to_le_bytes()
        );
        assert_eq!(seg.read_id(&PlainCodec, 12_345).unwrap(), "k12345");
    }

    #[test]
    fn tombstone_bitmap_roundtrips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let seg_dir = dir.path();
        write_segment(seg_dir, 1, &PlainCodec, &rows(), &[]).unwrap();
        // Absent .del => no dead rows.
        let seg = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert_eq!(seg.live_count(), 2);

        // Persist a tombstone for row 0, then reopen.
        let mut dead = RoaringBitmap::new();
        dead.insert(0);
        write_del(seg_dir, 1, &PlainCodec, &dead).unwrap();
        assert!(
            !del_tmp_path(seg_dir, 1).exists(),
            "temp must be renamed away"
        );

        let seg = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert!(seg.is_dead(0));
        assert!(!seg.is_dead(1));
        assert_eq!(seg.live_count(), 1);
    }

    #[test]
    fn secondary_index_is_written_and_queryable() {
        use crate::descriptor::FilterableField;
        let dir = tempfile::tempdir().unwrap();
        let seg_dir = dir.path();
        let rows = vec![
            SealRow {
                external_id: "a",
                vector: &[0, 0, 0, 0],
                payload: br#"{"city":"paris"}"#,
            },
            SealRow {
                external_id: "b",
                vector: &[0, 0, 0, 0],
                payload: br#"{"city":"lyon"}"#,
            },
        ];
        let filterable = [FilterableField::keyword("city")];
        write_segment(seg_dir, 2, &PlainCodec, &rows, &filterable).unwrap();
        assert!(sec_path(seg_dir, 2).exists(), ".sec must be written");
        let seg = open_segment(seg_dir, 2, &PlainCodec).unwrap();
        let hit = seg
            .sec_query(&SecPredicate::Eq {
                field: "city".into(),
                value: crate::sec::SecValue::Keyword("paris".into()),
            })
            .unwrap()
            .unwrap();
        assert_eq!(hit.iter().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn empty_segment_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 5, &PlainCodec, &[], &[]).unwrap();
        let seg = open_segment(dir.path(), 5, &PlainCodec).unwrap();
        assert_eq!(seg.row_count(), 0);
        assert_eq!(seg.row_count(), 0);
        assert!(seg.read_payload(&PlainCodec, 0).is_err());
    }

    #[test]
    fn seg_id_parses_from_any_companion() {
        assert_eq!(seg_id_of_file("seg-0000000007.vec"), Some(7));
        assert_eq!(seg_id_of_file("seg-0000000042.pay"), Some(42));
        assert_eq!(seg_id_of_file("seg-0000000003.dir"), Some(3));
        assert_eq!(seg_id_of_file("seg-0000000009.del"), Some(9));
        assert_eq!(seg_id_of_file("seg-0000000005.sec"), Some(5));
        assert_eq!(seg_id_of_file("CURRENT"), None);
        assert_eq!(seg_id_of_file("seg-bogus.vec"), None);
        assert!(is_temp_file("seg-0000000001.del.tmp"));
        assert!(!is_temp_file("seg-0000000001.del"));
    }
}
