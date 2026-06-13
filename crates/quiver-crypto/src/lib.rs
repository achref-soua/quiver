// SPDX-License-Identifier: AGPL-3.0-only
//! Audited-cryptography wrappers for Quiver: AEAD encryption-at-rest for pages
//! and write-ahead-log records, with HKDF-SHA256 key derivation.
//!
//! Quiver implements no cryptographic primitives of its own — every primitive
//! comes from an audited library (XChaCha20-Poly1305, HKDF-SHA256, and SHA-256
//! from the RustCrypto project; `rustls` for TLS in the server). This crate is a
//! thin, well-tested integration layer that plugs an [`AeadCodec`] into the
//! storage engine's [`quiver_core::page::PageCodec`] seam, so enabling
//! encryption-at-rest is a one-line change at `open` time and covers **all**
//! durable data — paged manifest and segment files *and* the record-framed WAL.
//!
//! Design: [`docs/security/crypto.md`](https://github.com/achref-soua/quiver/blob/main/docs/security/crypto.md),
//! ADR-0010 (envelope encryption & AEAD) and ADR-0012 (client-side encryption).
//!
//! ```no_run
//! use quiver_crypto::AeadCodec;
//! use quiver_core::Store;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let codec = AeadCodec::from_hex(&std::env::var("QUIVER_ENCRYPTION_KEY")?)?;
//! let store = Store::open_with_codec(std::path::Path::new("./data"), Box::new(codec))?;
//! # let _ = store;
//! # Ok(())
//! # }
//! ```

mod codec;

pub use codec::{AeadCodec, KEY_LEN};

/// Errors from constructing or configuring a [`AeadCodec`].
///
/// Failures that occur while sealing or opening data are reported through the
/// storage engine's [`quiver_core::CoreError`] (an open failure surfaces as a
/// malformed-page error), so recovery and read paths have one error channel.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CryptoError {
    /// The supplied key material was not a valid 256-bit key (wrong length or a
    /// non-hex character). The message never echoes the key material.
    #[error("invalid encryption key: {0}")]
    InvalidKey(String),
}
