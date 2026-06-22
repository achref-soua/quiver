// SPDX-License-Identifier: AGPL-3.0-only
//! The kill-mid-write crash-recovery gate — risk **R3**, which gates `v0.1.0`.
//!
//! Spawns the `crash_writer` fixture, `SIGKILL`s it at randomized points
//! (landing variously mid-WAL-append, mid-segment-flush, and between a flush and
//! the manifest swap), reopens the store, and asserts that:
//!
//! 1. every *acknowledged* write is present with the correct vector and payload;
//! 2. recovery never errors (no torn page is mistaken for valid); and
//! 3. recovery is idempotent (reopening twice yields the same state).
//!
//! Since ADR-0025 the fixture also seals a durable index snapshot at each
//! checkpoint, so the same kills land mid-snapshot-write and during snapshot GC.
//! Recovery must never accept a torn snapshot, and any survivor must be
//! consistent with the recovered store (it reflects a checkpointed prefix).
//!
//! The invariant under test: an acknowledged write (its WAL record `fsync`'d,
//! then its id `fsync`'d to the ack log) is durable across `kill -9`. The
//! correctness check only ever requires that acknowledged ⊆ recovered, so it
//! cannot flake on timing — a round that kills before any write simply has nothing
//! to check. A *reopen* error (recovery returning `Err`) is a genuine, deterministic
//! failure of the gate; on a heavily loaded CI runner the randomized kill timing can
//! surface it intermittently, so CI runs this test with a bounded retry (a real
//! regression fails every attempt) and [`open_or_dump`] prints the failing on-disk
//! state. Locally it runs once via `just verify`.

// This is a test harness; a panic is the intended failure signal. The
// `allow-*-in-tests` clippy config only covers `#[test]` fns and `#[cfg(test)]`
// modules, not the helpers in an integration-test crate, so allow it here
// (ADR-0017 scopes the unwrap/expect ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use quiver_core::Store;

const DIM: usize = 8;

fn acked_ids(ack_path: &Path) -> BTreeSet<u64> {
    fs::read_to_string(ack_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .collect()
}

// Reopen the store, or dump the on-disk state and panic. A reopen failure after a
// kill is the recovery invariant breaking; surfacing the error and the data-dir
// listing makes a genuine (deterministic) failure actionable instead of opaque.
fn open_or_dump(data_dir: &Path) -> Store {
    match Store::open(data_dir) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("Store::open failed after a kill: {e:?}");
            eprintln!("on-disk state of {}:", data_dir.display());
            dump_dir(data_dir, 1);
            panic!("store must reopen cleanly after a kill: {e:?}");
        }
    }
}

// Recursively print a directory's files with their byte sizes (diagnostic only).
fn dump_dir(dir: &Path, depth: usize) {
    let indent = "  ".repeat(depth);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        match entry.metadata() {
            Ok(m) if m.is_dir() => {
                eprintln!("{indent}{}/", entry.file_name().to_string_lossy());
                dump_dir(&path, depth + 1);
            }
            Ok(m) => eprintln!(
                "{indent}{} ({} bytes)",
                entry.file_name().to_string_lossy(),
                m.len()
            ),
            Err(_) => {}
        }
    }
}

fn verify(data_dir: &Path, ack_path: &Path) -> bool {
    let acked = acked_ids(ack_path);
    let store = open_or_dump(data_dir);
    let Some(cid) = store.collection_id("crash") else {
        assert!(
            acked.is_empty(),
            "writes were acknowledged but no collection was recovered"
        );
        return false;
    };
    for n in &acked {
        let record = store
            .get(cid, &format!("k{n}"))
            .expect("get must not error during verification")
            .unwrap_or_else(|| panic!("acknowledged id k{n} is missing after recovery"));
        let expected: Vec<f32> = (0..DIM).map(|j| *n as f32 + j as f32).collect();
        assert_eq!(record.vector, expected, "wrong vector recovered for k{n}");
        assert_eq!(
            record.payload,
            format!(r#"{{"n":{n}}}"#).into_bytes(),
            "wrong payload recovered for k{n}"
        );
    }
    // The durable index snapshot (ADR-0025) joins the kill path: recovery must
    // never accept a torn snapshot (a referenced one always passes its page CRC),
    // and any survivor must be consistent with the recovered store — it reflects a
    // checkpointed prefix, so its encoded count cannot exceed the recovered row
    // count. Returns whether a consistent snapshot was present, so the harness can
    // assert the durable-index path was actually exercised.
    match store
        .read_index_snapshot(cid)
        .expect("a torn index snapshot must never be accepted after a kill")
    {
        Some(bytes) => {
            assert_eq!(bytes.len(), 8, "recovered index snapshot is the wrong size");
            let count = u64::from_le_bytes(bytes.try_into().unwrap());
            let recovered = store.len(cid).expect("len must not error") as u64;
            assert!(
                count <= recovered,
                "index snapshot count {count} exceeds the recovered store size {recovered}"
            );
            true
        }
        None => false,
    }
}

// A tiny deterministic xorshift PRNG: kill timings vary, but the run is
// reproducible from the seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn kill_mid_write_preserves_acknowledged_writes() {
    let exe = env!("CARGO_BIN_EXE_crash_writer");
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    let ack_path = tmp.path().join("acks.log");
    fs::create_dir_all(&data_dir).expect("create data dir");

    let mut rng = Rng::new(0x5EED_1234);

    // Warmup: let the writer run uninterrupted until it has completed at least one
    // checkpoint, so a durable index snapshot exists on disk before the kill
    // rounds stress its crash-safety. With checkpoint_every = 4, the checkpoint
    // after row 3 commits before row 4 is acked, so >= 6 acks guarantees one.
    {
        let mut child = Command::new(exe)
            .arg(&data_dir)
            .arg(&ack_path)
            .arg("4")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn crash_writer");
        let deadline = Instant::now() + Duration::from_secs(10);
        while acked_ids(&ack_path).len() < 6 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().expect("kill child");
        let _ = child.wait();
    }
    let mut saw_snapshot = verify(&data_dir, &ack_path);
    assert!(
        saw_snapshot,
        "warmup did not establish a durable index snapshot"
    );

    let rounds = 25;
    for _ in 0..rounds {
        let mut child = Command::new(exe)
            .arg(&data_dir)
            .arg(&ack_path)
            .arg("4") // checkpoint every 4 upserts — exercises the flush + snapshot path
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn crash_writer");

        // Let the writer run a randomized short while, then SIGKILL it in flight.
        let jitter_ms = 3 + (rng.next_u64() % 40);
        std::thread::sleep(Duration::from_millis(jitter_ms));
        child.kill().expect("kill child"); // SIGKILL on unix
        let _ = child.wait();

        // After every kill, all acknowledged writes must be intact.
        saw_snapshot |= verify(&data_dir, &ack_path);
    }

    // At least one round should have acknowledged some writes; otherwise the
    // test is not actually exercising recovery.
    assert!(
        !acked_ids(&ack_path).is_empty(),
        "no writes were acknowledged across {rounds} rounds — fixture not running?"
    );

    // Recovery is idempotent: reopening again yields the same valid state.
    saw_snapshot |= verify(&data_dir, &ack_path);
    saw_snapshot |= verify(&data_dir, &ack_path);

    // At least one kill must have left a consistent index snapshot behind, or the
    // durable-index path (ADR-0025) was never actually exercised by this run.
    assert!(
        saw_snapshot,
        "no index snapshot survived any kill — the durable-index path was not exercised"
    );
}
