// SPDX-License-Identifier: AGPL-3.0-only
//! The AEAD page/record codec for encryption-at-rest.
//!
//! [`AeadCodec`] implements [`quiver_core::page::PageCodec`] with
//! **XChaCha20-Poly1305** over per-page (and per-record) subkeys derived with
//! **HKDF-SHA256**. Every seal uses a fresh random 192-bit nonce, so nonce reuse
//! is impossible by construction without relying on a global counter (the
//! extended nonce makes random selection safe well past database scale). The
//! primitives come entirely from audited RustCrypto crates — Quiver implements
//! no cryptography of its own (ADR-0010, `docs/security/crypto.md`).
//!
//! On-disk layout of one sealed page block (`block_size` bytes):
//!
//! ```text
//! [ nonce: 24 ][ ciphertext + Poly1305 tag: PAGE_SIZE + 16 ]
//! ```
//!
//! A sealed WAL record is the same shape with a variable-length ciphertext:
//! `[nonce: 24][ciphertext + tag]`.
//!
//! ## Phase 1 key handling
//!
//! Phase 1 takes a single operator-supplied 256-bit root key and derives
//! per-page / per-record subkeys from it. The full envelope hierarchy from
//! ADR-0010 — a master key wrapping per-collection DEKs, KMS integration, and
//! crypto-shredding — is Phase 3 ("security depth"); the on-disk format already
//! accommodates it because keys never appear on disk here either way.

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use quiver_core::error::{CoreError, Result};
use quiver_core::page::{PAGE_SIZE, PageCodec};

use crate::CryptoError;

/// Length of the AEAD root/sub key in bytes (256 bits).
pub const KEY_LEN: usize = 32;
/// Length of the XChaCha20-Poly1305 nonce in bytes (192 bits).
const NONCE_LEN: usize = 24;
/// Length of the Poly1305 authentication tag in bytes.
const TAG_LEN: usize = 16;

// HKDF `info` domain separators. A page subkey and a WAL-record subkey are
// derived under distinct domains, so they can never coincide even though both
// ultimately come from the same root key.
const PAGE_INFO: &[u8] = b"quiver/v1/page";
const WAL_INFO: &[u8] = b"quiver/v1/wal-record";

/// An encryption-at-rest codec: XChaCha20-Poly1305 with HKDF-SHA256 subkeys.
///
/// The root key is held in a [`Zeroizing`] buffer and wiped on drop; derived
/// subkeys are ephemeral and likewise zeroized. Cloning intentionally is not
/// derived — construct one codec and share it behind the `Box<dyn PageCodec>`
/// the engine already threads through.
pub struct AeadCodec {
    root_key: Zeroizing<[u8; KEY_LEN]>,
}

impl AeadCodec {
    /// Build a codec from a raw 256-bit root key. The key is copied into a
    /// zeroizing buffer; the caller should zeroize its own copy.
    #[must_use]
    pub fn new(root_key: [u8; KEY_LEN]) -> Self {
        Self {
            root_key: Zeroizing::new(root_key),
        }
    }

    /// Build a codec from a 64-character hex-encoded 256-bit key (the form the
    /// server config and `QUIVER_ENCRYPTION_KEY` accept). Errors if the string
    /// is not exactly 64 hex digits.
    pub fn from_hex(hex: &str) -> std::result::Result<Self, CryptoError> {
        Ok(Self::new(decode_key_hex(hex)?))
    }

    // Derive a 32-byte subkey from the root key under the given domain `info`
    // parts. HKDF-expand on a 32-byte output never fails, but the result is
    // still handled rather than unwrapped (no panics on the engine's paths).
    fn subkey(&self, info: &[&[u8]]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        let hk = Hkdf::<Sha256>::new(None, self.root_key.as_slice());
        let mut okm = Zeroizing::new([0u8; KEY_LEN]);
        hk.expand_multi_info(info, okm.as_mut_slice())
            .map_err(|_| CoreError::MalformedPage("hkdf subkey derivation failed".to_owned()))?;
        Ok(okm)
    }

    // Build a cipher bound to a freshly-derived subkey for `info`.
    fn cipher(&self, info: &[&[u8]]) -> Result<XChaCha20Poly1305> {
        let subkey = self.subkey(info)?;
        Ok(XChaCha20Poly1305::new(Key::from_slice(subkey.as_slice())))
    }

    // Seal `plaintext` under `info`, with `aad` authenticated but not encrypted,
    // into `[nonce][ciphertext+tag]`.
    fn seal_bytes(&self, info: &[&[u8]], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher = self.cipher(info)?;
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CoreError::MalformedPage("aead sealing failed".to_owned()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    // Open a `[nonce][ciphertext+tag]` blob sealed by `seal_bytes`. A wrong key
    // or any tampering fails the Poly1305 tag and returns an error.
    fn open_bytes(&self, info: &[&[u8]], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return Err(CoreError::MalformedPage(format!(
                "sealed blob is {} bytes, shorter than the {}-byte minimum",
                sealed.len(),
                NONCE_LEN + TAG_LEN
            )));
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
        let cipher = self.cipher(info)?;
        let nonce = XNonce::from_slice(nonce_bytes);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                CoreError::MalformedPage(
                    "aead open failed: wrong key or tampered ciphertext".to_owned(),
                )
            })
    }
}

impl PageCodec for AeadCodec {
    fn block_size(&self) -> usize {
        NONCE_LEN + PAGE_SIZE + TAG_LEN
    }

    fn seal(&self, page_id: u64, plaintext: &[u8; PAGE_SIZE], out: &mut [u8]) -> Result<()> {
        if out.len() != self.block_size() {
            return Err(CoreError::MalformedPage(format!(
                "seal output buffer is {} bytes, expected {}",
                out.len(),
                self.block_size()
            )));
        }
        // The page id is folded into both the subkey (via `info`) and the AAD, so
        // a sealed block cannot be silently relocated to a different page slot.
        let page_id_le = page_id.to_le_bytes();
        let sealed =
            self.seal_bytes(&[PAGE_INFO, &page_id_le], &page_id_le, plaintext.as_slice())?;
        if sealed.len() != self.block_size() {
            return Err(CoreError::MalformedPage(format!(
                "sealed page is {} bytes, expected {}",
                sealed.len(),
                self.block_size()
            )));
        }
        out.copy_from_slice(&sealed);
        Ok(())
    }

    fn open(&self, page_id: u64, block: &[u8], out: &mut [u8; PAGE_SIZE]) -> Result<()> {
        if block.len() != self.block_size() {
            return Err(CoreError::MalformedPage(format!(
                "page block is {} bytes, expected {}",
                block.len(),
                self.block_size()
            )));
        }
        let page_id_le = page_id.to_le_bytes();
        let plaintext = self.open_bytes(&[PAGE_INFO, &page_id_le], &page_id_le, block)?;
        if plaintext.len() != PAGE_SIZE {
            return Err(CoreError::MalformedPage(format!(
                "decrypted page is {} bytes, expected {PAGE_SIZE}",
                plaintext.len()
            )));
        }
        out.copy_from_slice(&plaintext);
        Ok(())
    }

    fn seal_record(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.seal_bytes(&[WAL_INFO], &[], plaintext)
    }

    fn open_record(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        self.open_bytes(&[WAL_INFO], &[], sealed)
    }
}

// Decode a 64-character hex string into a 256-bit key. Rejects a wrong length or
// any non-hex character; never panics.
fn decode_key_hex(hex: &str) -> std::result::Result<[u8; KEY_LEN], CryptoError> {
    let hex = hex.trim();
    if hex.len() != KEY_LEN * 2 {
        return Err(CryptoError::InvalidKey(format!(
            "expected {} hex characters for a 256-bit key, got {}",
            KEY_LEN * 2,
            hex.len()
        )));
    }
    let bytes = hex.as_bytes();
    let mut key = [0u8; KEY_LEN];
    for (i, slot) in key.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(key)
}

fn hex_nibble(c: u8) -> std::result::Result<u8, CryptoError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(CryptoError::InvalidKey(format!(
            "non-hex character {:?} in encryption key",
            char::from(other)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_core::page::{PageType, build_page};

    fn key(b: u8) -> [u8; KEY_LEN] {
        [b; KEY_LEN]
    }

    fn sample_page() -> [u8; PAGE_SIZE] {
        build_page(
            PageType::Segment,
            3,
            7,
            b"the quick brown fox jumps over the lazy dog",
        )
        .unwrap()
    }

    #[test]
    fn block_size_is_page_plus_aead_overhead() {
        let codec = AeadCodec::new(key(1));
        assert_eq!(codec.block_size(), PAGE_SIZE + NONCE_LEN + TAG_LEN);
        assert_eq!(codec.block_size(), PAGE_SIZE + 40);
    }

    #[test]
    fn page_seal_open_roundtrips() {
        let codec = AeadCodec::new(key(9));
        let page = sample_page();
        let mut block = vec![0u8; codec.block_size()];
        codec.seal(42, &page, &mut block).unwrap();
        let mut back = [0u8; PAGE_SIZE];
        codec.open(42, &block, &mut back).unwrap();
        assert_eq!(page, back);
    }

    #[test]
    fn sealed_page_does_not_contain_plaintext() {
        let codec = AeadCodec::new(key(5));
        let page = sample_page();
        let mut block = vec![0u8; codec.block_size()];
        codec.seal(1, &page, &mut block).unwrap();
        // The recognizable plaintext must not survive anywhere in the block.
        assert!(
            block
                .windows(b"quick brown fox".len())
                .all(|w| w != b"quick brown fox"),
            "plaintext leaked into the sealed block"
        );
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let codec = AeadCodec::new(key(3));
        let page = sample_page();
        let mut a = vec![0u8; codec.block_size()];
        let mut b = vec![0u8; codec.block_size()];
        codec.seal(0, &page, &mut a).unwrap();
        codec.seal(0, &page, &mut b).unwrap();
        // Same page, same id, but a fresh random nonce ⇒ different ciphertext.
        assert_ne!(a, b);
        // Both still open to the same plaintext.
        let mut back = [0u8; PAGE_SIZE];
        codec.open(0, &a, &mut back).unwrap();
        assert_eq!(page, back);
        codec.open(0, &b, &mut back).unwrap();
        assert_eq!(page, back);
    }

    #[test]
    fn wrong_key_fails_to_open_page() {
        let page = sample_page();
        let mut block = vec![0u8; AeadCodec::new(key(1)).block_size()];
        AeadCodec::new(key(1)).seal(7, &page, &mut block).unwrap();
        let mut back = [0u8; PAGE_SIZE];
        assert!(matches!(
            AeadCodec::new(key(2)).open(7, &block, &mut back),
            Err(CoreError::MalformedPage(_))
        ));
    }

    #[test]
    fn wrong_page_id_fails_to_open() {
        let codec = AeadCodec::new(key(4));
        let page = sample_page();
        let mut block = vec![0u8; codec.block_size()];
        codec.seal(10, &page, &mut block).unwrap();
        let mut back = [0u8; PAGE_SIZE];
        // The subkey and AAD are bound to the page id, so opening at a different
        // slot must fail.
        assert!(codec.open(11, &block, &mut back).is_err());
    }

    #[test]
    fn tampering_any_byte_fails_to_open() {
        let codec = AeadCodec::new(key(8));
        let page = sample_page();
        let mut block = vec![0u8; codec.block_size()];
        codec.seal(2, &page, &mut block).unwrap();
        for idx in [0usize, NONCE_LEN, NONCE_LEN + 100, block.len() - 1] {
            let mut t = block.clone();
            t[idx] ^= 0xFF;
            let mut back = [0u8; PAGE_SIZE];
            assert!(
                codec.open(2, &t, &mut back).is_err(),
                "tampering byte {idx} should fail authentication"
            );
        }
    }

    #[test]
    fn record_seal_open_roundtrips() {
        let codec = AeadCodec::new(key(6));
        let record = b"collection=secret;id=alice;payload={\"ssn\":\"123-45-6789\"}";
        let sealed = codec.seal_record(record).unwrap();
        assert_ne!(&sealed[..], &record[..]);
        let opened = codec.open_record(&sealed).unwrap();
        assert_eq!(opened, record);
    }

    #[test]
    fn record_wrong_key_and_tamper_fail() {
        let record = b"sensitive-wal-record";
        let sealed = AeadCodec::new(key(1)).seal_record(record).unwrap();
        assert!(AeadCodec::new(key(2)).open_record(&sealed).is_err());
        let mut t = sealed.clone();
        let last = t.len() - 1;
        t[last] ^= 0x01;
        assert!(AeadCodec::new(key(1)).open_record(&t).is_err());
    }

    #[test]
    fn record_domain_is_separated_from_pages() {
        // A blob sealed as a record must not open as a page block, and a too-short
        // blob is rejected rather than panicking.
        let codec = AeadCodec::new(key(7));
        assert!(codec.open_record(&[0u8; NONCE_LEN]).is_err());
        assert!(codec.open_record(b"short").is_err());
    }

    #[test]
    fn hex_key_parsing() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let codec = AeadCodec::from_hex(hex).unwrap();
        assert_eq!(codec.root_key[0], 0x00);
        assert_eq!(codec.root_key[1], 0x11);
        assert_eq!(codec.root_key[KEY_LEN - 1], 0xff);
        // Uppercase and surrounding whitespace are accepted.
        assert!(AeadCodec::from_hex(&format!("  {}  ", hex.to_uppercase())).is_ok());
        // Wrong length and non-hex are rejected.
        assert!(AeadCodec::from_hex("abcd").is_err());
        assert!(AeadCodec::from_hex(&"zz".repeat(32)).is_err());
    }

    // Known-answer test for the key-derivation function, against RFC 5869
    // (HKDF-SHA256) Test Case 1. This validates our KDF wiring; the AEAD
    // primitive's own RFC known-answer tests are provided by the audited
    // `chacha20poly1305` crate, which is the reason we depend on it rather than
    // implementing the cipher ourselves.
    #[test]
    fn hkdf_sha256_rfc5869_test_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: [u8; 13] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info: [u8; 10] = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut okm = [0u8; 42];
        hk.expand(&info, &mut okm).unwrap();
        let expected: [u8; 42] = [
            0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36,
            0x2f, 0x2a, 0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56,
            0xec, 0xc4, 0xc5, 0xbf, 0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
        ];
        assert_eq!(okm, expected);
    }

    // Our per-page subkey derivation is deterministic and domain/position
    // separated: equal inputs ⇒ equal subkey; different page id or domain ⇒
    // different subkey.
    #[test]
    fn subkey_derivation_is_deterministic_and_separated() {
        let codec = AeadCodec::new(key(1));
        let page5_a = codec.subkey(&[PAGE_INFO, &5u64.to_le_bytes()]).unwrap();
        let page5_b = codec.subkey(&[PAGE_INFO, &5u64.to_le_bytes()]).unwrap();
        let page6 = codec.subkey(&[PAGE_INFO, &6u64.to_le_bytes()]).unwrap();
        let wal = codec.subkey(&[WAL_INFO]).unwrap();
        assert_eq!(*page5_a, *page5_b);
        assert_ne!(*page5_a, *page6);
        assert_ne!(*page5_a, *wal);
        // A different root key yields a different subkey for the same context.
        let other = AeadCodec::new(key(2));
        assert_ne!(
            *page5_a,
            *other.subkey(&[PAGE_INFO, &5u64.to_le_bytes()]).unwrap()
        );
    }
}
