// SPDX-License-Identifier: AGPL-3.0-only
//! Seeded Lloyd's k-means with k-means++ initialization.
//!
//! Shared by the product quantizer (one run per subspace, ADR-0008) and, later,
//! the IVF coarse quantizer. Deterministic for a fixed seed so codebooks — and
//! therefore recall — are reproducible. Distances use the SIMD L2 kernel; means
//! accumulate in `f64` for numerical stability, then round to `f32` centroids.

use quiver_simd::l2_sq_f32;

use crate::rng::SplitMix64;

/// Train `k` centroids over `n` points of dimensionality `dim` (a flat
/// `n × dim` row-major slice). Runs at most `iters` Lloyd iterations, stopping
/// early once assignments stabilize. Returns a flat `k × dim` centroid slice.
///
/// Empty clusters are reseeded to a random point so all `k` centroids stay live.
/// When `n < k`, surplus centroids duplicate existing points (a degenerate but
/// well-defined codebook).
///
/// # Panics
/// Panics if `data.len() != n * dim` or `dim == 0`.
pub(crate) fn kmeans(
    data: &[f32],
    n: usize,
    dim: usize,
    k: usize,
    iters: usize,
    seed: u64,
) -> Vec<f32> {
    assert!(dim > 0, "kmeans needs dim > 0");
    assert_eq!(data.len(), n * dim, "data must be n*dim");
    let k = k.max(1);
    let mut centroids = vec![0f32; k * dim];
    if n == 0 {
        return centroids;
    }
    let point = |i: usize| &data[i * dim..(i + 1) * dim];
    let mut rng = SplitMix64::new(seed);

    // --- k-means++ seeding: spread initial centroids by squared distance. ---
    let first = rng.below(n);
    centroids[0..dim].copy_from_slice(point(first));
    let mut nearest = vec![f32::INFINITY; n];
    for c in 1..k {
        let last = &centroids[(c - 1) * dim..c * dim];
        let mut total = 0f64;
        for (i, slot) in nearest.iter_mut().enumerate() {
            let d = l2_sq_f32(point(i), last);
            if d < *slot {
                *slot = d;
            }
            total += f64::from(*slot);
        }
        // Sample the next center with probability proportional to its D².
        let target = rng.next_f64() * total;
        let mut acc = 0f64;
        let mut chosen = n - 1;
        for (i, &d) in nearest.iter().enumerate() {
            acc += f64::from(d);
            if acc >= target {
                chosen = i;
                break;
            }
        }
        centroids[c * dim..(c + 1) * dim].copy_from_slice(point(chosen));
    }

    // --- Lloyd iterations. ---
    let mut assign = vec![u32::MAX; n];
    let mut sums = vec![0f64; k * dim];
    let mut counts = vec![0u64; k];
    for _ in 0..iters {
        let mut changed = false;
        // Assignment step.
        for (i, slot) in assign.iter_mut().enumerate() {
            let p = point(i);
            let mut best = 0u32;
            let mut best_d = f32::INFINITY;
            for c in 0..k {
                let d = l2_sq_f32(p, &centroids[c * dim..(c + 1) * dim]);
                if d < best_d {
                    best_d = d;
                    best = c as u32;
                }
            }
            if *slot != best {
                *slot = best;
                changed = true;
            }
        }
        // Update step: centroid = mean of assigned points.
        sums.iter_mut().for_each(|s| *s = 0.0);
        counts.iter_mut().for_each(|c| *c = 0);
        for (i, &a) in assign.iter().enumerate() {
            let c = a as usize;
            counts[c] += 1;
            let p = point(i);
            let dst = &mut sums[c * dim..(c + 1) * dim];
            for (s, &x) in dst.iter_mut().zip(p) {
                *s += f64::from(x);
            }
        }
        for c in 0..k {
            if counts[c] == 0 {
                let r = rng.below(n);
                centroids[c * dim..(c + 1) * dim].copy_from_slice(point(r));
            } else {
                let inv = 1.0 / counts[c] as f64;
                let src = &sums[c * dim..(c + 1) * dim];
                let dst = &mut centroids[c * dim..(c + 1) * dim];
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = (s * inv) as f32;
                }
            }
        }
        if !changed {
            break;
        }
    }
    centroids
}

/// Index of the centroid nearest to `v` (by squared L2). Returns 0 for an empty
/// centroid set.
pub(crate) fn nearest_centroid(v: &[f32], centroids: &[f32], dim: usize) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for (c, chunk) in centroids.chunks_exact(dim).enumerate() {
        let d = l2_sq_f32(v, chunk);
        if d < best_d {
            best_d = d;
            best = c;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_two_obvious_clusters() {
        // Two tight clusters around 0 and around 10; k=2 must split them.
        let mut data = Vec::new();
        for i in 0..50 {
            let j = (i as f32) * 0.001;
            data.extend_from_slice(&[j, j]); // near origin
        }
        for i in 0..50 {
            let j = 10.0 + (i as f32) * 0.001;
            data.extend_from_slice(&[j, j]); // near (10,10)
        }
        let n = 100;
        let centroids = kmeans(&data, n, 2, 2, 25, 7);
        // Each point must be nearer its own cluster's centroid.
        let mut ok = 0;
        for i in 0..n {
            let p = &data[i * 2..i * 2 + 2];
            let c = nearest_centroid(p, &centroids, 2);
            let near_origin = p[0] < 5.0;
            let centroid_near_origin = centroids[c * 2] < 5.0;
            if near_origin == centroid_near_origin {
                ok += 1;
            }
        }
        assert_eq!(ok, n, "every point assigned to the correct cluster");
    }

    #[test]
    fn is_deterministic_for_a_fixed_seed() {
        let data: Vec<f32> = (0..200).map(|i| (i as f32 * 0.37).sin()).collect();
        let a = kmeans(&data, 100, 2, 8, 20, 42);
        let b = kmeans(&data, 100, 2, 8, 20, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn handles_k_larger_than_n() {
        // 3 points, 8 clusters: defined, no panic, returns k*dim centroids.
        let data = vec![0.0, 0.0, 1.0, 1.0, 2.0, 2.0];
        let centroids = kmeans(&data, 3, 2, 8, 10, 1);
        assert_eq!(centroids.len(), 8 * 2);
    }

    #[test]
    fn empty_dataset_yields_zero_centroids() {
        let centroids = kmeans(&[], 0, 4, 4, 10, 0);
        assert_eq!(centroids, vec![0.0; 16]);
    }
}
