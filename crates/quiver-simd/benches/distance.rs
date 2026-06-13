// SPDX-License-Identifier: AGPL-3.0-only
//! Microbenchmarks for the distance kernels (dispatched paths).
#![allow(missing_docs)] // criterion_group! generates an undocumented public fn

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use quiver_simd::{cosine_f32, dot_f32, dot_i8, l2_sq_f32, l2_sq_i8};

fn distance(c: &mut Criterion) {
    let dim = 768usize;
    let a: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.013).sin()).collect();
    let b: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.017).cos()).collect();
    let ai: Vec<i8> = (0..dim).map(|i| (i as i8).wrapping_mul(7)).collect();
    let bi: Vec<i8> = (0..dim).map(|i| (i as i8).wrapping_mul(13)).collect();

    c.bench_function("dot_f32/768", |bch| {
        bch.iter(|| dot_f32(black_box(&a), black_box(&b)))
    });
    c.bench_function("l2_sq_f32/768", |bch| {
        bch.iter(|| l2_sq_f32(black_box(&a), black_box(&b)))
    });
    c.bench_function("cosine_f32/768", |bch| {
        bch.iter(|| cosine_f32(black_box(&a), black_box(&b)))
    });
    c.bench_function("dot_i8/768", |bch| {
        bch.iter(|| dot_i8(black_box(&ai), black_box(&bi)))
    });
    c.bench_function("l2_sq_i8/768", |bch| {
        bch.iter(|| l2_sq_i8(black_box(&ai), black_box(&bi)))
    });
}

criterion_group!(benches, distance);
criterion_main!(benches);
