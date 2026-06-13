// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver's storage engine: memory-mapped segments, a write-ahead log, a
//! versioned manifest with atomic updates, copy-on-write, and crash recovery.
//!
//! Status: scaffolding — implementation lands in Phase 1. Design:
//! `docs/storage/on-disk-format.md` and ADR-0004 / ADR-0005.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
