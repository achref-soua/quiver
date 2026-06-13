// SPDX-License-Identifier: AGPL-3.0-only
//! Scalar reference kernels.
//!
//! Always correct and portable: the oracle the SIMD paths are differential-
//! tested against, and the fallback when no SIMD feature is detected.

pub(crate) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub(crate) fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

pub(crate) fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

pub(crate) fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| i32::from(*x) * i32::from(*y))
        .sum()
}

pub(crate) fn l2_sq_i8(a: &[i8], b: &[i8]) -> i32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = i32::from(*x) - i32::from(*y);
            d * d
        })
        .sum()
}

pub(crate) fn hamming_u64(a: &[u64], b: &[u64]) -> u32 {
    // `count_ones` lowers to a hardware POPCNT on any x86_64/aarch64 Quiver
    // targets, so the scalar path is itself fast; the AVX2 path widens it to
    // four words per step.
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum()
}
