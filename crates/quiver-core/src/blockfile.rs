// SPDX-License-Identifier: AGPL-3.0-only
//! Block files: a logical byte blob laid out across codec-sealed 16 KiB pages,
//! with random sub-range access through an `mmap`.
//!
//! A block file is a flat sequence of [`crate::page`] pages — each sealed by a
//! [`PageCodec`] and CRC-checked. Its *logical content* is the concatenation of
//! the live body bytes of every page, packed tightly so a record may straddle a
//! page boundary without per-record overhead (ADR-0004). Every page but the last
//! is full ([`PAGE_BODY_CAP`] bytes), which makes the map from a logical offset
//! to a `(page, intra-page offset)` pair exact and O(1).
//!
//! This is the substrate for the stride-addressed vector column (`seg-*.vec`)
//! and the payload heap (`seg-*.pay`): the writer packs rows tightly, and the
//! reader `mmap`s the file and decrypts only the pages a query touches, so only
//! the working set is resident — the memory-frugality goal of the storage
//! engine. Integrity is end-to-end: a corrupt or tampered page fails its CRC (or
//! AEAD tag) and the read errors rather than serving bad bytes.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;

use crate::error::{CoreError, Result};
use crate::page::{PAGE_BODY_CAP, PAGE_SIZE, PageCodec, PageType, build_page, parse_page};

/// A streaming writer for the block-file format: it seals and appends each page as
/// soon as its body fills, holding one page in memory instead of the whole blob.
/// Unlike [`crate::paged`], the format carries no length prefix — logical content
/// is just the concatenated page bodies — so the total need not be known up front,
/// and an empty blob writes a zero-page file. The page boundaries are identical to
/// [`write_blocks`]'s regardless of how the body is chunked across
/// [`BlockWriter::write`] calls, so the on-disk bytes are unchanged. Used by the
/// streaming compaction path to bound memory to one page per column (ADR-0068).
pub(crate) struct BlockWriter<'a> {
    file: BufWriter<File>,
    path: std::path::PathBuf,
    codec: &'a dyn PageCodec,
    page_type: PageType,
    stamp: u64,
    page_id: u64,
    // The page currently being filled; flushed once it reaches `PAGE_BODY_CAP`.
    body: Vec<u8>,
    // Reused seal-output buffer (`codec.block_size()` bytes).
    block: Vec<u8>,
}

impl<'a> BlockWriter<'a> {
    /// Create a block file at `path` (truncating any existing content). `stamp` is
    /// recorded in each page's `lsn` header field.
    pub(crate) fn create(
        path: &Path,
        codec: &'a dyn PageCodec,
        page_type: PageType,
        stamp: u64,
    ) -> Result<Self> {
        let file = File::create(path).map_err(|e| CoreError::io(path, e))?;
        Ok(Self {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
            codec,
            page_type,
            stamp,
            page_id: 0,
            body: Vec::with_capacity(PAGE_BODY_CAP),
            block: vec![0u8; codec.block_size()],
        })
    }

    /// Append `bytes` to the blob, sealing and writing whole pages as they fill.
    pub(crate) fn write(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let take = (PAGE_BODY_CAP - self.body.len()).min(bytes.len());
            self.body.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.body.len() == PAGE_BODY_CAP {
                self.flush_page()?;
            }
        }
        Ok(())
    }

    fn flush_page(&mut self) -> Result<()> {
        let page = build_page(self.page_type, self.page_id, self.stamp, &self.body)?;
        self.codec.seal(self.page_id, &page, &mut self.block)?;
        self.file
            .write_all(&self.block)
            .map_err(|e| CoreError::io(&self.path, e))?;
        self.page_id += 1;
        self.body.clear();
        Ok(())
    }

    /// Flush the trailing partial page (an empty blob writes no pages) and
    /// `sync_data` the file. Does not `fsync` the directory.
    pub(crate) fn finish(mut self) -> Result<()> {
        if !self.body.is_empty() {
            self.flush_page()?;
        }
        let file = self
            .file
            .into_inner()
            .map_err(|e| CoreError::io(&self.path, e.into_error()))?;
        file.sync_data().map_err(|e| CoreError::io(&self.path, e))?;
        Ok(())
    }
}

/// Write `body` to `path` as a sequence of sealed pages, then `fsync` the file.
///
/// The file's logical content is exactly `body`: it is split into
/// [`PAGE_BODY_CAP`]-byte chunks, each built as a `page_type` page (stamped with
/// `stamp` in its `lsn` header field), sealed by `codec`, and appended. An empty
/// `body` writes an empty (zero-page) file. The directory is not `fsync`'d — the
/// caller sequences that against the manifest swap.
pub(crate) fn write_blocks(
    path: &Path,
    codec: &dyn PageCodec,
    page_type: PageType,
    stamp: u64,
    body: &[u8],
) -> Result<()> {
    let mut w = BlockWriter::create(path, codec, page_type, stamp)?;
    w.write(body)?;
    w.finish()
}

/// A read-only, `mmap`-ed block file opened for random sub-range access.
///
/// Holds only the mapping and its geometry; the [`PageCodec`] is supplied per
/// read so segments do not each own a copy of the store's key material.
pub(crate) struct BlockFile {
    // `None` when the file is logically empty (zero pages): `mmap` cannot map a
    // zero-length file, and there is nothing to read anyway.
    mmap: Option<Mmap>,
    block_size: usize,
    page_type: PageType,
    n_pages: u64,
}

impl BlockFile {
    /// Open the block file at `path`. `codec` provides the on-disk block size and
    /// `page_type` the kind every page must declare; both must match those used
    /// to [`write_blocks`] the file. An empty file opens as a zero-page handle.
    ///
    /// # Errors
    /// Returns an I/O error if the file cannot be opened or mapped, or
    /// [`CoreError::MalformedPage`] if its size is not a whole number of blocks.
    pub(crate) fn open(path: &Path, codec: &dyn PageCodec, page_type: PageType) -> Result<Self> {
        let block_size = codec.block_size();
        let file = File::open(path).map_err(|e| CoreError::io(path, e))?;
        let len = file.metadata().map_err(|e| CoreError::io(path, e))?.len();
        if len == 0 {
            return Ok(Self {
                mmap: None,
                block_size,
                page_type,
                n_pages: 0,
            });
        }
        if !len.is_multiple_of(block_size as u64) {
            return Err(CoreError::MalformedPage(format!(
                "block file {} size {len} is not a multiple of block size {block_size}",
                path.display()
            )));
        }
        // SAFETY: a sealed segment file is immutable once written — it is created
        // by a checkpoint, referenced by an immutable manifest version, and never
        // mutated in place (compaction writes a *new* segment and reclaims this
        // one only after dropping every mapping) — so the mapped bytes cannot
        // change underneath the mapping.
        let mmap = unsafe { Mmap::map(&file).map_err(|e| CoreError::io(path, e))? };
        Ok(Self {
            mmap: Some(mmap),
            block_size,
            page_type,
            n_pages: len / block_size as u64,
        })
    }

    /// Read `len` logical bytes starting at logical offset `off`, decrypting and
    /// CRC-checking every page the range touches.
    ///
    /// # Errors
    /// Returns an integrity error ([`CoreError::PageCorrupt`] / `BadMagic` /
    /// `MalformedPage`) if a touched page fails verification, or
    /// [`CoreError::MalformedPage`] if the range runs past the written content.
    pub(crate) fn read_range(
        &self,
        codec: &dyn PageCodec,
        off: usize,
        len: usize,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut pos = off;
        let mut remaining = len;
        while remaining > 0 {
            let page_idx = (pos / PAGE_BODY_CAP) as u64;
            let intra = pos % PAGE_BODY_CAP;
            let body = self.page_body(codec, page_idx)?;
            if intra >= body.len() {
                return Err(CoreError::MalformedPage(format!(
                    "block-file read past page {page_idx}: offset {intra} ≥ {} live bytes",
                    body.len()
                )));
            }
            let take = remaining.min(body.len() - intra);
            out.extend_from_slice(&body[intra..intra + take]);
            pos += take;
            remaining -= take;
        }
        Ok(out)
    }

    // Decrypt, validate, and copy out the live body of one page.
    fn page_body(&self, codec: &dyn PageCodec, page_idx: u64) -> Result<Vec<u8>> {
        let Some(mmap) = &self.mmap else {
            return Err(CoreError::MalformedPage(
                "read from an empty block file".to_owned(),
            ));
        };
        if page_idx >= self.n_pages {
            return Err(CoreError::MalformedPage(format!(
                "block-file page {page_idx} out of range (file has {} pages)",
                self.n_pages
            )));
        }
        let start = page_idx as usize * self.block_size;
        let block = &mmap[start..start + self.block_size];
        let mut page = [0u8; PAGE_SIZE];
        codec.open(page_idx, block, &mut page)?;
        let (_, body) = parse_page(&page, self.page_type)?;
        Ok(body.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PlainCodec;

    fn roundtrip_at(body: &[u8], reads: &[(usize, usize)]) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blk");
        write_blocks(&path, &PlainCodec, PageType::Segment, 7, body).unwrap();
        let bf = BlockFile::open(&path, &PlainCodec, PageType::Segment).unwrap();
        for &(off, len) in reads {
            assert_eq!(
                bf.read_range(&PlainCodec, off, len).unwrap(),
                &body[off..off + len],
                "read ({off},{len}) mismatch"
            );
        }
    }

    #[test]
    fn single_page_subranges_roundtrip() {
        let body: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        roundtrip_at(&body, &[(0, 0), (0, 1), (0, 500), (100, 200), (499, 1)]);
    }

    #[test]
    fn straddling_reads_cross_page_boundaries() {
        // Three-plus pages; read ranges that straddle the page boundary at
        // PAGE_BODY_CAP and PAGE_BODY_CAP*2.
        let len = PAGE_BODY_CAP * 3 + 17;
        let body: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
        roundtrip_at(
            &body,
            &[
                (PAGE_BODY_CAP - 5, 10),
                (PAGE_BODY_CAP * 2 - 3, 9),
                (0, len),
                (PAGE_BODY_CAP, PAGE_BODY_CAP + 1),
            ],
        );
    }

    #[test]
    fn streaming_writer_matches_write_blocks_byte_for_byte() {
        // The on-disk bytes must not depend on how the body is chunked across
        // `BlockWriter::write` calls — that is what keeps the streaming compaction
        // path (ADR-0068) format-compatible with `write_blocks`.
        for len in [
            0usize,
            1,
            PAGE_BODY_CAP - 1,
            PAGE_BODY_CAP,
            PAGE_BODY_CAP * 2 + 9,
        ] {
            let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let dir = tempfile::tempdir().unwrap();

            let whole = dir.path().join("whole");
            write_blocks(&whole, &PlainCodec, PageType::Segment, 7, &body).unwrap();

            let streamed = dir.path().join("streamed");
            let mut w = BlockWriter::create(&streamed, &PlainCodec, PageType::Segment, 7).unwrap();
            for chunk in body.chunks(100) {
                w.write(chunk).unwrap();
            }
            w.finish().unwrap();

            assert_eq!(
                std::fs::read(&whole).unwrap(),
                std::fs::read(&streamed).unwrap(),
                "chunked stream differs from write_blocks at len {len}"
            );
        }
    }

    #[test]
    fn empty_file_opens_and_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blk");
        write_blocks(&path, &PlainCodec, PageType::Segment, 1, &[]).unwrap();
        let bf = BlockFile::open(&path, &PlainCodec, PageType::Segment).unwrap();
        assert_eq!(bf.read_range(&PlainCodec, 0, 0).unwrap(), Vec::<u8>::new());
        assert!(bf.read_range(&PlainCodec, 0, 1).is_err());
    }

    #[test]
    fn read_past_end_errors() {
        let body = vec![1u8; 100];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blk");
        write_blocks(&path, &PlainCodec, PageType::Segment, 1, &body).unwrap();
        let bf = BlockFile::open(&path, &PlainCodec, PageType::Segment).unwrap();
        assert!(bf.read_range(&PlainCodec, 90, 20).is_err());
    }

    #[test]
    fn corruption_in_a_touched_page_is_detected() {
        let body = vec![0xABu8; 300];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blk");
        write_blocks(&path, &PlainCodec, PageType::Segment, 1, &body).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside page 0's *live* body (header is 32 B; 300 live bytes
        // follow), so the CRC — which covers only the header and live body —
        // catches it. (Flipping trailing padding is invisible, by design.)
        bytes[100] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let bf = BlockFile::open(&path, &PlainCodec, PageType::Segment).unwrap();
        assert!(matches!(
            bf.read_range(&PlainCodec, 0, 300),
            Err(CoreError::PageCorrupt { .. })
        ));
    }

    #[test]
    fn wrong_page_type_is_rejected() {
        let body = vec![5u8; 64];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blk");
        write_blocks(&path, &PlainCodec, PageType::Manifest, 1, &body).unwrap();
        let bf = BlockFile::open(&path, &PlainCodec, PageType::Segment).unwrap();
        assert!(bf.read_range(&PlainCodec, 0, 64).is_err());
    }
}
