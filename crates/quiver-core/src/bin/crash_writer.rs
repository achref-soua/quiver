// SPDX-License-Identifier: AGPL-3.0-only
//! Crash-recovery test fixture: a writer process the harness `SIGKILL`s.
//!
//! Not part of the public API. It opens a [`quiver_core::Store`] and upserts
//! deterministic records in a tight loop, recording each *acknowledged* id to a
//! sidecar ack log (flushed and `fsync`'d before the next write) so the parent
//! test knows exactly which writes were durable at the instant of the kill. On
//! restart it resumes from the recovered row count, so repeated kills make
//! forward progress. See `tests/crash_recovery.rs`.

use std::io::Write;
use std::path::Path;

use quiver_core::{Descriptor, DistanceMetric, Dtype, Store};

const DIM: usize = 8;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let data_dir = args
        .next()
        .ok_or("usage: crash_writer <data_dir> <ack_log> [checkpoint_every]")?;
    let ack_path = args.next().ok_or("missing ack log path")?;
    let checkpoint_every: u64 = args.next().map_or(0, |s| s.parse().unwrap_or(0));

    let mut store = Store::open(Path::new(&data_dir))?;
    let cid = match store.collection_id("crash") {
        Some(c) => c,
        None => store.create_collection(
            "crash",
            Descriptor {
                dim: DIM as u32,
                dtype: Dtype::F32,
                metric: DistanceMetric::L2,
            },
        )?,
    };

    // Resume from the recovered row count so each restart makes progress.
    let mut next = store.len(cid)? as u64;
    let mut ack = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ack_path)?;

    loop {
        let id = format!("k{next}");
        let vector: Vec<f32> = (0..DIM).map(|j| next as f32 + j as f32).collect();
        let payload = format!(r#"{{"n":{next}}}"#).into_bytes();
        store.upsert(cid, &id, &vector, &payload)?;
        // The upsert is now durable (its WAL record was fsync'd). Record the ack
        // durably too, before the next write, so the parent never expects an id
        // the store did not actually persist.
        writeln!(ack, "{next}")?;
        ack.flush()?;
        ack.sync_data()?;

        if checkpoint_every > 0 && (next + 1).is_multiple_of(checkpoint_every) {
            store.checkpoint()?;
        }
        next += 1;
    }
}
