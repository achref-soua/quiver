// SPDX-License-Identifier: AGPL-3.0-only
//! AVX2 / FMA implementations of the distance kernels (x86_64).
//!
//! `f32` kernels use AVX + FMA; `i8` kernels use AVX2 integer ops. The required
//! CPU feature is checked by the caller in `lib.rs` before dispatch. Under
//! edition 2024 (target-feature 1.1), register-only intrinsics and same-feature
//! calls are safe inside a `#[target_feature]` function; only the raw-pointer
//! loads need an `unsafe` block.

use std::arch::x86_64::*;

/// Horizontal sum of an `f32` x8 accumulator (AVX; safe — caller carries the feature).
#[target_feature(enable = "avx")]
fn hsum256_ps(v: __m256) -> f32 {
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps(v, 1);
    let sum = _mm_add_ps(lo, hi);
    let shuf = _mm_movehdup_ps(sum);
    let sums = _mm_add_ps(sum, shuf);
    let shuf2 = _mm_movehl_ps(shuf, sums);
    let sums2 = _mm_add_ss(sums, shuf2);
    _mm_cvtss_f32(sums2)
}

/// Horizontal sum of an `i32` x8 accumulator (AVX2; safe — caller carries the feature).
#[target_feature(enable = "avx2")]
fn hsum256_epi32(v: __m256i) -> i32 {
    let lo = _mm256_castsi256_si128(v);
    let hi = _mm256_extracti128_si256(v, 1);
    let sum = _mm_add_epi32(lo, hi);
    let hi64 = _mm_unpackhi_epi64(sum, sum);
    let sum64 = _mm_add_epi32(sum, hi64);
    let hi32 = _mm_shuffle_epi32(sum64, 0b01);
    let sum32 = _mm_add_epi32(sum64, hi32);
    _mm_cvtsi128_si32(sum32)
}

/// Inner product of two equal-length `f32` slices.
///
/// # Safety
/// The target CPU must support AVX and FMA.
#[target_feature(enable = "avx,fma")]
pub(crate) unsafe fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len();
    let mut i = 0usize;
    let mut acc = _mm256_setzero_ps();
    while i + 8 <= n {
        // SAFETY: i + 8 <= n, so the 8-lane loads at offset i are in bounds.
        unsafe {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            acc = _mm256_fmadd_ps(va, vb, acc);
        }
        i += 8;
    }
    let mut sum = hsum256_ps(acc);
    while i < n {
        sum += a[i] * b[i];
        i += 1;
    }
    sum
}

/// Squared Euclidean distance of two equal-length `f32` slices.
///
/// # Safety
/// The target CPU must support AVX and FMA.
#[target_feature(enable = "avx,fma")]
pub(crate) unsafe fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len();
    let mut i = 0usize;
    let mut acc = _mm256_setzero_ps();
    while i + 8 <= n {
        // SAFETY: i + 8 <= n, so the 8-lane loads at offset i are in bounds.
        unsafe {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            let d = _mm256_sub_ps(va, vb);
            acc = _mm256_fmadd_ps(d, d, acc);
        }
        i += 8;
    }
    let mut sum = hsum256_ps(acc);
    while i < n {
        let d = a[i] - b[i];
        sum += d * d;
        i += 1;
    }
    sum
}

/// Cosine similarity (in `[-1, 1]`) of two equal-length `f32` slices.
///
/// # Safety
/// The target CPU must support AVX and FMA.
#[target_feature(enable = "avx,fma")]
pub(crate) unsafe fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len();
    let mut i = 0usize;
    let mut dot = _mm256_setzero_ps();
    let mut na = _mm256_setzero_ps();
    let mut nb = _mm256_setzero_ps();
    while i + 8 <= n {
        // SAFETY: i + 8 <= n, so the 8-lane loads at offset i are in bounds.
        unsafe {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            dot = _mm256_fmadd_ps(va, vb, dot);
            na = _mm256_fmadd_ps(va, va, na);
            nb = _mm256_fmadd_ps(vb, vb, nb);
        }
        i += 8;
    }
    let mut d = hsum256_ps(dot);
    let mut sa = hsum256_ps(na);
    let mut sb = hsum256_ps(nb);
    while i < n {
        d += a[i] * b[i];
        sa += a[i] * a[i];
        sb += b[i] * b[i];
        i += 1;
    }
    let denom = sa.sqrt() * sb.sqrt();
    if denom > 0.0 { d / denom } else { 0.0 }
}

/// Inner product of two equal-length `i8` slices (accumulated in `i32`).
///
/// # Safety
/// The target CPU must support AVX2.
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    let n = a.len();
    let mut i = 0usize;
    let mut acc = _mm256_setzero_si256();
    while i + 16 <= n {
        // SAFETY: i + 16 <= n, so the 16-lane loads at offset i are in bounds.
        unsafe {
            let va = _mm_loadu_si128(a.as_ptr().add(i).cast());
            let vb = _mm_loadu_si128(b.as_ptr().add(i).cast());
            let va16 = _mm256_cvtepi8_epi16(va);
            let vb16 = _mm256_cvtepi8_epi16(vb);
            let prod = _mm256_madd_epi16(va16, vb16);
            acc = _mm256_add_epi32(acc, prod);
        }
        i += 16;
    }
    let mut sum = hsum256_epi32(acc);
    while i < n {
        sum += i32::from(a[i]) * i32::from(b[i]);
        i += 1;
    }
    sum
}

/// Population count of a 256-bit register via Muła's nibble-shuffle algorithm
/// (AVX2 lacks a per-qword popcount until VPOPCNTDQ). Returns four per-lane byte
/// sums in the qword lanes, ready to accumulate with `_mm256_add_epi64`.
#[target_feature(enable = "avx2")]
fn popcnt256(v: __m256i) -> __m256i {
    // Per-nibble popcount lookup table, replicated across both 128-bit halves.
    let lookup = _mm256_setr_epi8(
        0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3,
        3, 4,
    );
    let low_mask = _mm256_set1_epi8(0x0f);
    let lo = _mm256_and_si256(v, low_mask);
    let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), low_mask);
    let popcnt_lo = _mm256_shuffle_epi8(lookup, lo);
    let popcnt_hi = _mm256_shuffle_epi8(lookup, hi);
    let bytes = _mm256_add_epi8(popcnt_lo, popcnt_hi);
    // Sum the 8 bytes within each qword lane into four lane totals.
    _mm256_sad_epu8(bytes, _mm256_setzero_si256())
}

/// Hamming distance of two equal-length packed-bit vectors (`u64` words):
/// `popcount(a XOR b)`.
///
/// # Safety
/// The target CPU must support AVX2.
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn hamming_u64(a: &[u64], b: &[u64]) -> u32 {
    let n = a.len();
    let mut i = 0usize;
    let mut acc = _mm256_setzero_si256();
    while i + 4 <= n {
        // SAFETY: i + 4 <= n, so the 4-qword (256-bit) loads at offset i are in bounds.
        unsafe {
            let va = _mm256_loadu_si256(a.as_ptr().add(i).cast());
            let vb = _mm256_loadu_si256(b.as_ptr().add(i).cast());
            let x = _mm256_xor_si256(va, vb);
            acc = _mm256_add_epi64(acc, popcnt256(x));
        }
        i += 4;
    }
    // Horizontal sum of the four qword lanes.
    let lo = _mm256_castsi256_si128(acc);
    let hi = _mm256_extracti128_si256(acc, 1);
    let sum128 = _mm_add_epi64(lo, hi);
    let hi64 = _mm_unpackhi_epi64(sum128, sum128);
    let total = _mm_add_epi64(sum128, hi64);
    #[allow(clippy::cast_sign_loss)]
    let mut sum = _mm_cvtsi128_si64(total) as u64 as u32;
    while i < n {
        sum += (a[i] ^ b[i]).count_ones();
        i += 1;
    }
    sum
}

/// Squared Euclidean distance of two equal-length `i8` slices (in `i32`).
///
/// # Safety
/// The target CPU must support AVX2.
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn l2_sq_i8(a: &[i8], b: &[i8]) -> i32 {
    let n = a.len();
    let mut i = 0usize;
    let mut acc = _mm256_setzero_si256();
    while i + 16 <= n {
        // SAFETY: i + 16 <= n, so the 16-lane loads at offset i are in bounds.
        unsafe {
            let va = _mm_loadu_si128(a.as_ptr().add(i).cast());
            let vb = _mm_loadu_si128(b.as_ptr().add(i).cast());
            let va16 = _mm256_cvtepi8_epi16(va);
            let vb16 = _mm256_cvtepi8_epi16(vb);
            let diff = _mm256_sub_epi16(va16, vb16);
            let sq = _mm256_madd_epi16(diff, diff);
            acc = _mm256_add_epi32(acc, sq);
        }
        i += 16;
    }
    let mut sum = hsum256_epi32(acc);
    while i < n {
        let d = i32::from(a[i]) - i32::from(b[i]);
        sum += d * d;
        i += 1;
    }
    sum
}
