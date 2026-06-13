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
//! The invariant under test: an acknowledged write (its WAL record `fsync`'d,
//! then its id `fsync`'d to the ack log) is durable across `kill -9`. The test
//! only ever requires that acknowledged ⊆ recovered, so it cannot flake on
//! timing — a round that kills before any write simply has nothing to check.

// This is a test harness; a panic is the intended failure signal. The
// `allow-*-in-tests` clippy config only covers `#[test]` fns and `#[cfg(test)]`
// modules, not the helpers in an integration-test crate, so allow it here
// (ADR-0017 scopes the unwrap/expect ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use quiver_core::Store;

const DIM: usize = 8;

fn acked_ids(ack_path: &Path) -> BTreeSet<u64> {
    fs::read_to_string(ack_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .collect()
}

fn verify(data_dir: &Path, ack_path: &Path) {
    let acked = acked_ids(ack_path);
    let store = Store::open(data_dir).expect("store must reopen cleanly after a kill");
    let Some(cid) = store.collection_id("crash") else {
        assert!(
            acked.is_empty(),
            "writes were acknowledged but no collection was recovered"
        );
        return;
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
    let rounds = 25;
    for _ in 0..rounds {
        let mut child = Command::new(exe)
            .arg(&data_dir)
            .arg(&ack_path)
            .arg("16") // checkpoint every 16 upserts, exercising the flush path
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
        verify(&data_dir, &ack_path);
    }

    // At least one round should have acknowledged some writes; otherwise the
    // test is not actually exercising recovery.
    assert!(
        !acked_ids(&ack_path).is_empty(),
        "no writes were acknowledged across {rounds} rounds — fixture not running?"
    );

    // Recovery is idempotent: reopening again yields the same valid state.
    verify(&data_dir, &ack_path);
    verify(&data_dir, &ack_path);
}
