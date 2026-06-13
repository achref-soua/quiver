// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver's query planner and hybrid filtered search over vectors and metadata
//! (pre- vs post-filtering by predicate selectivity; top-k merge and re-rank).
//!
//! Status: scaffolding — implementation lands in Phase 1. Design:
//! `docs/index/design.md` (filtered search) and `docs/api/wire-protocol.md`.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
