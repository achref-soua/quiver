// SPDX-License-Identifier: AGPL-3.0-only
//! Sealed, immutable segments: the on-disk home of checkpointed rows.
//!
//! Phase 1 uses **snapshot-delta** segments: each checkpoint seals the rows
//! upserted (and the ids deleted) since the previous checkpoint into one new
//! immutable segment file, appended to the collection's segment list. Recovery
//! replays segments oldest-to-newest — later rows shadow earlier ones for the
//! same id, and a segment's recorded tombstones remove ids — then applies the
//! WAL tail on top.
//!
//! The richer columnar layout in `docs/storage/on-disk-format.md` — a stride
//! addressed, `mmap`-read `.vec` column with a paged payload heap and roaring
//! tombstones — is a Phase 2 concern, where it pairs with the disk-resident
//! index and the memory-frugality work. Phase 1 keeps live rows resident (the
//! in-memory HNSW needs the vectors anyway) and persists them through the same
//! CRC'd, codec-sealed pages as the manifest, so durability and
//! encryption-at-rest already apply.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{CoreError, Result};
use crate::page::{PageCodec, PageType};

/// Current segment-data schema version.
pub(crate) const SEGMENT_FORMAT_VERSION: u16 = 1;

/// One row stored in a segment: an external id, its raw little-endian vector
/// bytes, and its opaque payload bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentRow {
    pub external_id: String,
    pub vector: Vec<u8>,
    pub payload: Vec<u8>,
}

/// The serialized contents of one segment file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentData {
    /// Schema version of this segment file.
    pub format_version: u16,
    /// Id of the segment (matches its [`crate::manifest::SegmentRef`]).
    pub segment_id: u64,
    /// Rows upserted in the checkpoint window this segment captures.
    pub rows: Vec<SegmentRow>,
    /// Ids deleted in the window, removing rows from older segments on replay.
    pub tombstones: Vec<String>,
}

/// Write a segment to `path` as a CRC'd, codec-sealed paged file and `fsync` it.
pub(crate) fn write_segment(path: &Path, codec: &dyn PageCodec, data: &SegmentData) -> Result<()> {
    let body = postcard::to_allocvec(data)?;
    crate::paged::write_paged(path, codec, PageType::Segment, data.segment_id, &body)
}

/// Read and verify a segment written by [`write_segment`].
pub(crate) fn read_segment(path: &Path, codec: &dyn PageCodec) -> Result<SegmentData> {
    let body = crate::paged::read_paged(path, codec, PageType::Segment)?;
    let data: SegmentData = postcard::from_bytes(&body)?;
    if data.format_version != SEGMENT_FORMAT_VERSION {
        return Err(CoreError::UnsupportedVersion {
            found: data.format_version,
            supported: SEGMENT_FORMAT_VERSION,
        });
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PlainCodec;

    #[test]
    fn segment_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-0000000001.seg");
        let data = SegmentData {
            format_version: SEGMENT_FORMAT_VERSION,
            segment_id: 1,
            rows: vec![
                SegmentRow {
                    external_id: "a".into(),
                    vector: vec![0, 1, 2, 3],
                    payload: b"{}".to_vec(),
                },
                SegmentRow {
                    external_id: "b".into(),
                    vector: vec![4, 5, 6, 7],
                    payload: b"[]".to_vec(),
                },
            ],
            tombstones: vec!["old".into()],
        };
        write_segment(&path, &PlainCodec, &data).unwrap();
        let back = read_segment(&path, &PlainCodec).unwrap();
        assert_eq!(back, data);
    }
}
