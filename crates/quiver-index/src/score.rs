// SPDX-License-Identifier: AGPL-3.0-only
//! Metric scoring shared by the indexes and the embeddable pre-filter
//! brute-force path, so the ordering key and the reported score never drift
//! between an index search and an exact scan.
//!
//! Search ranks candidates by [`ordering_distance`] — *smaller is closer* for
//! every metric, with similarities negated so one min-ordering works regardless
//! of metric. The value handed back to a caller is the *true* metric
//! ([`report_metric`]): squared-L2 distance unchanged, similarities positive.

use quiver_simd::Metric;

/// Distance between `a` and `b` under `metric` with **smaller is closer**
/// semantics: squared Euclidean distance for [`Metric::L2`], and the negated
/// similarity for [`Metric::Dot`] / [`Metric::Cosine`] (so every metric orders
/// the same way). This is the key search ranks by.
#[must_use]
pub fn ordering_distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::L2 => quiver_simd::l2_sq_f32(a, b),
        Metric::Dot => -quiver_simd::dot_f32(a, b),
        Metric::Cosine => -quiver_simd::cosine_f32(a, b),
    }
}

/// Convert an [`ordering_distance`] back to the **true metric** value reported
/// to callers: the squared-L2 distance is returned unchanged, while a negated
/// similarity is flipped positive again.
#[must_use]
pub fn report_metric(metric: Metric, ordering_distance: f32) -> f32 {
    match metric {
        Metric::L2 => ordering_distance,
        Metric::Dot | Metric::Cosine => -ordering_distance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_orders_and_reports_the_same_value() {
        let a = [0.0, 0.0];
        let near = [1.0, 0.0];
        let far = [9.0, 0.0];
        let dn = ordering_distance(Metric::L2, &a, &near);
        let df = ordering_distance(Metric::L2, &a, &far);
        assert!(dn < df, "nearer point orders smaller");
        // L2 is a true distance: the reported value is the ordering value.
        assert_eq!(report_metric(Metric::L2, dn), dn);
    }

    #[test]
    fn similarities_order_inverted_but_report_positive() {
        let q = [1.0, 0.0];
        let aligned = [1.0, 0.0];
        let orthogonal = [0.0, 1.0];
        for metric in [Metric::Dot, Metric::Cosine] {
            let close = ordering_distance(metric, &q, &aligned);
            let far = ordering_distance(metric, &q, &orthogonal);
            // More similar ⇒ smaller ordering key.
            assert!(close < far, "{metric:?}: aligned orders smaller");
            // ...but the reported score is the positive similarity.
            assert!(report_metric(metric, close) >= report_metric(metric, far));
            assert!(
                report_metric(metric, close) > 0.0,
                "{metric:?} similarity positive"
            );
        }
    }
}
