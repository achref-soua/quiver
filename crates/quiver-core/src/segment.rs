// SPDX-License-Identifier: AGPL-3.0-only
//! Sealed, immutable segments in the row-addressed on-disk format (ADR-0004).
//!
//! Each checkpoint seals the rows changed since the previous checkpoint into a
//! new immutable segment, written as three companion files named by a monotonic
//! segment id:
//!
//! - `seg-NNNNNNNNNN.vec` — the **vector column**: each live row's raw
//!   little-endian vector bytes, packed tightly at `row × stride`, read through
//!   an `mmap` ([`crate::blockfile`]). O(1) random access, cache-friendly scans.
//! - `seg-NNNNNNNNNN.pay` — the **payload heap**: each row's opaque payload bytes
//!   concatenated, also `mmap`-read.
//! - `seg-NNNNNNNNNN.dir` — the **row directory** ([`SegmentDir`]): per row, the
//!   external id and the payload's `(offset, length)` in the heap, plus the ids
//!   **tombstoned** in this checkpoint window. Serialized as a paged `postcard`
//!   blob ([`crate::paged`]), so it inherits per-page CRC integrity.
//!
//! Vectors and payloads therefore live on disk and are decrypted on demand; only
//! the row directory (external ids + payload offsets) is read into RAM, where it
//! seeds the engine's primary index. Recovery still replays segments
//! oldest-to-newest — a later row shadows an earlier one for the same id, and a
//! segment's tombstones remove ids — then applies the WAL tail on top.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::blockfile::{BlockFile, write_blocks};
use crate::error::{CoreError, Result};
use crate::page::{PageCodec, PageType};

/// Current row-directory schema version. (v1 was the Phase-1 snapshot-delta
/// `postcard` blob; v2 is this row-addressed layout.)
pub(crate) const SEGMENT_FORMAT_VERSION: u16 = 2;

/// One live row's entry in the segment directory. The row's vector lives at
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

/// The `.dir` file: the row directory plus the ids tombstoned in this window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentDir {
    /// Schema version of this segment's files.
    pub format_version: u16,
    /// Segment id (matches its [`crate::manifest::SegmentRef`] and file names).
    pub segment_id: u64,
    /// Live rows sealed into this segment, in `.vec`/`.pay` row order.
    pub rows: Vec<RowEntry>,
    /// Ids deleted in this window, removing rows from older segments on replay.
    pub tombstones: Vec<String>,
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
/// payload heap, plus the row → payload-location directory.
pub(crate) struct SealedSegment {
    /// Segment id; names the files and matches the manifest.
    pub seg_id: u64,
    vec: BlockFile,
    pay: BlockFile,
    paylocs: Vec<PayLoc>,
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
}

/// Write a new sealed segment's three files into `seg_dir` and `fsync` each.
///
/// `rows` are sealed in the given order (row `i` → `.vec` slot `i`); `tombstones`
/// records ids deleted in this window. The directory is *not* `fsync`'d here —
/// the engine sequences directory `fsync`s against the manifest swap.
pub(crate) fn write_segment(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
    rows: &[SealRow<'_>],
    tombstones: &[String],
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
        tombstones: tombstones.to_vec(),
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

/// Open a sealed segment for reads, returning the `mmap`-backed handle together
/// with the external ids (in row order) and the ids it tombstones — the engine
/// uses those two lists to (re)build the primary index.
pub(crate) fn open_segment(
    seg_dir: &Path,
    segment_id: u64,
    codec: &dyn PageCodec,
) -> Result<(SealedSegment, Vec<String>, Vec<String>)> {
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
        },
        ext_ids,
        dir.tombstones,
    ))
}

/// File name of a segment's vector column.
fn vec_path(seg_dir: &Path, seg_id: u64) -> std::path::PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.vec"))
}

/// File name of a segment's payload heap.
fn pay_path(seg_dir: &Path, seg_id: u64) -> std::path::PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.pay"))
}

/// File name of a segment's row directory.
fn dir_path(seg_dir: &Path, seg_id: u64) -> std::path::PathBuf {
    seg_dir.join(format!("seg-{seg_id:010}.dir"))
}

/// Parse the segment id from any of a segment's companion file names
/// (`seg-NNNNNNNNNN.{vec,pay,dir}`), for garbage-collecting orphans.
pub(crate) fn seg_id_of_file(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("seg-")?;
    let dot = stem.find('.')?;
    stem[..dot].parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PlainCodec;

    #[test]
    fn segment_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let seg_dir = dir.path();
        let stride = 4; // 1-dim f32 rows for the test
        let rows = vec![
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
        ];
        write_segment(seg_dir, 1, &PlainCodec, &rows, &["old".to_owned()]).unwrap();
        let (seg, ext_ids, tombstones) = open_segment(seg_dir, 1, &PlainCodec).unwrap();
        assert_eq!(ext_ids, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(tombstones, vec!["old".to_owned()]);
        assert_eq!(
            seg.read_vector(&PlainCodec, 0, stride).unwrap(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            seg.read_vector(&PlainCodec, 1, stride).unwrap(),
            vec![4, 5, 6, 7]
        );
        assert_eq!(seg.read_payload(&PlainCodec, 0).unwrap(), b"{}");
        assert_eq!(seg.read_payload(&PlainCodec, 1).unwrap(), b"[1,2,3]");
    }

    #[test]
    fn tombstone_only_segment_has_empty_columns() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 5, &PlainCodec, &[], &["gone".to_owned()]).unwrap();
        let (seg, ext_ids, tombstones) = open_segment(dir.path(), 5, &PlainCodec).unwrap();
        assert!(ext_ids.is_empty());
        assert_eq!(tombstones, vec!["gone".to_owned()]);
        // A tombstone-only segment has empty columns; reading any row errors.
        assert!(seg.read_payload(&PlainCodec, 0).is_err());
    }

    #[test]
    fn empty_payloads_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            SealRow {
                external_id: "a",
                vector: &[1, 2, 3, 4],
                payload: b"",
            },
            SealRow {
                external_id: "b",
                vector: &[5, 6, 7, 8],
                payload: b"",
            },
        ];
        write_segment(dir.path(), 2, &PlainCodec, &rows, &[]).unwrap();
        let (seg, _, _) = open_segment(dir.path(), 2, &PlainCodec).unwrap();
        assert_eq!(seg.read_payload(&PlainCodec, 0).unwrap(), Vec::<u8>::new());
        assert_eq!(
            seg.read_vector(&PlainCodec, 1, 4).unwrap(),
            vec![5, 6, 7, 8]
        );
    }

    #[test]
    fn seg_id_parses_from_any_companion() {
        assert_eq!(seg_id_of_file("seg-0000000007.vec"), Some(7));
        assert_eq!(seg_id_of_file("seg-0000000042.pay"), Some(42));
        assert_eq!(seg_id_of_file("seg-0000000003.dir"), Some(3));
        assert_eq!(seg_id_of_file("CURRENT"), None);
        assert_eq!(seg_id_of_file("seg-bogus.vec"), None);
    }
}
