// SPDX-License-Identifier: AGPL-3.0-only
//! SIMD distance kernels for Quiver — cosine, squared-L2, and inner product over
//! `f32` and `i8`, plus Hamming distance over packed-bit (`u64`) vectors, with
//! runtime CPU-feature dispatch and a scalar fallback.
//!
//! Each public function selects the best available implementation once per call
//! (`is_x86_feature_detected!` results are cached by `std`) and always has a
//! correct scalar fallback. The SIMD paths are differential-tested against the
//! scalar reference. Design: `docs/index/distance-kernels.md`, ADR-0009.

mod scalar;

#[cfg(target_arch = "x86_64")]
mod avx2;

/// A supported distance / similarity metric over dense vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Inner product — higher is more similar.
    Dot,
    /// Cosine similarity in `[-1, 1]` — higher is more similar.
    Cosine,
    /// Squared Euclidean distance — lower is more similar.
    L2,
}

/// Inner product (dot product) of two equal-length `f32` vectors.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
#[inline]
#[must_use]
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "vectors must have equal length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma") {
            // SAFETY: AVX and FMA were just confirmed present.
            return unsafe { avx2::dot_f32(a, b) };
        }
    }
    scalar::dot_f32(a, b)
}

/// Squared Euclidean distance of two equal-length `f32` vectors.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
#[inline]
#[must_use]
pub fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "vectors must have equal length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma") {
            // SAFETY: AVX and FMA were just confirmed present.
            return unsafe { avx2::l2_sq_f32(a, b) };
        }
    }
    scalar::l2_sq_f32(a, b)
}

/// Cosine similarity (in `[-1, 1]`) of two equal-length `f32` vectors.
///
/// Returns `0.0` if either vector has zero magnitude.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
#[inline]
#[must_use]
pub fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "vectors must have equal length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma") {
            // SAFETY: AVX and FMA were just confirmed present.
            return unsafe { avx2::cosine_f32(a, b) };
        }
    }
    scalar::cosine_f32(a, b)
}

/// Inner product of two equal-length `i8` vectors, accumulated in `i32`.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
#[inline]
#[must_use]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    assert_eq!(a.len(), b.len(), "vectors must have equal length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was just confirmed present.
            return unsafe { avx2::dot_i8(a, b) };
        }
    }
    scalar::dot_i8(a, b)
}

/// Squared Euclidean distance of two equal-length `i8` vectors, in `i32`.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
#[inline]
#[must_use]
pub fn l2_sq_i8(a: &[i8], b: &[i8]) -> i32 {
    assert_eq!(a.len(), b.len(), "vectors must have equal length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was just confirmed present.
            return unsafe { avx2::l2_sq_i8(a, b) };
        }
    }
    scalar::l2_sq_i8(a, b)
}

/// Hamming distance of two equal-length packed-bit vectors: the number of
/// differing bits, `popcount(a XOR b)`, over `u64` words.
///
/// This is the fast pre-filter for binary-quantized search (ADR-0008): pack each
/// vector's sign bits into `u64` words, rank candidates by Hamming distance, then
/// re-rank the shortlist with an exact full-precision metric.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
#[inline]
#[must_use]
pub fn hamming_u64(a: &[u64], b: &[u64]) -> u32 {
    assert_eq!(a.len(), b.len(), "vectors must have equal length");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was just confirmed present.
            return unsafe { avx2::hamming_u64(a, b) };
        }
    }
    scalar::hamming_u64(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny deterministic xorshift PRNG so tests need no external dependency.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        /// A value in `[-1, 1)`, from 24 random bits.
        fn f32(&mut self) -> f32 {
            let bits = (self.next_u64() >> 40) as u32;
            (bits as f32 / 16_777_216.0) * 2.0 - 1.0
        }
        fn i8(&mut self) -> i8 {
            (self.next_u64() >> 56) as i8
        }
    }

    const F32_DIMS: &[usize] = &[0, 1, 7, 8, 9, 16, 31, 128, 769];
    const I8_DIMS: &[usize] = &[0, 1, 15, 16, 17, 31, 128, 769];
    // Word counts including non-multiples of 4 to exercise the AVX2 tail.
    const U64_WORDS: &[usize] = &[0, 1, 2, 3, 4, 5, 7, 8, 13, 16, 96];

    // A naive, obviously-correct Hamming reference: count differing bits one at
    // a time, independent of `count_ones`.
    fn hamming_naive(a: &[u64], b: &[u64]) -> u32 {
        let mut n = 0u32;
        for (x, y) in a.iter().zip(b.iter()) {
            let mut d = x ^ y;
            while d != 0 {
                n += (d & 1) as u32;
                d >>= 1;
            }
        }
        n
    }

    fn close(got: f32, exp: f32) -> bool {
        (got - exp).abs() <= 1e-3 + 1e-4 * exp.abs()
    }

    #[test]
    fn dot_f32_matches_scalar() {
        let mut rng = Rng::new(0xC0FFEE);
        for &dim in F32_DIMS {
            let a: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let b: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let (got, exp) = (dot_f32(&a, &b), scalar::dot_f32(&a, &b));
            assert!(close(got, exp), "dim {dim}: {got} vs {exp}");
        }
    }

    #[test]
    fn l2_sq_f32_matches_scalar() {
        let mut rng = Rng::new(0xBEEF);
        for &dim in F32_DIMS {
            let a: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let b: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let (got, exp) = (l2_sq_f32(&a, &b), scalar::l2_sq_f32(&a, &b));
            assert!(close(got, exp), "dim {dim}: {got} vs {exp}");
        }
    }

    #[test]
    fn cosine_f32_matches_scalar() {
        let mut rng = Rng::new(0xABCD);
        for &dim in F32_DIMS {
            let a: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let b: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let (got, exp) = (cosine_f32(&a, &b), scalar::cosine_f32(&a, &b));
            assert!(close(got, exp), "dim {dim}: {got} vs {exp}");
        }
    }

    #[test]
    fn i8_kernels_match_scalar_exactly() {
        let mut rng = Rng::new(0x1234_5678);
        for &dim in I8_DIMS {
            let a: Vec<i8> = (0..dim).map(|_| rng.i8()).collect();
            let b: Vec<i8> = (0..dim).map(|_| rng.i8()).collect();
            assert_eq!(dot_i8(&a, &b), scalar::dot_i8(&a, &b), "dot dim {dim}");
            assert_eq!(l2_sq_i8(&a, &b), scalar::l2_sq_i8(&a, &b), "l2 dim {dim}");
        }
    }

    #[test]
    fn cosine_zero_vector_is_zero() {
        let z = vec![0.0f32; 8];
        let v = vec![1.0f32; 8];
        assert!(cosine_f32(&z, &v).abs() < 1e-6);
        assert!(cosine_f32(&z, &z).abs() < 1e-6);
    }

    #[test]
    fn empty_vectors() {
        let e: [f32; 0] = [];
        assert!(dot_f32(&e, &e).abs() < 1e-6);
        assert!(l2_sq_f32(&e, &e).abs() < 1e-6);
        let ei: [i8; 0] = [];
        assert_eq!(dot_i8(&ei, &ei), 0);
        let eu: [u64; 0] = [];
        assert_eq!(hamming_u64(&eu, &eu), 0);
    }

    #[test]
    fn hamming_matches_naive_and_scalar() {
        let mut rng = Rng::new(0x9911_AA55);
        for &words in U64_WORDS {
            let a: Vec<u64> = (0..words).map(|_| rng.next_u64()).collect();
            let b: Vec<u64> = (0..words).map(|_| rng.next_u64()).collect();
            let naive = hamming_naive(&a, &b);
            assert_eq!(hamming_u64(&a, &b), naive, "dispatch, {words} words");
            assert_eq!(scalar::hamming_u64(&a, &b), naive, "scalar, {words} words");
        }
    }

    #[test]
    fn hamming_axioms() {
        let mut rng = Rng::new(0x5151_2727);
        for &words in U64_WORDS {
            let a: Vec<u64> = (0..words).map(|_| rng.next_u64()).collect();
            let b: Vec<u64> = (0..words).map(|_| rng.next_u64()).collect();
            // Identity of indiscernibles, symmetry, and the bit-count bound.
            assert_eq!(hamming_u64(&a, &a), 0, "{words}: d(a,a)=0");
            assert_eq!(
                hamming_u64(&a, &b),
                hamming_u64(&b, &a),
                "{words}: symmetry"
            );
            assert!(
                hamming_u64(&a, &b) <= (words * 64) as u32,
                "{words}: within bound"
            );
        }
        // All-ones vs all-zeros differs in every bit.
        let ones = vec![u64::MAX; 8];
        let zeros = vec![0u64; 8];
        assert_eq!(hamming_u64(&ones, &zeros), 8 * 64);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn hamming_avx2_matches_scalar_directly() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let mut rng = Rng::new(0xC1A0_F00D);
        for &words in U64_WORDS {
            let a: Vec<u64> = (0..words).map(|_| rng.next_u64()).collect();
            let b: Vec<u64> = (0..words).map(|_| rng.next_u64()).collect();
            // SAFETY: AVX2 detected above.
            let got = unsafe { avx2::hamming_u64(&a, &b) };
            assert_eq!(got, scalar::hamming_u64(&a, &b), "avx2 {words} words");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_paths_match_scalar_directly() {
        let have_f32 = is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma");
        let have_i8 = is_x86_feature_detected!("avx2");
        if !have_f32 && !have_i8 {
            return;
        }
        let mut rng = Rng::new(99);
        for &dim in &[8usize, 17, 256, 769] {
            let a: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            let b: Vec<f32> = (0..dim).map(|_| rng.f32()).collect();
            if have_f32 {
                // SAFETY: AVX + FMA detected above.
                let got = unsafe { avx2::dot_f32(&a, &b) };
                assert!(close(got, scalar::dot_f32(&a, &b)), "dot dim {dim}");
                // SAFETY: AVX + FMA detected above.
                let got = unsafe { avx2::l2_sq_f32(&a, &b) };
                assert!(close(got, scalar::l2_sq_f32(&a, &b)), "l2 dim {dim}");
            }
            if have_i8 {
                let ai: Vec<i8> = (0..dim).map(|_| rng.i8()).collect();
                let bi: Vec<i8> = (0..dim).map(|_| rng.i8()).collect();
                // SAFETY: AVX2 detected above.
                assert_eq!(unsafe { avx2::dot_i8(&ai, &bi) }, scalar::dot_i8(&ai, &bi));
            }
        }
    }
}
