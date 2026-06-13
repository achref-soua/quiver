// SPDX-License-Identifier: AGPL-3.0-only
//! Encryption-at-rest end-to-end: prove the engine writes no plaintext user data
//! to disk — **including the record-framed WAL**, which a page-only codec would
//! leave in the clear — and that the data round-trips under the right key while a
//! wrong key is rejected.
//!
//! These are integration-test helpers (not `#[test]` fns), so the crate's
//! `clippy.toml` unwrap/expect allowance does not reach them; opt in explicitly
//! (ADR-0017 scopes the ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use quiver_core::{Descriptor, DistanceMetric, Dtype, Store};
use quiver_crypto::AeadCodec;

// A recognizable marker placed in the external id and the payload. Both are
// serialized — alongside the vector bytes and the descriptor — into the same
// AEAD-sealed page/record blob, so the marker's absence on disk proves the whole
// blob is sealed, not just one field.
const NEEDLE: &str = "TOPSECRET_NEEDLE_7f3a9c2b";

fn desc() -> Descriptor {
    Descriptor::new(4, Dtype::F32, DistanceMetric::L2)
}

fn key(b: u8) -> [u8; 32] {
    [b; 32]
}

// Recursively read every file under `root` and report whether any contains the
// needle bytes.
fn tree_contains(root: &Path, needle: &[u8]) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        } else if meta.is_file() {
            let bytes = fs::read(&path).unwrap();
            if bytes.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        }
    }
    false
}

#[test]
fn encrypted_store_writes_no_plaintext_to_disk_including_the_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let needle = NEEDLE.as_bytes();

    {
        let mut store = Store::open_with_codec(dir, Box::new(AeadCodec::new(key(1)))).unwrap();
        let c = store.create_collection("vault", desc()).unwrap();
        let payload = format!(r#"{{"secret":"{NEEDLE}"}}"#);
        store
            .upsert(c, NEEDLE, &[1.0, 2.0, 3.0, 4.0], payload.as_bytes())
            .unwrap();

        // Before any checkpoint the only durable copy lives in the WAL. This is
        // the gap a page-only codec would miss: assert the WAL holds no plaintext.
        let wal_dir = dir.join("wal");
        assert!(wal_dir.is_dir(), "wal directory should exist");
        assert!(
            !tree_contains(&wal_dir, needle),
            "plaintext leaked into the WAL — record payloads are not encrypted"
        );

        // Seal into a segment, then write more that stays only in the rotated WAL.
        store.checkpoint().unwrap();
        let payload2 = format!(r#"{{"secret":"{NEEDLE}","n":2}}"#);
        store
            .upsert(
                c,
                &format!("{NEEDLE}-2"),
                &[5.0, 6.0, 7.0, 8.0],
                payload2.as_bytes(),
            )
            .unwrap();
    }

    // The whole data directory — segments, manifest, and WAL — must be ciphertext.
    assert!(
        !tree_contains(dir, needle),
        "plaintext leaked somewhere under the data directory"
    );

    // The right key recovers everything (the segment row and the WAL-only row).
    {
        let store = Store::open_with_codec(dir, Box::new(AeadCodec::new(key(1)))).unwrap();
        let c = store.collection_id("vault").expect("collection recovered");
        assert_eq!(store.len(c).unwrap(), 2);
        let got = store.get(c, NEEDLE).unwrap().unwrap();
        assert_eq!(got.vector, vec![1.0, 2.0, 3.0, 4.0]);
        assert!(
            String::from_utf8_lossy(&got.payload).contains(NEEDLE),
            "decrypted payload should contain the marker"
        );
        let got2 = store.get(c, &format!("{NEEDLE}-2")).unwrap().unwrap();
        assert_eq!(got2.vector, vec![5.0, 6.0, 7.0, 8.0]);
    }

    // The wrong key cannot open the store: the AEAD tag fails on the segment or
    // the WAL record, surfacing as a hard error rather than corrupt data.
    assert!(
        Store::open_with_codec(dir, Box::new(AeadCodec::new(key(2)))).is_err(),
        "a wrong key must fail to open the encrypted store"
    );
}

#[test]
fn plaintext_store_does_leak_the_needle() {
    // Positive control: without encryption the same marker is plainly visible on
    // disk, proving the scanner above can actually detect plaintext.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut store = Store::open(dir).unwrap();
    let c = store.create_collection("plain", desc()).unwrap();
    let payload = format!(r#"{{"secret":"{NEEDLE}"}}"#);
    store
        .upsert(c, NEEDLE, &[1.0, 2.0, 3.0, 4.0], payload.as_bytes())
        .unwrap();
    store.checkpoint().unwrap();
    assert!(
        tree_contains(dir, NEEDLE.as_bytes()),
        "unencrypted store should contain the plaintext marker on disk"
    );
}
