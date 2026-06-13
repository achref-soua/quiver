// SPDX-License-Identifier: AGPL-3.0-only
//! The embeddable, in-process Quiver database handle over the storage and index
//! engines — the seam shared by server mode and library mode, so both exercise
//! identical engine semantics.
//!
//! Status: scaffolding — the handle API lands in Phase 1. Design:
//! `docs/architecture/overview.md`.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
