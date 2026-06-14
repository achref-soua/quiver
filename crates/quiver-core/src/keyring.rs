// SPDX-License-Identifier: AGPL-3.0-only
//! The key-supply seam between the storage engine and `quiver-crypto`.
//!
//! A [`KeyRing`] tells the [`Store`](crate::Store) which [`PageCodec`] seals
//! which bytes: one **catalog** codec for the engine-wide structures (the
//! manifest and the write-ahead log), and a **per-collection** codec for each
//! collection's segments and index artifacts. Splitting the codec by collection
//! is what makes **crypto-shredding** possible (ADR-0010): when a collection's
//! data is sealed under its own data-encryption key (DEK), destroying that one
//! small key renders the collection's durable bytes unrecoverable even if the
//! ciphertext survives in a backup.
//!
//! This module defines the seam and the trivial [`SingleCodecKeyRing`], which
//! preserves the pre-envelope behaviour — one codec for everything, either the
//! plaintext [`PlainCodec`] when encryption-at-rest is off or a single AEAD codec
//! when a key is configured without the per-collection envelope. The envelope
//! key-ring that wraps per-collection DEKs under a master key lives in
//! `quiver-crypto`, so the engine itself stays free of key management.

use crate::error::Result;
use crate::ids::CollectionId;
use crate::page::{PageCodec, PlainCodec};

/// Supplies the page codecs the storage engine seals data with, and manages the
/// per-collection key lifecycle that crypto-shredding relies on.
///
/// Implementations are shared for the lifetime of a [`Store`](crate::Store), so
/// they must be `Send + Sync`.
pub trait KeyRing: Send + Sync {
    /// The codec for engine-wide structures: the manifest and the write-ahead
    /// log.
    fn catalog_codec(&self) -> &dyn PageCodec;

    /// The codec for one collection's segments and index artifacts.
    ///
    /// # Errors
    /// Fails if the collection's key material is unavailable — for an envelope
    /// key-ring that means it was crypto-shredded, so the data is intentionally
    /// unrecoverable.
    fn collection_codec(&self, collection: CollectionId) -> Result<Box<dyn PageCodec>>;

    /// Provision key material for a new collection. Idempotent, and a no-op for
    /// key-rings without per-collection keys.
    ///
    /// # Errors
    /// Fails if key material cannot be generated or persisted.
    fn provision_collection(&self, collection: CollectionId) -> Result<()>;

    /// Crypto-shred a collection: destroy its key material so its sealed data can
    /// never be decrypted again. A no-op for key-rings without per-collection
    /// keys, where reclaiming the files is the only erasure.
    ///
    /// # Errors
    /// Fails if the key material cannot be destroyed.
    fn shred_collection(&self, collection: CollectionId) -> Result<()>;
}

/// A [`KeyRing`] that seals everything — catalog and every collection — with one
/// shared codec.
///
/// This is the pre-envelope behaviour: [`PlainCodec`] when encryption-at-rest is
/// disabled, or a single AEAD codec when a key is configured without the
/// per-collection envelope. It holds no per-collection keys, so `provision` and
/// `shred` are no-ops — a dropped collection is erased only by reclaiming its
/// files.
pub struct SingleCodecKeyRing {
    codec: Box<dyn PageCodec>,
}

impl SingleCodecKeyRing {
    /// Wrap a single codec as a key-ring.
    #[must_use]
    pub fn new(codec: Box<dyn PageCodec>) -> Self {
        Self { codec }
    }

    /// A plaintext key-ring — encryption-at-rest disabled.
    #[must_use]
    pub fn plaintext() -> Self {
        Self::new(Box::new(PlainCodec))
    }
}

impl KeyRing for SingleCodecKeyRing {
    fn catalog_codec(&self) -> &dyn PageCodec {
        self.codec.as_ref()
    }

    fn collection_codec(&self, _collection: CollectionId) -> Result<Box<dyn PageCodec>> {
        Ok(self.codec.clone_box())
    }

    fn provision_collection(&self, _collection: CollectionId) -> Result<()> {
        Ok(())
    }

    fn shred_collection(&self, _collection: CollectionId) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PAGE_SIZE;

    #[test]
    fn single_codec_keyring_shares_one_codec() {
        let kr = SingleCodecKeyRing::plaintext();
        // Catalog and every collection resolve to a codec with the same block
        // size (the plaintext identity codec here).
        assert_eq!(kr.catalog_codec().block_size(), PAGE_SIZE);
        let c0 = kr.collection_codec(CollectionId(0)).unwrap();
        let c1 = kr.collection_codec(CollectionId(1)).unwrap();
        assert_eq!(c0.block_size(), PAGE_SIZE);
        assert_eq!(c1.block_size(), PAGE_SIZE);
    }

    #[test]
    fn single_codec_keyring_provision_and_shred_are_noops() {
        let kr = SingleCodecKeyRing::plaintext();
        // No per-collection keys: provisioning and shredding always succeed and
        // leave the codec available.
        kr.provision_collection(CollectionId(7)).unwrap();
        kr.shred_collection(CollectionId(7)).unwrap();
        assert!(kr.collection_codec(CollectionId(7)).is_ok());
    }
}
