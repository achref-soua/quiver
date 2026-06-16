// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side **opaque** vector encryption (ADR-0032): the reference
//! implementation of Quiver's semantically secure vector mode.
//!
//! Unlike DCPE ([`crate::dcpe`]), which deliberately leaks the
//! distance-comparison relation so an untrusted server can rank ciphertexts,
//! this mode seals a vector into an **IND-CPA** AEAD blob the server can neither
//! read nor compare. The server stores the blob (in the payload) alongside a zero
//! placeholder vector, does **no** distance math, and returns the entitled set to
//! the client — which decrypts and ranks locally. It is the strongest point on
//! Quiver's encrypted-search spectrum: the server learns only ciphertext, the
//! collection's size and dimension, whatever payload fields the client leaves
//! cleartext to stay server-filterable, and access patterns. The honest cost is
//! that the server does not rank, so it suits small/medium collections or
//! server-pre-filtered subsets (ADR-0032).
//!
//! [`VectorCipher`] is usable directly by a Rust embedder of
//! [`quiver_embed`](https://docs.rs/quiver-embed) and is the canonical definition
//! of the on-the-wire envelope the Python and TypeScript SDKs mirror. The envelope
//! parallels the payload envelope ([`crate::payload`]) but seals the vector's raw
//! little-endian `f32` bytes rather than JSON, so it round-trips **bit-exactly**
//! and reproduces **byte-identically** across languages (no transcendental floats,
//! unlike DCPE):
//!
//! ```json
//! { "__quiver_vec__": {
//!     "v":   1,
//!     "alg": "xchacha20poly1305",
//!     "dim": 8,
//!     "n":   "<base64 24-byte nonce>",
//!     "ct":  "<base64 ciphertext+tag>"
//! } }
//! ```
//!
//! The sealed blob is meant to ride in a point's payload under the reserved key
//! [`VECTOR_ENVELOPE_KEY`], next to the zero placeholder vector the client
//! upserts. Cleartext sibling fields stay server-filterable; [`VectorCipher::open`]
//! reads only the reserved key.
//!
//! ```
//! use quiver_crypto::vector::VectorCipher;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cipher = VectorCipher::from_hex(
//!     "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
//! )?;
//! let v = [0.1f32, -0.2, 0.3, 0.4];
//! let sealed = cipher.seal(&v)?;          // store this blob in the payload
//! let recovered = cipher.open(&sealed)?;  // exact, bit-for-bit
//! assert_eq!(recovered, v);
//! # Ok(())
//! # }
//! ```
//!
//! ## Algorithm and key handling
//!
//! Sealing uses **XChaCha20-Poly1305** (the same audited RustCrypto primitive as
//! encryption-at-rest and payload encryption) with a fresh random 192-bit nonce
//! per seal, so nonce reuse is impossible by construction. The supplied 256-bit
//! key is used directly — no key derivation — so the envelope is trivially
//! reproducible in any language. **Use a dedicated key for vector encryption;
//! never reuse your `QUIVER_ENCRYPTION_KEY` (at-rest) key.** The client owns key
//! management: losing the key means the vectors are unrecoverable (ADR-0032).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::codec::{KEY_LEN, decode_key_hex};

/// The reserved payload key under which a sealed vector envelope is stored.
pub const VECTOR_ENVELOPE_KEY: &str = "__quiver_vec__";

/// The envelope format version (the `v` field).
const VERSION: u64 = 1;
/// The AEAD algorithm identifier (the `alg` field).
const ALG: &str = "xchacha20poly1305";
/// XChaCha20-Poly1305 nonce length in bytes (192 bits).
const NONCE_LEN: usize = 24;
/// Poly1305 authentication tag length in bytes.
const TAG_LEN: usize = 16;
/// Associated data binding every ciphertext to this envelope version, so a blob
/// from a different scheme/version cannot be opened as a v1 vector envelope. It is
/// distinct from the payload envelope's AAD, so the two envelopes never collide.
const AAD: &[u8] = b"quiver/vector/v1";

/// Errors from sealing or opening a client-side vector envelope.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VectorError {
    /// The supplied key material was not a valid 256-bit key. The message never
    /// echoes the key material.
    #[error("invalid vector key: {0}")]
    InvalidKey(String),
    /// The value is not a Quiver vector envelope (no [`VECTOR_ENVELOPE_KEY`]
    /// field).
    #[error("value is not a quiver-encrypted vector envelope")]
    NotEncrypted,
    /// The envelope is structurally invalid (missing/garbled fields, an
    /// unsupported version or algorithm, undecodable base64, or a decrypted byte
    /// length that does not match the stated dimension).
    #[error("malformed encrypted vector envelope: {0}")]
    Malformed(String),
    /// Sealing or opening failed. On open this means the wrong key or a tampered
    /// ciphertext (the Poly1305 tag did not verify).
    #[error("vector cryptographic operation failed: {0}")]
    Crypto(&'static str),
}

/// A client-held key for sealing and opening vector envelopes (ADR-0032).
///
/// The key is held in a [`Zeroizing`] buffer and wiped on drop. Construct one
/// cipher per key and reuse it.
pub struct VectorCipher {
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl VectorCipher {
    /// Build a cipher from a raw 256-bit key. The key is copied into a zeroizing
    /// buffer; the caller should zeroize its own copy.
    #[must_use]
    pub fn new(key: [u8; KEY_LEN]) -> Self {
        Self {
            key: Zeroizing::new(key),
        }
    }

    /// Build a cipher from a 64-character hex-encoded 256-bit key. Errors if the
    /// string is not exactly 64 hex digits.
    ///
    /// # Errors
    /// [`VectorError::InvalidKey`] if `hex` is not a valid 256-bit key.
    pub fn from_hex(hex: &str) -> Result<Self, VectorError> {
        let key = decode_key_hex(hex).map_err(|e| VectorError::InvalidKey(e.to_string()))?;
        Ok(Self::new(key))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(self.key.as_slice()))
    }

    /// Seal `vector` into a one-key envelope object `{ VECTOR_ENVELOPE_KEY: { .. } }`.
    /// The vector's little-endian `f32` bytes are encrypted as one AEAD message;
    /// each call uses a fresh random nonce, so sealing the same vector twice yields
    /// different ciphertext.
    ///
    /// # Errors
    /// [`VectorError::Crypto`] if the AEAD layer fails (e.g. the message exceeds
    /// the cipher's limits).
    pub fn seal(&self, vector: &[f32]) -> Result<Value, VectorError> {
        let bytes: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher()
            .encrypt(
                &nonce,
                Payload {
                    msg: &bytes,
                    aad: AAD,
                },
            )
            .map_err(|_| VectorError::Crypto("AEAD sealing failed"))?;
        Ok(json!({
            VECTOR_ENVELOPE_KEY: {
                "v": VERSION,
                "alg": ALG,
                "dim": vector.len() as u64,
                "n": BASE64.encode(nonce.as_slice()),
                "ct": BASE64.encode(&ciphertext),
            }
        }))
    }

    /// Open an envelope sealed by [`VectorCipher::seal`], returning the original
    /// vector. The `sealed` value may carry cleartext sibling fields; only the
    /// [`VECTOR_ENVELOPE_KEY`] field is read. A wrong key or any tampering fails the
    /// authentication tag and returns [`VectorError::Crypto`].
    ///
    /// # Errors
    /// [`VectorError::NotEncrypted`] if there is no envelope; [`VectorError::Malformed`]
    /// for a structurally invalid envelope or a length/dimension mismatch;
    /// [`VectorError::Crypto`] for a wrong key or tampered ciphertext.
    pub fn open(&self, sealed: &Value) -> Result<Vec<f32>, VectorError> {
        let envelope = sealed
            .get(VECTOR_ENVELOPE_KEY)
            .ok_or(VectorError::NotEncrypted)?;

        let version = envelope.get("v").and_then(Value::as_u64);
        if version != Some(VERSION) {
            return Err(VectorError::Malformed(format!(
                "unsupported envelope version: {}",
                version.map_or_else(|| "missing".to_owned(), |v| v.to_string())
            )));
        }
        match envelope.get("alg").and_then(Value::as_str) {
            Some(ALG) => {}
            other => {
                return Err(VectorError::Malformed(format!(
                    "unsupported envelope algorithm: {}",
                    other.unwrap_or("missing")
                )));
            }
        }
        let dim = envelope
            .get("dim")
            .and_then(Value::as_u64)
            .ok_or_else(|| VectorError::Malformed("missing or non-integer `dim`".to_owned()))?;

        let nonce_bytes = decode_field(envelope, "n")?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(VectorError::Malformed(format!(
                "nonce is {} bytes, expected {NONCE_LEN}",
                nonce_bytes.len()
            )));
        }
        let ciphertext = decode_field(envelope, "ct")?;
        if ciphertext.len() < TAG_LEN {
            return Err(VectorError::Malformed(format!(
                "ciphertext is {} bytes, shorter than the {TAG_LEN}-byte tag",
                ciphertext.len()
            )));
        }

        let plaintext = self
            .cipher()
            .decrypt(
                XNonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &ciphertext,
                    aad: AAD,
                },
            )
            .map_err(|_| VectorError::Crypto("wrong key or tampered ciphertext"))?;

        // The decrypted bytes must be exactly `dim` little-endian f32s. The `dim`
        // header is not in the AAD, but a mismatch here proves it was altered (the
        // ciphertext length is fixed by the tag), so we reject rather than guess.
        if plaintext.len() != dim as usize * 4 {
            return Err(VectorError::Malformed(format!(
                "decrypted {} bytes, expected {} for dim {dim}",
                plaintext.len(),
                dim as usize * 4
            )));
        }
        Ok(plaintext
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }
}

/// Whether `value` carries a Quiver vector envelope (the [`VECTOR_ENVELOPE_KEY`]
/// field). Useful for SDKs that auto-decrypt only sealed vectors.
#[must_use]
pub fn is_sealed_vector(value: &Value) -> bool {
    value.get(VECTOR_ENVELOPE_KEY).is_some()
}

// Base64-decode a required string field of the envelope.
fn decode_field(envelope: &Value, field: &str) -> Result<Vec<u8>, VectorError> {
    let encoded = envelope
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| VectorError::Malformed(format!("missing envelope field `{field}`")))?;
    BASE64
        .decode(encoded)
        .map_err(|e| VectorError::Malformed(format!("envelope field `{field}` is not base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn cipher() -> VectorCipher {
        match VectorCipher::from_hex(KEY_HEX) {
            Ok(c) => c,
            Err(e) => panic!("test key should parse: {e}"),
        }
    }

    #[test]
    fn seal_then_open_round_trips_bit_exactly() {
        let cipher = cipher();
        // Includes negatives, zero, fractional, and a large magnitude — all exact
        // through the LE-byte round-trip (no float transform).
        let v = vec![0.0f32, 1.0, -1.0, 0.5, -0.5, 7.25, 2.5, 42.0];
        let sealed = cipher.seal(&v).expect("seal");
        assert!(is_sealed_vector(&sealed));
        let envelope = &sealed[VECTOR_ENVELOPE_KEY];
        assert_eq!(envelope["v"], json!(VERSION));
        assert_eq!(envelope["alg"], json!(ALG));
        assert_eq!(envelope["dim"], json!(8));
        assert!(envelope["n"].is_string() && envelope["ct"].is_string());
        // Bit-exact: equality, not approximate.
        assert_eq!(cipher.open(&sealed).expect("open"), v);
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let cipher = cipher();
        let v = [1.0f32, 2.0, 3.0];
        let a = cipher.seal(&v).expect("seal a");
        let b = cipher.seal(&v).expect("seal b");
        assert_ne!(a[VECTOR_ENVELOPE_KEY]["n"], b[VECTOR_ENVELOPE_KEY]["n"]);
        assert_ne!(a[VECTOR_ENVELOPE_KEY]["ct"], b[VECTOR_ENVELOPE_KEY]["ct"]);
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let sealed = cipher().seal(&[1.0f32, 2.0]).expect("seal");
        let wrong =
            VectorCipher::from_hex(&"ff".repeat(KEY_LEN)).expect("wrong key parses but differs");
        assert!(matches!(wrong.open(&sealed), Err(VectorError::Crypto(_))));
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let cipher = cipher();
        let mut sealed = cipher.seal(&[9.0f32, 8.0, 7.0]).expect("seal");
        let ct = sealed[VECTOR_ENVELOPE_KEY]["ct"]
            .as_str()
            .expect("ct string");
        let mut bytes = BASE64.decode(ct).expect("decode ct");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        sealed[VECTOR_ENVELOPE_KEY]["ct"] = json!(BASE64.encode(&bytes));
        assert!(matches!(cipher.open(&sealed), Err(VectorError::Crypto(_))));
    }

    #[test]
    fn open_cleartext_value_reports_not_encrypted() {
        let cipher = cipher();
        assert!(matches!(
            cipher.open(&json!({ "tier": "gold" })),
            Err(VectorError::NotEncrypted)
        ));
        assert!(!is_sealed_vector(&json!({ "tier": "gold" })));
    }

    #[test]
    fn open_reads_only_the_envelope_ignoring_cleartext_siblings() {
        let cipher = cipher();
        let sealed = cipher.seal(&[1.5f32, 2.5]).expect("seal");
        // Merge the envelope alongside a cleartext, server-filterable field.
        let mut payload = json!({ "tier": "gold" });
        let obj = payload.as_object_mut().expect("object");
        obj.extend(sealed.as_object().expect("envelope object").clone());
        assert_eq!(payload["tier"], json!("gold"));
        assert!(is_sealed_vector(&payload));
        assert_eq!(cipher.open(&payload).expect("open"), vec![1.5f32, 2.5]);
    }

    #[test]
    fn open_rejects_unknown_version_and_algorithm() {
        let cipher = cipher();
        let good = cipher.seal(&[1.0f32]).expect("seal");

        let mut bad = good.clone();
        bad[VECTOR_ENVELOPE_KEY]["v"] = json!(999);
        assert!(matches!(cipher.open(&bad), Err(VectorError::Malformed(_))));

        let mut bad = good;
        bad[VECTOR_ENVELOPE_KEY]["alg"] = json!("aes-256-gcm");
        assert!(matches!(cipher.open(&bad), Err(VectorError::Malformed(_))));
    }

    #[test]
    fn open_rejects_dimension_mismatch() {
        let cipher = cipher();
        let mut sealed = cipher.seal(&[1.0f32, 2.0, 3.0]).expect("seal");
        // Claim a different dimension than the ciphertext actually holds.
        sealed[VECTOR_ENVELOPE_KEY]["dim"] = json!(4);
        assert!(matches!(
            cipher.open(&sealed),
            Err(VectorError::Malformed(_))
        ));
    }

    #[test]
    fn from_hex_rejects_bad_keys() {
        assert!(matches!(
            VectorCipher::from_hex("abcd"),
            Err(VectorError::InvalidKey(_))
        ));
        assert!(matches!(
            VectorCipher::from_hex(&"zz".repeat(KEY_LEN)),
            Err(VectorError::InvalidKey(_))
        ));
    }

    // Cross-language anchor: an envelope produced once by this Rust reference,
    // embedded verbatim. The Python and TypeScript SDKs decrypt this exact blob in
    // their own tests, proving byte-identical interop (ADR-0032). Because the
    // sealed message is raw f32 LE bytes, interop is exact — not tolerance-based
    // like DCPE.
    const KAT_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const KAT_VECTOR: [f32; 6] = [0.0, 1.0, -1.0, 0.5, -0.25, 3.5];
    const KAT_ENVELOPE: &str = r#"{"__quiver_vec__":{"alg":"xchacha20poly1305","ct":"8zgd/+aSyPbmk1vkIdfaGYBKr45Bv0DsPOGdDFojuCqldB3jGiguWQ==","dim":6,"n":"1Tt6qe+yyU87VhS4bfOpdtloq2DlFllv","v":1}}"#;

    #[test]
    fn decrypts_the_known_answer_envelope() {
        let cipher = VectorCipher::from_hex(KAT_KEY_HEX).expect("kat key");
        let sealed: Value = serde_json::from_str(KAT_ENVELOPE).expect("kat json");
        assert_eq!(cipher.open(&sealed).expect("open kat"), KAT_VECTOR.to_vec());
    }
}
