// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk fuzz target: arbitrary bytes interpreted as a 16 KiB page must be
//! rejected by the header/version/type/CRC checks — never panic, never read
//! out of bounds (a corrupt or hostile file must not crash the engine).
#![no_main]

use libfuzzer_sys::fuzz_target;
use quiver_core::page::{parse_page, PageType, PAGE_SIZE};

fuzz_target!(|data: &[u8]| {
    // Fit the fuzz input into one fixed-size page buffer (pad or truncate).
    let mut page = [0u8; PAGE_SIZE];
    let n = data.len().min(PAGE_SIZE);
    page[..n].copy_from_slice(&data[..n]);
    // Parsing must terminate in Ok/Err for every expected page type.
    for ty in [PageType::Manifest, PageType::Segment, PageType::IndexBlock] {
        let _ = parse_page(&page, ty);
    }
});
