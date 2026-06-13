// SPDX-License-Identifier: AGPL-3.0-only
//! Vector indexes for Quiver, pluggable per collection: HNSW (in-memory) now;
//! DiskANN/Vamana and IVF with quantization in Phase 2.
//!
//! Status: scaffolding — HNSW lands in Phase 1. Design: `docs/index/design.md`
//! and ADR-0007 / ADR-0008.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
