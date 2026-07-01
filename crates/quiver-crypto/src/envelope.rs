// SPDX-License-Identifier: AGPL-3.0-only
//! Envelope key management for crypto-shredding (ADR-0010).
//!
//! [`EnvelopeKeyRing`] implements the engine's [`KeyRing`] seam with a two-level
//! key hierarchy: one operator-supplied **master key** (MK) protects every
//! collection's randomly-generated **data-encryption key** (DEK). A collection's
//! segments and index artifacts are sealed under its own DEK; the catalog (the
//! manifest and the write-ahead log) is sealed under a separate key derived from
//! the MK.
//!
//! DEKs are stored **wrapped** — AEAD-encrypted under the MK and bound to the
//! collection id — as small files under `<data_dir>/keys/`. **Crypto-shredding**
//! a collection deletes its wrapped DEK: the DEK existed nowhere else, so once
//! the file is gone the collection's sealed bytes can never be decrypted again,
//! even by the master-key holder and even if the ciphertext survives in a
//! backup. This erases a collection's durable data without overwriting every
//! byte of it — the key-destruction ("right to erasure") pattern.
//!
//! The master key never touches disk. Only audited primitives are used
//! (XChaCha20-Poly1305 and HKDF-SHA256), exactly as for [`AeadCodec`].
//!
//! ## Scope of erasure
//! Crypto-shredding covers a collection's durable **segments and index** — the
//! bulk store. Recent un-checkpointed writes live in the catalog-keyed WAL;
//! [`Store::shred_collection`](quiver_core::Store::shred_collection) first
//! checkpoints (sealing them into DEK-protected segments) and rotates the WAL,
//! so after it returns no live key can decrypt the collection's durable data.
//! See `docs/security/crypto.md`.

use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use quiver_core::error::{CoreError, Result};
use quiver_core::ids::CollectionId;
use quiver_core::keyring::KeyRing;
use quiver_core::page::PageCodec;

use crate::CryptoError;
use crate::codec::{AeadCodec, KEY_LEN, decode_key_hex};

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

// HKDF domain separators over the master key: the catalog codec's root key and
// the DEK-wrapping key are distinct, so neither can stand in for the other.
const CATALOG_INFO: &[u8] = b"quiver/v1/catalog";
const WRAP_INFO: &[u8] = b"quiver/v1/dek-wrap";
// AAD prefix binding a wrapped DEK to its collection id, so a wrapped DEK cannot
// be relocated to a different collection.
const DEK_AAD_PREFIX: &[u8] = b"quiver/v1/dek";

/// A two-level envelope key-ring: a master key wrapping per-collection
/// data-encryption keys, enabling crypto-shredding (ADR-0010).
///
/// Construct one with [`EnvelopeKeyRing::open`] (or [`EnvelopeKeyRing::from_hex`])
/// and hand it to [`Store::open_with_keyring`](quiver_core::Store::open_with_keyring)
/// or `Database::open_with_keyring`.
pub struct EnvelopeKeyRing {
    master: Zeroizing<[u8; KEY_LEN]>,
    catalog: AeadCodec,
    keys_dir: PathBuf,
}

impl EnvelopeKeyRing {
    /// Open an envelope key-ring rooted at `data_dir` under a 256-bit master
    /// `key`. Wrapped DEKs live in `<data_dir>/keys/`, created if absent.
    ///
    /// # Errors
    /// Fails if the keys directory cannot be created or a key cannot be derived.
    pub fn open(data_dir: &Path, key: [u8; KEY_LEN]) -> Result<Self> {
        let keys_dir = data_dir.join("keys");
        fs::create_dir_all(&keys_dir).map_err(|e| CoreError::io(&keys_dir, e))?;
        let catalog_key = derive(&key, CATALOG_INFO)?;
        let catalog = AeadCodec::new(*catalog_key);
        Ok(Self {
            master: Zeroizing::new(key),
            catalog,
            keys_dir,
        })
    }

    /// Open an envelope key-ring from a 64-character hex master key (the
    /// `QUIVER_ENCRYPTION_KEY` form).
    ///
    /// # Errors
    /// [`CryptoError::InvalidKey`] if the hex is malformed or the keys directory
    /// cannot be prepared.
    pub fn from_hex(hex: &str, data_dir: &Path) -> std::result::Result<Self, CryptoError> {
        let key = decode_key_hex(hex)?;
        Self::open(data_dir, key).map_err(|e| CryptoError::InvalidKey(e.to_string()))
    }

    fn dek_path(&self, id: CollectionId) -> PathBuf {
        self.keys_dir.join(format!("{:010}.dek", id.value()))
    }

    // Wrap a DEK under the master key, bound to the collection id (AAD), as
    // `[nonce][ciphertext+tag]`.
    fn wrap(&self, id: CollectionId, dek: &[u8; KEY_LEN]) -> Result<Vec<u8>> {
        let wrap_key = derive(self.master.as_slice(), WRAP_INFO)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(wrap_key.as_slice()));
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = dek_aad(id);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: dek.as_slice(),
                    aad: &aad,
                },
            )
            .map_err(|_| CoreError::MalformedPage("dek wrap failed".to_owned()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    // Unwrap a DEK produced by `wrap`. A wrong master key or any tampering fails
    // the Poly1305 tag.
    fn unwrap(&self, id: CollectionId, wrapped: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        if wrapped.len() != NONCE_LEN + KEY_LEN + TAG_LEN {
            return Err(CoreError::MalformedPage(format!(
                "wrapped dek is {} bytes, expected {}",
                wrapped.len(),
                NONCE_LEN + KEY_LEN + TAG_LEN
            )));
        }
        let wrap_key = derive(self.master.as_slice(), WRAP_INFO)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(wrap_key.as_slice()));
        let (nonce, ciphertext) = wrapped.split_at(NONCE_LEN);
        let aad = dek_aad(id);
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                CoreError::MalformedPage(
                    "dek unwrap failed: wrong master key or tampered key file".to_owned(),
                )
            })?;
        // The AEAD returns the DEK plaintext in a plain Vec; wrap it so the
        // transient copy is wiped on drop rather than lingering in freed heap —
        // this is the exact secret crypto-shredding relies on.
        let plaintext = Zeroizing::new(plaintext);
        let mut dek = Zeroizing::new([0u8; KEY_LEN]);
        dek.copy_from_slice(&plaintext);
        Ok(dek)
    }
}

impl KeyRing for EnvelopeKeyRing {
    fn catalog_codec(&self) -> &dyn PageCodec {
        &self.catalog
    }

    fn collection_codec(&self, collection: CollectionId) -> Result<Box<dyn PageCodec>> {
        let path = self.dek_path(collection);
        let wrapped = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CoreError::NotFound(format!(
                "the data-encryption key for collection {collection} is gone \
                 (crypto-shredded, or a store predating the per-collection envelope)"
            )),
            _ => CoreError::io(&path, e),
        })?;
        let dek = self.unwrap(collection, &wrapped)?;
        Ok(Box::new(AeadCodec::new(*dek)))
    }

    fn provision_collection(&self, collection: CollectionId) -> Result<()> {
        let path = self.dek_path(collection);
        if path.exists() {
            return Ok(()); // idempotent: a DEK is provisioned exactly once
        }
        // Fresh random 256-bit DEK from the OS CSPRNG.
        let generated = XChaCha20Poly1305::generate_key(&mut OsRng);
        let mut dek = Zeroizing::new([0u8; KEY_LEN]);
        dek.copy_from_slice(generated.as_slice());
        let wrapped = self.wrap(collection, &dek)?;
        // Write atomically: a temp file fsync'd, then renamed, then the directory
        // fsync'd — so a crash can never leave a torn DEK.
        let tmp = self
            .keys_dir
            .join(format!("{:010}.dek.tmp", collection.value()));
        write_sync(&tmp, &wrapped)?;
        fs::rename(&tmp, &path).map_err(|e| CoreError::io(&path, e))?;
        fsync_dir(&self.keys_dir)
    }

    fn shred_collection(&self, collection: CollectionId) -> Result<()> {
        let path = self.dek_path(collection);
        match fs::remove_file(&path) {
            Ok(()) => fsync_dir(&self.keys_dir),
            // Already gone (e.g. shredded twice) — erasure is idempotent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::io(&path, e)),
        }
    }
}

// Derive a 256-bit subkey from the master key under a domain `info`. HKDF-expand
// of 32 bytes cannot fail in practice, but the result is handled, never
// unwrapped, to keep panics off the engine's paths.
fn derive(ikm: &[u8], info: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut okm = Zeroizing::new([0u8; KEY_LEN]);
    hk.expand(info, okm.as_mut_slice())
        .map_err(|_| CoreError::MalformedPage("hkdf key derivation failed".to_owned()))?;
    Ok(okm)
}

// The AAD binding a wrapped DEK to its collection id.
fn dek_aad(id: CollectionId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(DEK_AAD_PREFIX.len() + 8);
    aad.extend_from_slice(DEK_AAD_PREFIX);
    aad.extend_from_slice(&id.value().to_le_bytes());
    aad
}

// Write `bytes` to `path` and fsync the file before returning.
fn write_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = fs::File::create(path).map_err(|e| CoreError::io(path, e))?;
    f.write_all(bytes).map_err(|e| CoreError::io(path, e))?;
    f.sync_all().map_err(|e| CoreError::io(path, e))
}

// fsync a directory so a create/rename/unlink within it is durable.
fn fsync_dir(dir: &Path) -> Result<()> {
    let f = fs::File::open(dir).map_err(|e| CoreError::io(dir, e))?;
    f.sync_all().map_err(|e| CoreError::io(dir, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_core::page::{PAGE_SIZE, PageType, build_page};

    const MK: [u8; KEY_LEN] = [7u8; KEY_LEN];

    fn page() -> [u8; PAGE_SIZE] {
        build_page(PageType::Segment, 1, 1, b"top-secret-vector-bytes").unwrap()
    }

    #[test]
    fn catalog_and_collection_codecs_use_distinct_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let kr = EnvelopeKeyRing::open(tmp.path(), MK).unwrap();
        kr.provision_collection(CollectionId(1)).unwrap();
        let cat = kr.catalog_codec();
        let col = kr.collection_codec(CollectionId(1)).unwrap();

        // The same page sealed under each codec differs, and neither opens the
        // other's block — the catalog key and the DEK are independent.
        let mut cat_block = vec![0u8; cat.block_size()];
        cat.seal(1, &page(), &mut cat_block).unwrap();
        let mut back = [0u8; PAGE_SIZE];
        assert!(col.open(1, &cat_block, &mut back).is_err());
    }

    #[test]
    fn two_collections_get_independent_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let kr = EnvelopeKeyRing::open(tmp.path(), MK).unwrap();
        kr.provision_collection(CollectionId(1)).unwrap();
        kr.provision_collection(CollectionId(2)).unwrap();
        let c1 = kr.collection_codec(CollectionId(1)).unwrap();
        let c2 = kr.collection_codec(CollectionId(2)).unwrap();
        let mut b1 = vec![0u8; c1.block_size()];
        c1.seal(1, &page(), &mut b1).unwrap();
        // Collection 2's codec cannot open collection 1's block.
        let mut back = [0u8; PAGE_SIZE];
        assert!(c2.open(1, &b1, &mut back).is_err());
    }

    #[test]
    fn provision_is_idempotent_and_keeps_the_same_dek() {
        let tmp = tempfile::tempdir().unwrap();
        let kr = EnvelopeKeyRing::open(tmp.path(), MK).unwrap();
        kr.provision_collection(CollectionId(1)).unwrap();
        let c1 = kr.collection_codec(CollectionId(1)).unwrap();
        let mut block = vec![0u8; c1.block_size()];
        c1.seal(1, &page(), &mut block).unwrap();
        // Provisioning again must not rotate the DEK, or sealed data would break.
        kr.provision_collection(CollectionId(1)).unwrap();
        let mut back = [0u8; PAGE_SIZE];
        kr.collection_codec(CollectionId(1))
            .unwrap()
            .open(1, &block, &mut back)
            .unwrap();
        assert_eq!(back, page());
    }

    #[test]
    fn shredding_makes_a_collection_unrecoverable_even_with_the_master_key() {
        let tmp = tempfile::tempdir().unwrap();
        let kr = EnvelopeKeyRing::open(tmp.path(), MK).unwrap();
        let cid = CollectionId(3);
        kr.provision_collection(cid).unwrap();

        // Seal a page with the collection's DEK; this `block` stands in for the
        // collection's ciphertext that might survive in a backup.
        let codec = kr.collection_codec(cid).unwrap();
        let mut block = vec![0u8; codec.block_size()];
        codec.seal(1, &page(), &mut block).unwrap();
        // The key holder can open it before shredding.
        let mut back = [0u8; PAGE_SIZE];
        codec.open(1, &block, &mut back).unwrap();
        assert_eq!(back, page());

        // Crypto-shred: destroy the wrapped DEK.
        kr.shred_collection(cid).unwrap();

        // A fresh key-ring with the SAME master key can no longer derive the
        // collection's codec — the ciphertext is permanently unrecoverable.
        let reopened = EnvelopeKeyRing::open(tmp.path(), MK).unwrap();
        assert!(reopened.collection_codec(cid).is_err());
        // Shredding again is a harmless no-op.
        reopened.shred_collection(cid).unwrap();
    }

    #[test]
    fn a_wrong_master_key_cannot_unwrap_a_dek() {
        let tmp = tempfile::tempdir().unwrap();
        EnvelopeKeyRing::open(tmp.path(), MK)
            .unwrap()
            .provision_collection(CollectionId(1))
            .unwrap();
        // A different master key over the same directory fails to unwrap.
        let wrong = EnvelopeKeyRing::open(tmp.path(), [9u8; KEY_LEN]).unwrap();
        assert!(wrong.collection_codec(CollectionId(1)).is_err());
    }
}
