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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    // The operator-facing Display messages are produced only when an error is
    // formatted (logged / returned to a caller), which the variant-matching tests
    // elsewhere never trigger. Format every variant so a broken message surfaces
    // here rather than in production logs.
    #[test]
    fn every_variant_formats_a_useful_message() {
        let io = CoreError::io(
            "/tmp/x",
            io::Error::new(ErrorKind::PermissionDenied, "boom"),
        );
        assert_eq!(io.to_string(), "i/o error at /tmp/x: boom");

        let bare: CoreError = io::Error::new(ErrorKind::UnexpectedEof, "eof").into();
        assert_eq!(bare.to_string(), "i/o error: eof");

        assert_eq!(
            CoreError::BadMagic {
                expected: 0xDEAD_BEEF,
                found: 0x0000_0001
            }
            .to_string(),
            "bad magic: expected 0xdeadbeef, found 0x00000001",
        );
        assert_eq!(
            CoreError::UnsupportedVersion {
                found: 9,
                supported: 2
            }
            .to_string(),
            "unsupported format version 9 (this build supports 2)",
        );
        assert_eq!(
            CoreError::PageCorrupt {
                page_id: 7,
                expected: 0x0000_00ff,
                computed: 0x0000_0100
            }
            .to_string(),
            "page 7 failed crc check (header 0x000000ff, computed 0x00000100)",
        );

        assert_eq!(
            CoreError::MalformedPage("len".into()).to_string(),
            "malformed page: len"
        );
        assert_eq!(
            CoreError::TooLarge("payload".into()).to_string(),
            "value too large: payload"
        );
        assert_eq!(CoreError::NotFound("c".into()).to_string(), "not found: c");
        assert_eq!(
            CoreError::AlreadyExists("c".into()).to_string(),
            "already exists: c"
        );
        assert_eq!(
            CoreError::InvalidArgument("dim".into()).to_string(),
            "invalid argument: dim",
        );

        // A real postcard failure flows through the `#[from]` conversion.
        let de = postcard::from_bytes::<u32>(&[]).unwrap_err();
        let err: CoreError = de.into();
        assert!(err.to_string().starts_with("serialization error:"), "{err}");
    }

    #[test]
    fn io_constructor_tags_the_path() {
        let err = CoreError::io("data/seg.000", io::Error::from(ErrorKind::NotFound));
        match err {
            CoreError::Io { path, .. } => assert_eq!(path, PathBuf::from("data/seg.000")),
            other => panic!("expected Io, got {other:?}"),
        }
    }
}
