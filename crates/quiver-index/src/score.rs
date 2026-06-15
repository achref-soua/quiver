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

/// The late-interaction **MaxSim** score (ColBERT, ADR-0028): for each query
/// token, the maximum similarity to any document token, summed over the query
/// tokens —
///
/// ```text
/// score(Q, D) = Σ_q  max_d  sim(q, d)
/// ```
///
/// Higher is more relevant. Per-pair similarity is `-`[`ordering_distance`], so
/// the *closest* document token under the collection metric is always the maximum
/// (`Dot`/`Cosine` give the raw similarity; `L2` gives the negated distance) and
/// MaxSim reuses exactly the math an index search ranks by — no drift. Late
/// interaction is defined for the similarity metrics (`Dot`, `Cosine`), to which a
/// `multivector` collection is restricted at creation.
///
/// An empty query or an empty document scores `0.0`. Every vector must share the
/// collection dimensionality (the embeddable database enforces this on upsert and
/// at search time).
#[must_use]
pub fn max_sim(metric: Metric, query_tokens: &[Vec<f32>], doc_tokens: &[Vec<f32>]) -> f32 {
    let mut total = 0.0;
    for q in query_tokens {
        let best = doc_tokens
            .iter()
            .map(|d| -ordering_distance(metric, q, d))
            .fold(f32::NEG_INFINITY, f32::max);
        // A document with no tokens contributes nothing (and leaves `best` at
        // negative infinity); guard so it scores 0 rather than -inf.
        if best.is_finite() {
            total += best;
        }
    }
    total
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

    #[test]
    fn max_sim_sums_per_query_token_maxima() {
        // q0 best-matches d0 (dot 1), q1 best-matches d1 (dot 2) ⇒ 1 + 2 = 3.
        let query = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let doc = vec![vec![1.0, 0.0], vec![0.0, 2.0]];
        assert!((max_sim(Metric::Dot, &query, &doc) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn max_sim_peaks_on_the_aligned_document_token() {
        // The aligned token (dot 25) beats the orthogonal-ish one (dot 15).
        let query = vec![vec![3.0, 4.0]];
        let doc = vec![vec![3.0, 4.0], vec![5.0, 0.0]];
        assert!((max_sim(Metric::Dot, &query, &doc) - 25.0).abs() < 1e-6);
    }

    #[test]
    fn max_sim_empty_query_or_document_scores_zero() {
        let one = vec![vec![1.0, 0.0]];
        assert_eq!(max_sim(Metric::Dot, &[], &one), 0.0);
        assert_eq!(max_sim(Metric::Cosine, &one, &[]), 0.0);
        assert_eq!(max_sim(Metric::Dot, &[], &[]), 0.0);
    }

    #[test]
    fn max_sim_matches_an_independent_reference() {
        // Recompute MaxSim straight from the raw simd kernels (not via
        // ordering_distance) as an independent cross-check for both similarity
        // metrics.
        let query = vec![vec![0.2, 0.9, -0.1], vec![-0.5, 0.3, 0.8]];
        let doc = vec![
            vec![0.1, 0.7, 0.0],
            vec![-0.6, 0.2, 0.9],
            vec![0.4, -0.4, 0.3],
        ];
        for metric in [Metric::Dot, Metric::Cosine] {
            let reference: f32 = query
                .iter()
                .map(|q| {
                    doc.iter()
                        .map(|d| match metric {
                            Metric::Dot => quiver_simd::dot_f32(q, d),
                            Metric::Cosine => quiver_simd::cosine_f32(q, d),
                            Metric::L2 => unreachable!(),
                        })
                        .fold(f32::NEG_INFINITY, f32::max)
                })
                .sum();
            assert!(
                (max_sim(metric, &query, &doc) - reference).abs() < 1e-5,
                "{metric:?}: {} vs reference {reference}",
                max_sim(metric, &query, &doc)
            );
        }
    }
}
