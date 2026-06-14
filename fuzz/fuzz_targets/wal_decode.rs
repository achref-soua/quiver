// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk fuzz target: a torn or corrupt write-ahead log (arbitrary bytes on
//! disk) must recover to a point-in-time replay or a clean error — never panic.
//! This exercises the real recovery entry point (`read_all`) over a staged file.
#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;
use quiver_core::page::PlainCodec;
use quiver_core::wal::read_all;

fuzz_target!(|data: &[u8]| {
    // `read_all` reads a path, so stage the fuzz bytes as a temporary WAL file.
    let Ok(mut file) = tempfile::NamedTempFile::new() else {
        return;
    };
    if file.write_all(data).is_err() {
        return;
    }
    let _ = read_all(file.path(), &PlainCodec);
});
