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
//! - `seg-NNNNNNNNNN.dir` — the **row directory** ([`SegmentDir`]): per row, the
//!   external id and the payload's `(offset, length)` in the heap. A paged
//!   `postcard` blob ([`crate::paged`]) with per-page CRC integrity.
//! - `seg-NNNNNNNNNN.del` — the **tombstone bitmap**: a `roaring` bitmap of this
//!   segment's row indices that are no longer live (deleted, or shadowed by a
//!   newer upsert). Written atomically (temp + rename) since, unlike the other
//!   three files, it is rewritten as rows die; absent means no dead rows.
//!
//! Vectors and payloads live on disk and are decrypted on demand; only the row
//! directory (external ids + payload offsets) and the tombstone bitmap are read
//! into RAM, where they seed the engine's primary index. On recovery, a row that
//! is tombstoned in its segment is skipped, so each external id is live in at
//! most one segment; the WAL tail is then applied on top.

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::blockfile::{BlockFile, write_blocks};
use crate::error::{CoreError, Result};
use crate::page::{PageCodec, PageType};

/// Current segment schema version. (v1 was the Phase-1 snapshot-delta `postcard`
/// blob; v2 the row-addressed layout with string tombstones; v3 moves tombstones
/// out of the directory into the roaring `.del` bitmap.)
pub(crate) const SEGMENT_FORMAT_VERSION: u16 = 3;

/// One row's entry in the segment directory. The row's vector lives at
/// `row_index × stride` in the `.vec` column; its payload at `(pay_off, pay_len)`
/// in the `.pay` heap. `row_index` is the entry's position in [`SegmentDir::rows`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RowEntry {
    /// Caller-supplied external id.
    pub external_id: String,
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
    /// Rows sealed into this segment, in `.vec`/`.pay` row order.
    pub rows: Vec<RowEntry>,
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

/// The payload location of one row within the `.pay` heap (the RAM-resident part
/// of the directory; external ids are consumed into the engine's primary index).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PayLoc {
    off: u64,
    len: u32,
}

/// A sealed segment opened for reads: `mmap` handles for the vector column and
/// payload heap, the row → payload-location directory, and the tombstone bitmap.
pub(crate) struct SealedSegment {
    /// Segment id; names the files and matches the manifest.
    pub seg_id: u64,
    vec: BlockFile,
    pay: BlockFile,
    paylocs: Vec<PayLoc>,
    // Rows of this segment that are no longer live.
    dead: RoaringBitmap,
}

impl SealedSegment {
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

/// Write a new sealed segment's `.vec`, `.pay`, and `.dir` files into `seg_dir`
/// and `fsync` each. A new segment has no tombstones, so no `.del` is written.
///
/// `rows` are sealed in the given order (row `i` → `.vec` slot `i`). The directory
/// is *not* `fsync`'d here — the engine sequences directory `fsync`s against the
/// manifest swap.
pub(crate) fn write_segment(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
    rows: &[SealRow<'_>],
) -> Result<()> {
    let mut vec_blob = Vec::new();
    let mut pay_blob = Vec::new();
    let mut dir_rows = Vec::with_capacity(rows.len());
    for row in rows {
        vec_blob.extend_from_slice(row.vector);
        let off = pay_blob.len() as u64;
        pay_blob.extend_from_slice(row.payload);
        dir_rows.push(RowEntry {
            external_id: row.external_id.to_owned(),
            pay_off: off,
            pay_len: row.payload.len() as u32,
        });
    }
    let dir = SegmentDir {
        format_version: SEGMENT_FORMAT_VERSION,
        segment_id,
        rows: dir_rows,
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
    crate::paged::write_paged(
        &dir_path(seg_dir, segment_id),
        codec,
        PageType::Segment,
        segment_id,
        &dir_blob,
    )?;
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

/// Open a sealed segment for reads, returning the `mmap`-backed handle together
/// with the external ids in row order — the engine uses them, minus the segment's
/// dead rows, to (re)build the primary index.
pub(crate) fn open_segment(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
) -> Result<(SealedSegment, Vec<String>)> {
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
    let dead = read_del(seg_dir, segment_id, codec)?;

    let mut ext_ids = Vec::with_capacity(dir.rows.len());
    let mut paylocs = Vec::with_capacity(dir.rows.len());
    for r in dir.rows {
        ext_ids.push(r.external_id);
        paylocs.push(PayLoc {
            off: r.pay_off,
            len: r.pay_len,
        });
    }
    Ok((
        SealedSegment {
            seg_id: segment_id,
            vec,
            pay,
            paylocs,
            dead,
        },
        ext_ids,
    ))
}

/// File name of a segment's vector column.
fn vec_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.vec"))
}

/// File name of a segment's payload heap.
fn pay_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.pay"))
}

/// File name of a segment's row directory.
fn dir_path(seg_dir: &Path, seg_id: u64) -> PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.dir"))
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
        write_segment(seg_dir, 1, &PlainCodec, &rows()).unwrap();
        let (seg, ext_ids) = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert_eq!(ext_ids, vec!["a".to_owned(), "b".to_owned()]);
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
    }

    #[test]
    fn tombstone_bitmap_roundtrips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let seg_dir = dir.path();
        write_segment(seg_dir, 1, &PlainCodec, &rows()).unwrap();
        // Absent .del => no dead rows.
        let (seg, _) = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert_eq!(seg.live_count(), 2);

        // Persist a tombstone for row 0, then reopen.
        let mut dead = RoaringBitmap::new();
        dead.insert(0);
        write_del(seg_dir, 1, &PlainCodec, &dead).unwrap();
        assert!(
            !del_tmp_path(seg_dir, 1).exists(),
            "temp must be renamed away"
        );

        let (seg, _) = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert!(seg.is_dead(0));
        assert!(!seg.is_dead(1));
        assert_eq!(seg.live_count(), 1);
    }

    #[test]
    fn empty_segment_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 5, &PlainCodec, &[]).unwrap();
        let (seg, ext_ids) = open_segment(dir.path(), 5, &PlainCodec).unwrap();
        assert!(ext_ids.is_empty());
        assert_eq!(seg.row_count(), 0);
        assert!(seg.read_payload(&PlainCodec, 0).is_err());
    }

    #[test]
    fn seg_id_parses_from_any_companion() {
        assert_eq!(seg_id_of_file("seg-0000000007.vec"), Some(7));
        assert_eq!(seg_id_of_file("seg-0000000042.pay"), Some(42));
        assert_eq!(seg_id_of_file("seg-0000000003.dir"), Some(3));
        assert_eq!(seg_id_of_file("seg-0000000009.del"), Some(9));
        assert_eq!(seg_id_of_file("CURRENT"), None);
        assert_eq!(seg_id_of_file("seg-bogus.vec"), None);
        assert!(is_temp_file("seg-0000000001.del.tmp"));
        assert!(!is_temp_file("seg-0000000001.del"));
    }
}
