// SPDX-License-Identifier: AGPL-3.0-only
//! Error types for the storage engine.
//!
//! Library crates expose typed errors via `thiserror` (ADR-0017); the binary
//! edges add human context with `anyhow`. Integrity failures get their own
//! variants so a detected torn or tampered page aborts the read instead of
//! silently serving bad data.

use std::io;
use std::path::{Path, PathBuf};

/// Errors returned by the storage engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// An I/O operation failed against a known path.
    #[error("i/o error at {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// The underlying OS error.
        source: io::Error,
    },

    /// An I/O operation failed without an associated path.
    #[error("i/o error: {0}")]
    BareIo(#[from] io::Error),

    /// A page or file carried the wrong magic number — a different file kind, or
    /// not a Quiver file at all.
    #[error("bad magic: expected {expected:#010x}, found {found:#010x}")]
    BadMagic {
        /// The magic the reader expected.
        expected: u32,
        /// The magic actually found on disk.
        found: u32,
    },

    /// The on-disk format version is not understood by this build.
    #[error("unsupported format version {found} (this build supports {supported})")]
    UnsupportedVersion {
        /// Version read from disk.
        found: u16,
        /// Highest version this build can read.
        supported: u16,
    },

    /// A page failed its CRC32C check — corruption or tampering was detected.
    #[error("page {page_id} failed crc check (header {expected:#010x}, computed {computed:#010x})")]
    PageCorrupt {
        /// Page id from the (possibly damaged) header.
        page_id: u64,
        /// CRC stored in the page header.
        expected: u32,
        /// CRC recomputed over the page contents.
        computed: u32,
    },

    /// A page or file header was structurally invalid (impossible length,
    /// unknown page type, out-of-order page id, …).
    #[error("malformed page: {0}")]
    MalformedPage(String),

    /// Serialization or deserialization of a metadata structure failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] postcard::Error),

    /// A value exceeded a hard structural limit (e.g. a payload larger than a
    /// page body, or a WAL record over the size cap).
    #[error("value too large: {0}")]
    TooLarge(String),

    /// A referenced collection or record does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A resource that must be unique already exists (e.g. a collection name).
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// A caller supplied an invalid argument (e.g. a vector of the wrong dim).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl CoreError {
    /// Build an [`CoreError::Io`] tagged with the path it occurred on.
    #[must_use]
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}

/// Convenience alias for storage-engine results.
pub type Result<T> = std::result::Result<T, CoreError>;
