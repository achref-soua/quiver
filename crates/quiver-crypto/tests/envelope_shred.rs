// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end crypto-shredding through the real engine (ADR-0010): a collection
//! encrypted under an envelope key-ring is sealed to disk, then shredding
//! destroys its wrapped data-encryption key so the data is unrecoverable and the
//! collection is gone after reopening with the same master key.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use quiver_crypto::EnvelopeKeyRing;
use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype};

const MK: [u8; 32] = [0x42; 32];

fn open(dir: &Path) -> Database {
    let keyring = EnvelopeKeyRing::open(dir, MK).expect("open envelope key-ring");
    Database::open_with_keyring(dir, Box::new(keyring)).expect("open database")
}

// The wrapped per-collection DEK files under `<dir>/keys/`.
fn dek_files(dir: &Path) -> Vec<PathBuf> {
    let keys = dir.join("keys");
    if !keys.is_dir() {
        return Vec::new();
    }
    std::fs::read_dir(&keys)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dek"))
        .collect()
}

#[test]
fn shredding_a_collection_destroys_its_key_and_data() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut db = open(tmp.path());
        db.create_collection("secret", Descriptor::new(4, Dtype::F32, DistanceMetric::L2))
            .unwrap();
        db.upsert(
            "secret",
            "a",
            &[1.0, 2.0, 3.0, 4.0],
            &serde_json::json!({"pii": "ssn-078-05-1120"}),
        )
        .unwrap();
        // Seal the row into a DEK-protected segment on disk.
        db.checkpoint().unwrap();
        assert!(db.get("secret", "a").unwrap().is_some());

        // The collection now has exactly one wrapped DEK on disk.
        assert_eq!(
            dek_files(tmp.path()).len(),
            1,
            "the encrypted collection has a wrapped DEK"
        );

        // Crypto-shred it.
        assert!(db.shred_collection("secret").unwrap());
        // The collection is gone; reads fail rather than return data.
        assert!(db.get("secret", "a").is_err());
    }

    // The wrapped DEK was destroyed, so the sealed segment can never be decrypted
    // again — even by a holder of the master key.
    assert!(
        dek_files(tmp.path()).is_empty(),
        "shred must destroy the collection's wrapped DEK"
    );

    // Reopening with the same master key confirms the collection is gone.
    let db = open(tmp.path());
    assert!(db.collection_names().is_empty());
}

#[test]
fn dropped_collection_key_is_reclaimed_at_checkpoint() {
    // A plain drop (not an explicit shred) still crypto-shreds the DEK once a
    // checkpoint rewrites the manifest without the collection and GC runs.
    let tmp = tempfile::tempdir().unwrap();
    let mut db = open(tmp.path());
    db.create_collection("c", Descriptor::new(4, Dtype::F32, DistanceMetric::L2))
        .unwrap();
    db.upsert("c", "a", &[1.0; 4], &serde_json::json!({}))
        .unwrap();
    db.checkpoint().unwrap();
    assert_eq!(dek_files(tmp.path()).len(), 1);

    db.drop_collection("c").unwrap();
    db.checkpoint().unwrap(); // GC reclaims the dropped collection's files + DEK
    assert!(
        dek_files(tmp.path()).is_empty(),
        "the dropped collection's DEK is reclaimed at the next checkpoint"
    );
}
