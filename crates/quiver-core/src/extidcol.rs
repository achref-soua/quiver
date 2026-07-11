// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk external-id column for the embedded index's base id map (ADR-0073).
//!
//! A collection-wide analogue of the segment `.ids` column (ADR-0072): the
//! embedded ANN index's base internal-id → external-id map, moved off the heap
//! into a codec-sealed, `mmap`-ed, on-demand-paged column plus a compact resident
//! forward index for the reverse (ext → internal) lookup. Written once per index
//! rebuild by `quiver-embed`; the small post-rebuild tail stays resident there.
//!
//! The id *strings* — the dominant term at scale (~90–110 B/pt held twice across
//! the old `Vec<String>` + `HashMap<String,u64>`) — live on disk and are decrypted
//! one page at a time on demand. Only fixed-width metadata is resident: the
//! `offsets` table (`u64/pt`) and the `sorted` forward index (`u32/pt`), ~12 B/pt.
//!
//! Logical layout of the single `BlockFile`:
//! ```text
//! [ n: u64 LE | offsets: (n+1) × u64 LE | sorted: n × u32 LE | id bytes … ]
//! ```
//! `offsets` are relative to the start of the id-bytes region: internal id `i`
//! occupies bytes `[offsets[i], offsets[i+1])`. `sorted` lists the internal ids in
//! ascending order of their external id (a stable sort), binary-searched by
//! decrypting each candidate's id to compare — decrypt-on-compare, exactly as
//! the segment's `lookup`.

use std::path::Path;

use crate::blockfile::{BlockFile, BlockWriter};
use crate::error::{CoreError, Result};
use crate::page::{PageCodec, PageType};

// The id bytes and the metadata share one block file whose pages carry the segment
// page type (they are the same kind of codec-sealed column). The stamp is unused
// here (no WAL ordering), so it is 0.
const PAGE_TYPE: PageType = PageType::Segment;
const STAMP: u64 = 0;

/// A read-only, on-demand id column: `internal → ext` by page decrypt, and
/// `ext → internal` by binary search over a resident forward index. Owns its own
/// [`PageCodec`] clone (like the DiskVamana base), so reads need no external codec.
pub struct ExtIdColumn {
    file: BlockFile,
    codec: Box<dyn PageCodec>,
    // n+1 cumulative id-byte offsets, relative to `ids_base`.
    offsets: Vec<u64>,
    // Internal ids ordered by their external id (the forward index).
    sorted: Vec<u32>,
    // Logical offset in the block file where the id bytes begin.
    ids_base: usize,
}

impl ExtIdColumn {
    /// Seal `ids` (in internal-id order — `ids[i]` is the ext id of internal `i`)
    /// into a new column at `path`, encrypted by `codec`. Streams through a
    /// `BlockWriter` so only one page — not a second copy of the corpus — is
    /// resident at a time. Truncates any existing file at `path`.
    pub fn write(path: &Path, codec: &dyn PageCodec, ids: &[String]) -> Result<()> {
        let n = ids.len();
        let mut w = BlockWriter::create(path, codec, PAGE_TYPE, STAMP)?;
        w.write(&(n as u64).to_le_bytes())?;
        // offsets[0..=n], cumulative id-byte lengths.
        w.write(&0u64.to_le_bytes())?;
        let mut cum = 0u64;
        for id in ids {
            cum += id.len() as u64;
            w.write(&cum.to_le_bytes())?;
        }
        // sorted: internal ids by ascending ext id (stable).
        for row in sorted_by_id(ids) {
            w.write(&row.to_le_bytes())?;
        }
        // id bytes, packed in internal-id order.
        for id in ids {
            w.write(id.as_bytes())?;
        }
        w.finish()
    }

    /// Open the column at `path`, taking ownership of `codec`. Reads the fixed-width
    /// `offsets` + `sorted` metadata into RAM (the resident working set) and leaves
    /// the id bytes on disk for on-demand reads.
    pub fn open(path: &Path, codec: Box<dyn PageCodec>) -> Result<Self> {
        let file = BlockFile::open(path, codec.as_ref(), PAGE_TYPE)?;
        let n = {
            let b = file.read_range(codec.as_ref(), 0, 8)?;
            u64::from_le_bytes(b.as_slice().try_into().map_err(|_| corrupt("header"))?)
        } as usize;
        let off_bytes = file.read_range(codec.as_ref(), 8, (n + 1) * 8)?;
        let mut offsets = Vec::with_capacity(n + 1);
        for c in off_bytes.chunks_exact(8) {
            let a: [u8; 8] = c.try_into().map_err(|_| corrupt("offset table"))?;
            offsets.push(u64::from_le_bytes(a));
        }
        let sorted_off = 8 + (n + 1) * 8;
        let sorted_bytes = file.read_range(codec.as_ref(), sorted_off, n * 4)?;
        let mut sorted = Vec::with_capacity(n);
        for c in sorted_bytes.chunks_exact(4) {
            let a: [u8; 4] = c.try_into().map_err(|_| corrupt("forward index"))?;
            sorted.push(u32::from_le_bytes(a));
        }
        Ok(Self {
            file,
            codec,
            offsets,
            sorted,
            ids_base: sorted_off + n * 4,
        })
    }

    /// Number of ids in the column.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.sorted.len() as u64
    }

    /// Whether the column is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// The external id of internal id `internal`, decrypting the page(s) it spans.
    pub fn read(&self, internal: u64) -> Result<String> {
        let i = internal as usize;
        if i + 1 >= self.offsets.len() {
            return Err(corrupt(&format!(
                "internal id {internal} out of range (len {})",
                self.len()
            )));
        }
        let off = self.ids_base + self.offsets[i] as usize;
        let len = (self.offsets[i + 1] - self.offsets[i]) as usize;
        let bytes = self.file.read_range(self.codec.as_ref(), off, len)?;
        String::from_utf8(bytes).map_err(|_| corrupt("id bytes are not valid utf-8"))
    }

    /// The internal id of external id `ext`, or `None` if absent. Binary-searches
    /// the resident forward index, decrypting each candidate's id to compare.
    pub fn lookup(&self, ext: &str) -> Result<Option<u64>> {
        let mut lo: i64 = 0;
        let mut hi: i64 = self.sorted.len() as i64 - 1;
        while lo <= hi {
            let mid = (lo + hi) / 2;
            let row = self.sorted[mid as usize];
            match self.read(u64::from(row))?.as_str().cmp(ext) {
                std::cmp::Ordering::Equal => return Ok(Some(u64::from(row))),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid - 1,
            }
        }
        Ok(None)
    }
}

// Internal ids `0..ids.len()` ordered by their external id — a stable sort keyed by
// `ids[row]` (mirrors `segment::sorted_rows_by_id`).
fn sorted_by_id(ids: &[String]) -> Vec<u32> {
    let mut order: Vec<u32> = (0..ids.len() as u32).collect();
    order.sort_by(|&a, &b| ids[a as usize].cmp(&ids[b as usize]));
    order
}

fn corrupt(what: &str) -> CoreError {
    CoreError::MalformedPage(format!("ext-id column: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PlainCodec;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn round_trips_read_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idmap.qic");
        // Deliberately unsorted, mixed-length ids including a multi-byte one.
        let ids = v(&["banana", "apple", "cherry", "d", "élan"]);
        ExtIdColumn::write(&path, &PlainCodec, &ids).unwrap();
        let col = ExtIdColumn::open(&path, Box::new(PlainCodec)).unwrap();

        assert_eq!(col.len(), 5);
        assert!(!col.is_empty());
        // read: internal id -> ext, in original (internal) order.
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(&col.read(i as u64).unwrap(), id);
        }
        // lookup: ext -> internal, found and not-found.
        assert_eq!(col.lookup("banana").unwrap(), Some(0));
        assert_eq!(col.lookup("apple").unwrap(), Some(1));
        assert_eq!(col.lookup("élan").unwrap(), Some(4));
        assert_eq!(col.lookup("missing").unwrap(), None);
        assert_eq!(col.lookup("").unwrap(), None);
        // out-of-range read errors rather than panicking.
        assert!(col.read(5).is_err());
    }

    #[test]
    fn empty_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idmap.qic");
        ExtIdColumn::write(&path, &PlainCodec, &[]).unwrap();
        let col = ExtIdColumn::open(&path, Box::new(PlainCodec)).unwrap();
        assert_eq!(col.len(), 0);
        assert!(col.is_empty());
        assert_eq!(col.lookup("anything").unwrap(), None);
        assert!(col.read(0).is_err());
    }

    #[test]
    fn many_ids_spanning_pages() {
        // Enough ids that the column spans many pages, exercising read_range's
        // page-straddling path for both metadata and id bytes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idmap.qic");
        let ids: Vec<String> = (0..5000).map(|i| format!("id-{i:07}")).collect();
        ExtIdColumn::write(&path, &PlainCodec, &ids).unwrap();
        let col = ExtIdColumn::open(&path, Box::new(PlainCodec)).unwrap();
        assert_eq!(col.len(), 5000);
        assert_eq!(col.read(0).unwrap(), "id-0000000");
        assert_eq!(col.read(4999).unwrap(), "id-0004999");
        assert_eq!(col.lookup("id-0002500").unwrap(), Some(2500));
        assert_eq!(col.lookup("id-9999999").unwrap(), None);
    }
}
