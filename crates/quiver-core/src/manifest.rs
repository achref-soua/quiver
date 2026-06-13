// SPDX-License-Identifier: AGPL-3.0-only
//! The versioned manifest: the catalog and durability anchor.
//!
//! The manifest records what is live — per collection, the set of sealed,
//! immutable segments with their LSN ranges and schema — plus the global
//! `last_checkpointed_lsn` (the WAL position safely captured in segments) and
//! the id allocators. It is the source of truth consulted first on recovery.
//!
//! Each update writes a new immutable `manifest-NNNNNNNNNN` file and atomically
//! swaps the `CURRENT` pointer using the **write-new + fsync + atomic-rename**
//! protocol (LevelDB-style, ADR-0004): the new manifest is written and `fsync`'d,
//! the directory is `fsync`'d, then `CURRENT.tmp` is written, `fsync`'d, and
//! `rename`d over `CURRENT`. A crash at any point leaves either the old or the
//! new catalog fully intact — never a half-written one. A manifest file written
//! but never pointed to by `CURRENT` is an orphan, ignored on read and garbage
//! collected by the engine.
//!
//! The manifest body is `postcard`-encoded (ADR-0003) and laid out across one or
//! more [`crate::page`] pages, inheriting their per-page CRC integrity. Page 0's
//! body begins with the total body length so the reader concatenates exactly the
//! right bytes.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ids::{CollectionId, Lsn};
use crate::page::{PageCodec, PageType};
use crate::paged::{fsync_dir, read_paged, write_paged};

/// On-disk manifest schema version (independent of the product SemVer and of the
/// page format version).
pub const MANIFEST_FORMAT_VERSION: u16 = 1;

const CURRENT_FILE: &str = "CURRENT";
const CURRENT_TMP: &str = "CURRENT.tmp";

fn manifest_file_name(version: u64) -> String {
    // Zero-padded so lexical order matches numeric order.
    format!("manifest-{version:010}")
}

/// A reference to one sealed, immutable segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    /// Monotonic segment id; also names the segment's files.
    pub id: u64,
    /// Number of rows (including tombstoned) in the segment.
    pub row_count: u64,
    /// Lowest LSN captured in this segment.
    pub lsn_low: Lsn,
    /// Highest LSN captured in this segment.
    pub lsn_high: Lsn,
}

/// Catalog entry for one collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionEntry {
    /// Stable collection id.
    pub id: CollectionId,
    /// Human-readable collection name, unique within the store.
    pub name: String,
    /// Postcard-encoded collection descriptor (dim, dtype, metric, fields).
    pub descriptor: Vec<u8>,
    /// Live sealed segments, in creation order.
    pub segments: Vec<SegmentRef>,
}

/// A complete catalog snapshot. Immutable once written; superseded by writing a
/// higher version and swapping `CURRENT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest schema version.
    pub format_version: u16,
    /// Monotonic manifest version; also names the file.
    pub version: u64,
    /// Highest LSN durably captured in segments — the WAL replay floor.
    pub last_checkpointed_lsn: Lsn,
    /// Next collection id to hand out.
    pub next_collection_id: u64,
    /// Next segment id to hand out.
    pub next_segment_id: u64,
    /// All collections in the store.
    pub collections: Vec<CollectionEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self::empty()
    }
}

impl Manifest {
    /// An empty manifest for a brand-new store (version 0).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            version: 0,
            last_checkpointed_lsn: Lsn::ZERO,
            next_collection_id: 0,
            next_segment_id: 0,
            collections: Vec::new(),
        }
    }

    /// Find a collection by id.
    #[must_use]
    pub fn collection(&self, id: CollectionId) -> Option<&CollectionEntry> {
        self.collections.iter().find(|c| c.id == id)
    }

    /// Find a collection by name.
    #[must_use]
    pub fn collection_by_name(&self, name: &str) -> Option<&CollectionEntry> {
        self.collections.iter().find(|c| c.name == name)
    }
}

/// Serialize `manifest` and durably install it as the new `CURRENT`, using the
/// write-new + fsync + atomic-rename protocol. `dir` is the store root.
pub fn write_manifest(dir: &Path, manifest: &Manifest, codec: &dyn PageCodec) -> Result<()> {
    let body = postcard::to_allocvec(manifest)?;

    // 1. Write the new manifest file in full and fsync it.
    let file_name = manifest_file_name(manifest.version);
    let manifest_path = dir.join(&file_name);
    write_paged(
        &manifest_path,
        codec,
        PageType::Manifest,
        manifest.version,
        &body,
    )?;
    // 2. fsync the directory so the new file entry is durable before we point at it.
    fsync_dir(dir)?;

    // 3. Write CURRENT.tmp pointing at the new manifest, and fsync it.
    let tmp_path = dir.join(CURRENT_TMP);
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|e| CoreError::io(&tmp_path, e))?;
        f.write_all(file_name.as_bytes())
            .map_err(|e| CoreError::io(&tmp_path, e))?;
        f.write_all(b"\n")
            .map_err(|e| CoreError::io(&tmp_path, e))?;
        f.sync_data().map_err(|e| CoreError::io(&tmp_path, e))?;
    }
    // 4. Atomically swap CURRENT, then fsync the directory to make it durable.
    let current_path = dir.join(CURRENT_FILE);
    std::fs::rename(&tmp_path, &current_path).map_err(|e| CoreError::io(&current_path, e))?;
    fsync_dir(dir)?;
    Ok(())
}

/// Read the current manifest, or `None` if the store has no `CURRENT` yet (a
/// fresh data directory).
pub fn read_current(dir: &Path, codec: &dyn PageCodec) -> Result<Option<Manifest>> {
    let current_path = dir.join(CURRENT_FILE);
    let name = match std::fs::read_to_string(&current_path) {
        Ok(s) => s.trim().to_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CoreError::io(&current_path, e)),
    };
    if name.is_empty() {
        return Err(CoreError::MalformedPage("CURRENT is empty".to_owned()));
    }
    let manifest = read_manifest_file(&dir.join(&name), codec)?;
    Ok(Some(manifest))
}

fn read_manifest_file(path: &Path, codec: &dyn PageCodec) -> Result<Manifest> {
    let body = read_paged(path, codec, PageType::Manifest)?;
    let manifest: Manifest = postcard::from_bytes(&body)?;
    if manifest.format_version != MANIFEST_FORMAT_VERSION {
        return Err(CoreError::UnsupportedVersion {
            found: manifest.format_version,
            supported: MANIFEST_FORMAT_VERSION,
        });
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PAGE_BODY_CAP, PAGE_SIZE, PlainCodec};

    fn sample(version: u64, n_collections: usize, desc_len: usize) -> Manifest {
        let collections = (0..n_collections)
            .map(|c| CollectionEntry {
                id: CollectionId(c as u64),
                name: format!("col-{c}"),
                descriptor: vec![(c % 256) as u8; desc_len],
                segments: vec![SegmentRef {
                    id: c as u64,
                    row_count: 10 * c as u64,
                    lsn_low: Lsn(c as u64),
                    lsn_high: Lsn(c as u64 + 5),
                }],
            })
            .collect();
        Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            version,
            last_checkpointed_lsn: Lsn(version),
            next_collection_id: n_collections as u64,
            next_segment_id: n_collections as u64,
            collections,
        }
    }

    #[test]
    fn fresh_dir_has_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_current(dir.path(), &PlainCodec).unwrap(), None);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample(1, 3, 16);
        write_manifest(dir.path(), &m, &PlainCodec).unwrap();
        let back = read_current(dir.path(), &PlainCodec).unwrap();
        assert_eq!(back, Some(m));
    }

    #[test]
    fn newer_version_supersedes() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), &sample(1, 1, 8), &PlainCodec).unwrap();
        let v2 = sample(2, 2, 8);
        write_manifest(dir.path(), &v2, &PlainCodec).unwrap();
        assert_eq!(read_current(dir.path(), &PlainCodec).unwrap(), Some(v2));
    }

    #[test]
    fn multi_page_manifest_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        // A descriptor far larger than one page body forces several pages.
        let m = sample(7, 2, PAGE_BODY_CAP);
        write_manifest(dir.path(), &m, &PlainCodec).unwrap();
        // Confirm the file really spans multiple pages.
        let bytes = std::fs::read(dir.path().join(manifest_file_name(7))).unwrap();
        assert!(bytes.len() > PAGE_SIZE * 2);
        assert_eq!(read_current(dir.path(), &PlainCodec).unwrap(), Some(m));
    }

    #[test]
    fn temp_pointer_is_renamed_away() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), &sample(1, 1, 8), &PlainCodec).unwrap();
        assert!(!dir.path().join(CURRENT_TMP).exists());
        assert!(dir.path().join(CURRENT_FILE).exists());
    }

    #[test]
    fn orphan_manifest_without_current_swap_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = sample(1, 1, 8);
        write_manifest(dir.path(), &v1, &PlainCodec).unwrap();
        // Simulate a crash after a v2 file is written but before CURRENT is
        // swapped: drop a bogus manifest-0000000002 with CURRENT untouched.
        std::fs::write(dir.path().join(manifest_file_name(2)), b"garbage").unwrap();
        // CURRENT still names v1, so the orphan is ignored.
        assert_eq!(read_current(dir.path(), &PlainCodec).unwrap(), Some(v1));
    }

    #[test]
    fn stale_current_tmp_does_not_affect_read() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = sample(1, 1, 8);
        write_manifest(dir.path(), &v1, &PlainCodec).unwrap();
        std::fs::write(dir.path().join(CURRENT_TMP), b"manifest-9999999999\n").unwrap();
        assert_eq!(read_current(dir.path(), &PlainCodec).unwrap(), Some(v1));
    }

    #[test]
    fn corrupt_manifest_page_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), &sample(1, 1, 64), &PlainCodec).unwrap();
        let path = dir.path().join(manifest_file_name(1));
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside page 0's body (past the 32-byte header).
        bytes[64] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            read_current(dir.path(), &PlainCodec),
            Err(CoreError::PageCorrupt { .. })
        ));
    }

    #[test]
    fn accessors_find_collections() {
        let m = sample(1, 3, 8);
        assert_eq!(
            m.collection(CollectionId(1)).map(|c| c.name.as_str()),
            Some("col-1")
        );
        assert_eq!(
            m.collection_by_name("col-2").map(|c| c.id),
            Some(CollectionId(2))
        );
        assert!(m.collection(CollectionId(99)).is_none());
        assert!(m.collection_by_name("nope").is_none());
    }
}
