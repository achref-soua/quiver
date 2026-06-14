// SPDX-License-Identifier: AGPL-3.0-only
//! Wire-protocol fuzz target: a search `filter` is attacker-supplied JSON
//! (REST body / gRPC `filter` bytes), so deserializing a [`Filter`] from
//! arbitrary bytes must always reject cleanly — never panic, never hang.
#![no_main]

use libfuzzer_sys::fuzz_target;
use quiver_query::Filter;

fuzz_target!(|data: &[u8]| {
    // The result (Ok parsed filter or a serde error) is irrelevant — the
    // property under test is that parsing untrusted bytes never panics.
    let _ = serde_json::from_slice::<Filter>(data);
});
