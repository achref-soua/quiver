// SPDX-License-Identifier: AGPL-3.0-only
//! SIMD distance kernels for Quiver — cosine, L2, dot, and Hamming over
//! `f32`/`f16`/`bf16`/`int8`/`binary`, with runtime CPU-feature dispatch and a
//! scalar fallback.
//!
//! Status: scaffolding — kernels land in Phase 1. Design:
//! `docs/index/distance-kernels.md`.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
