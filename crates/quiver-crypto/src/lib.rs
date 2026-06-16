// SPDX-License-Identifier: AGPL-3.0-only
//! Audited-cryptography wrappers for Quiver: AEAD encryption-at-rest for pages
//! and write-ahead-log records, with HKDF-SHA256 key derivation.
//!
//! Quiver implements no cryptographic primitives of its own — every primitive
//! comes from an audited library (XChaCha20-Poly1305, ChaCha20, HMAC-SHA256,
//! HKDF-SHA256, and SHA-256 from the RustCrypto project; `rustls` for TLS in the
//! server). This crate is a thin, well-tested integration layer that plugs an
//! [`AeadCodec`] into the storage engine's [`quiver_core::page::PageCodec`] seam,
//! so enabling encryption-at-rest is a one-line change at `open` time and covers
//! **all** durable data — paged manifest and segment files *and* the
//! record-framed WAL. It also hosts the **client-side** ciphers Quiver never sees
//! the key for: [`PayloadCipher`] (payload envelopes), the experimental
//! [`DcpeCipher`] (property-preserving vector encryption), and [`VectorCipher`]
//! (semantically secure opaque vector encryption).
//!
//! Design: [`docs/security/crypto.md`](https://github.com/achref-soua/quiver/blob/main/docs/security/crypto.md),
//! ADR-0010 (envelope encryption & AEAD), ADR-0012 (client-side encryption),
//! ADR-0031 (experimental DCPE vector encryption — composed from the primitives
//! above; it is **not** semantically secure, see [`dcpe`]), and ADR-0032
//! (semantically secure client-side opaque vector encryption, see [`vector`]).
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

use std::path::Path;

use quiver_core::KeyRing;

mod codec;
pub mod dcpe;
pub mod envelope;
pub mod payload;
pub mod vector;

pub use codec::{AeadCodec, KEY_LEN};
pub use dcpe::{DcpeCipher, DcpeError, EncryptedVector};
pub use envelope::EnvelopeKeyRing;
pub use payload::{ENVELOPE_KEY, PayloadCipher, PayloadError, is_sealed};
pub use vector::{VECTOR_ENVELOPE_KEY, VectorCipher, VectorError, is_sealed_vector};

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

    /// Encryption-at-rest is required (the process is not running insecure) but
    /// no master key was supplied.
    #[error(
        "no encryption key set: encryption-at-rest is on by default — set \
         QUIVER_ENCRYPTION_KEY (or QUIVER_MASTER_KEY_FILE), or run insecure"
    )]
    KeyRequired,
}

/// Resolve the at-rest key-ring for a database at `data_dir` from an optional
/// hex master key, applying Quiver's secure-by-default posture (ADR-0010,
/// ADR-0013) uniformly across the network server, the MCP server, and the CLI:
///
/// - `Some(key)` ⇒ an [`EnvelopeKeyRing`] master key — each collection gets its
///   own wrapped data-encryption key;
/// - `None` with `insecure` ⇒ `Ok(None)`: the caller opens the store in
///   plaintext;
/// - `None` without `insecure` ⇒ [`CryptoError::KeyRequired`].
///
/// Every Quiver entrypoint opens through this one function, so a data directory
/// written by one is byte-for-byte readable by another.
///
/// # Errors
/// [`CryptoError::InvalidKey`] if `master_key` is malformed or the keys
/// directory cannot be prepared; [`CryptoError::KeyRequired`] if no key is
/// supplied outside insecure mode.
pub fn open_keyring(
    data_dir: &Path,
    master_key: Option<&str>,
    insecure: bool,
) -> Result<Option<Box<dyn KeyRing>>, CryptoError> {
    match master_key {
        Some(key) => {
            let keyring: Box<dyn KeyRing> = Box::new(EnvelopeKeyRing::from_hex(key, data_dir)?);
            Ok(Some(keyring))
        }
        None if insecure => Ok(None),
        None => Err(CryptoError::KeyRequired),
    }
}

#[cfg(test)]
mod open_keyring_tests {
    use super::*;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn a_master_key_yields_a_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let kr = open_keyring(dir.path(), Some(KEY), false).unwrap();
        assert!(kr.is_some(), "a key should produce a key-ring");
    }

    #[test]
    fn no_key_in_insecure_mode_is_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let kr = open_keyring(dir.path(), None, true).unwrap();
        assert!(kr.is_none(), "insecure + no key should open in plaintext");
    }

    #[test]
    fn no_key_outside_insecure_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            open_keyring(dir.path(), None, false),
            Err(CryptoError::KeyRequired)
        ));
    }

    #[test]
    fn a_malformed_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            open_keyring(dir.path(), Some("not-hex"), false),
            Err(CryptoError::InvalidKey(_))
        ));
    }
}
