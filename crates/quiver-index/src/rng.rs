// SPDX-License-Identifier: AGPL-3.0-only
//! A tiny seeded SplitMix64 PRNG shared across the index crate.
//!
//! Deterministic and dependency-free, so codebook training and any randomized
//! index construction are reproducible for a fixed seed (a requirement of the
//! benchmark methodology and the recall regression gates).

/// SplitMix64 (Steele, Lea & Flood, 2014): a fast, well-distributed 64-bit
/// generator used wherever the index needs reproducible randomness.
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    /// Seed the generator. Any seed is valid.
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 uniform bits.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A `usize` in `[0, n)`. Returns 0 when `n == 0`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        // Lemire-style reduction without the bias rejection loop: good enough
        // for seeding/init, and fully deterministic.
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// A float in `[0, 1)` from the top 53 bits.
    pub(crate) fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}
