// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side payload encryption (ADR-0012): the **reference implementation**
//! of Quiver's payload envelope.
//!
//! A caller seals a JSON payload with a key Quiver never sees; the server stores
//! and returns the result as an **opaque blob** it cannot read. This is the
//! honest, scoped confidentiality guarantee from ADR-0012 — it protects
//! *payloads*, not vectors (standard ANN needs plaintext vectors server-side),
//! and encrypted fields cannot be filtered or indexed server-side.
//!
//! [`PayloadCipher`] is usable directly by a Rust embedder of
//! [`quiver_embed`](https://docs.rs/quiver-embed), and it is the canonical
//! definition of the on-the-wire envelope that the Python and TypeScript SDKs
//! mirror byte-for-byte:
//!
//! ```json
//! { "__quiver_enc__": {
//!     "v": 1,
//!     "alg": "xchacha20poly1305",
//!     "n":  "<base64 24-byte nonce>",
//!     "ct": "<base64 ciphertext+tag>"
//! } }
//! ```
//!
//! The envelope is one object with the single reserved key
//! [`ENVELOPE_KEY`]. To keep some fields server-filterable, leave them in
//! cleartext and merge the sealed envelope alongside them — `open` reads only
//! the reserved key and ignores cleartext siblings:
//!
//! ```
//! use quiver_crypto::payload::PayloadCipher;
//! use serde_json::json;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cipher = PayloadCipher::from_hex(
//!     "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
//! )?;
//! let secret = cipher.seal(&json!({ "ssn": "078-05-1120" }))?;
//! // The stored payload keeps `tier` filterable but hides `ssn`.
//! let mut payload = json!({ "tier": "gold" });
//! payload
//!     .as_object_mut()
//!     .ok_or("payload must be an object")?
//!     .extend(secret.as_object().cloned().unwrap_or_default());
//! // ... upsert `payload`; the server only ever sees the ciphertext for `ssn`.
//! let recovered = cipher.open(&payload)?;
//! assert_eq!(recovered, json!({ "ssn": "078-05-1120" }));
//! # Ok(())
//! # }
//! ```
//!
//! ## Algorithm and key handling
//!
//! Sealing uses **XChaCha20-Poly1305** (the same audited RustCrypto primitive as
//! encryption-at-rest) with a fresh random 192-bit nonce per seal, so nonce
//! reuse is impossible by construction. The supplied 256-bit key is used
//! directly — no key derivation — so the envelope is trivially reproducible in
//! any language. **Use a dedicated key for payload encryption; never reuse your
//! `QUIVER_ENCRYPTION_KEY` (at-rest) key.** The client owns key management:
//! losing the key means the data is unrecoverable (ADR-0012).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::codec::{KEY_LEN, decode_key_hex};

/// The reserved payload key under which a sealed envelope is stored.
pub const ENVELOPE_KEY: &str = "__quiver_enc__";

/// The envelope format version (the `v` field).
const VERSION: u64 = 1;
/// The AEAD algorithm identifier (the `alg` field).
const ALG: &str = "xchacha20poly1305";
/// XChaCha20-Poly1305 nonce length in bytes (192 bits).
const NONCE_LEN: usize = 24;
/// Poly1305 authentication tag length in bytes.
const TAG_LEN: usize = 16;
/// Associated data binding every ciphertext to this envelope version, so an
/// envelope from a different scheme/version cannot be opened as a v1 one.
const AAD: &[u8] = b"quiver/payload/v1";

/// Errors from sealing or opening a client-side payload envelope.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PayloadError {
    /// The supplied key material was not a valid 256-bit key. The message never
    /// echoes the key material.
    #[error("invalid payload key: {0}")]
    InvalidKey(String),
    /// The value is not a Quiver payload envelope (no [`ENVELOPE_KEY`] field).
    #[error("payload is not a quiver-encrypted envelope")]
    NotEncrypted,
    /// The envelope is structurally invalid (missing/garbled fields, an
    /// unsupported version or algorithm, or undecodable base64).
    #[error("malformed encrypted envelope: {0}")]
    Malformed(String),
    /// Sealing or opening failed. On open this means the wrong key or a tampered
    /// ciphertext (the Poly1305 tag did not verify).
    #[error("payload cryptographic operation failed: {0}")]
    Crypto(&'static str),
}

/// A client-held key for sealing and opening payload envelopes (ADR-0012).
///
/// The key is held in a [`Zeroizing`] buffer and wiped on drop. Construct one
/// cipher per key and reuse it.
pub struct PayloadCipher {
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl PayloadCipher {
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
    pub fn from_hex(hex: &str) -> Result<Self, PayloadError> {
        let key = decode_key_hex(hex).map_err(|e| PayloadError::InvalidKey(e.to_string()))?;
        Ok(Self::new(key))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(self.key.as_slice()))
    }

    /// Seal `plaintext` into a one-key envelope object `{ ENVELOPE_KEY: { .. } }`.
    /// Each call uses a fresh random nonce, so sealing the same value twice
    /// yields different ciphertext.
    pub fn seal(&self, plaintext: &Value) -> Result<Value, PayloadError> {
        let bytes = serde_json::to_vec(plaintext)
            .map_err(|e| PayloadError::Malformed(format!("serializing plaintext: {e}")))?;
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
            .map_err(|_| PayloadError::Crypto("AEAD sealing failed"))?;
        Ok(json!({
            ENVELOPE_KEY: {
                "v": VERSION,
                "alg": ALG,
                "n": BASE64.encode(nonce.as_slice()),
                "ct": BASE64.encode(&ciphertext),
            }
        }))
    }

    /// Open an envelope sealed by [`PayloadCipher::seal`], returning the original
    /// plaintext value. The `sealed` value may carry cleartext sibling fields;
    /// only the [`ENVELOPE_KEY`] field is read. A wrong key or any tampering
    /// fails the authentication tag and returns [`PayloadError::Crypto`].
    pub fn open(&self, sealed: &Value) -> Result<Value, PayloadError> {
        let envelope = sealed.get(ENVELOPE_KEY).ok_or(PayloadError::NotEncrypted)?;

        let version = envelope.get("v").and_then(Value::as_u64);
        if version != Some(VERSION) {
            return Err(PayloadError::Malformed(format!(
                "unsupported envelope version: {}",
                version.map_or_else(|| "missing".to_owned(), |v| v.to_string())
            )));
        }
        match envelope.get("alg").and_then(Value::as_str) {
            Some(ALG) => {}
            other => {
                return Err(PayloadError::Malformed(format!(
                    "unsupported envelope algorithm: {}",
                    other.unwrap_or("missing")
                )));
            }
        }

        let nonce_bytes = decode_field(envelope, "n")?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(PayloadError::Malformed(format!(
                "nonce is {} bytes, expected {NONCE_LEN}",
                nonce_bytes.len()
            )));
        }
        let ciphertext = decode_field(envelope, "ct")?;
        if ciphertext.len() < TAG_LEN {
            return Err(PayloadError::Malformed(format!(
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
            .map_err(|_| PayloadError::Crypto("wrong key or tampered ciphertext"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|e| PayloadError::Malformed(format!("decrypted bytes are not json: {e}")))
    }
}

/// Whether `value` carries a Quiver payload envelope (the [`ENVELOPE_KEY`]
/// field). Useful for SDKs that auto-decrypt only sealed payloads.
#[must_use]
pub fn is_sealed(value: &Value) -> bool {
    value.get(ENVELOPE_KEY).is_some()
}

// Base64-decode a required string field of the envelope.
fn decode_field(envelope: &Value, field: &str) -> Result<Vec<u8>, PayloadError> {
    let encoded = envelope
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| PayloadError::Malformed(format!("missing envelope field `{field}`")))?;
    BASE64.decode(encoded).map_err(|e| {
        PayloadError::Malformed(format!("envelope field `{field}` is not base64: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn cipher() -> PayloadCipher {
        match PayloadCipher::from_hex(KEY_HEX) {
            Ok(c) => c,
            Err(e) => panic!("test key should parse: {e}"),
        }
    }

    #[test]
    fn seal_then_open_round_trips() {
        let cipher = cipher();
        let plaintext = json!({ "ssn": "078-05-1120", "notes": ["a", "b"], "n": 42 });
        let sealed = cipher.seal(&plaintext).expect("seal");
        // The envelope is exactly the reserved key over a structured header.
        assert!(is_sealed(&sealed));
        let envelope = &sealed[ENVELOPE_KEY];
        assert_eq!(envelope["v"], json!(VERSION));
        assert_eq!(envelope["alg"], json!(ALG));
        assert!(envelope["n"].is_string() && envelope["ct"].is_string());
        // And it round-trips.
        assert_eq!(cipher.open(&sealed).expect("open"), plaintext);
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let cipher = cipher();
        let plaintext = json!({ "x": 1 });
        let a = cipher.seal(&plaintext).expect("seal a");
        let b = cipher.seal(&plaintext).expect("seal b");
        // Same plaintext, different nonce ⇒ different ciphertext.
        assert_ne!(a[ENVELOPE_KEY]["n"], b[ENVELOPE_KEY]["n"]);
        assert_ne!(a[ENVELOPE_KEY]["ct"], b[ENVELOPE_KEY]["ct"]);
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let sealed = cipher().seal(&json!({ "secret": true })).expect("seal");
        let wrong =
            PayloadCipher::from_hex(&"ff".repeat(KEY_LEN)).expect("wrong key parses but differs");
        assert!(matches!(wrong.open(&sealed), Err(PayloadError::Crypto(_))));
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let cipher = cipher();
        let mut sealed = cipher.seal(&json!({ "secret": "value" })).expect("seal");
        // Flip the last base64 char of the ciphertext.
        let ct = sealed[ENVELOPE_KEY]["ct"].as_str().expect("ct string");
        let mut bytes = BASE64.decode(ct).expect("decode ct");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        sealed[ENVELOPE_KEY]["ct"] = json!(BASE64.encode(&bytes));
        assert!(matches!(cipher.open(&sealed), Err(PayloadError::Crypto(_))));
    }

    #[test]
    fn open_cleartext_value_reports_not_encrypted() {
        let cipher = cipher();
        assert!(matches!(
            cipher.open(&json!({ "tier": "gold" })),
            Err(PayloadError::NotEncrypted)
        ));
        assert!(!is_sealed(&json!({ "tier": "gold" })));
    }

    #[test]
    fn open_reads_only_the_envelope_ignoring_cleartext_siblings() {
        let cipher = cipher();
        let secret = cipher.seal(&json!({ "ssn": "078-05-1120" })).expect("seal");
        // Merge the envelope alongside a cleartext, server-filterable field.
        let mut payload = json!({ "tier": "gold" });
        let obj = payload.as_object_mut().expect("object");
        obj.extend(secret.as_object().expect("envelope object").clone());
        assert_eq!(payload["tier"], json!("gold"));
        assert!(is_sealed(&payload));
        assert_eq!(
            cipher.open(&payload).expect("open"),
            json!({ "ssn": "078-05-1120" })
        );
    }

    #[test]
    fn open_rejects_unknown_version_and_algorithm() {
        let cipher = cipher();
        let mut sealed = cipher.seal(&json!({ "x": 1 })).expect("seal");
        let good = sealed.clone();

        sealed[ENVELOPE_KEY]["v"] = json!(999);
        assert!(matches!(
            cipher.open(&sealed),
            Err(PayloadError::Malformed(_))
        ));

        sealed = good;
        sealed[ENVELOPE_KEY]["alg"] = json!("aes-256-gcm");
        assert!(matches!(
            cipher.open(&sealed),
            Err(PayloadError::Malformed(_))
        ));
    }

    #[test]
    fn from_hex_rejects_bad_keys() {
        assert!(matches!(
            PayloadCipher::from_hex("abcd"),
            Err(PayloadError::InvalidKey(_))
        ));
        assert!(matches!(
            PayloadCipher::from_hex(&"zz".repeat(KEY_LEN)),
            Err(PayloadError::InvalidKey(_))
        ));
    }
}
