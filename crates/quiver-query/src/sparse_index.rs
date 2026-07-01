// SPDX-License-Identifier: AGPL-3.0-only
//! Derived in-memory inverted index for sparse vectors (ADR-0045).
//!
//! ADR-0043 added hybrid search but scored the sparse side by scanning every row
//! of the store per query — correct, but O(N-rows). This is the inverted index it
//! described: postings of `dim → { doc-slot → weight }`, so a query touches only
//! the posting lists of its nonzero dimensions.
//!
//! Document ids are **interned** to dense `u32` slots (a [`Vec<String>`] plus a
//! free list), so a posting carries a 4-byte slot rather than a cloned id String —
//! the memory-frugal representation that matches Quiver's wedge. A per-slot list of
//! the dimensions a document occupies lets [`SparseInvertedIndex::upsert`] and
//! [`SparseInvertedIndex::remove`] clean the prior postings in O(terms) hash
//! operations, so there are **no tombstones, no generations, and no compaction
//! pass** — memory stays tight under churn.
//!
//! The index is **derived**: `quiver-embed` rebuilds it from the store on open and
//! maintains it on the incremental upsert/delete path, exactly like every other
//! Quiver index, so there is no on-disk format change and the crash gate is
//! untouched.

use std::collections::HashMap;

use crate::sparse::SparseVector;

/// An in-memory inverted index over sparse vectors (ADR-0045).
///
/// Maps each sparse dimension to the documents that have a nonzero weight there,
/// so [`search`](SparseInvertedIndex::search) accumulates a dot-product score over
/// only the query's nonzero dimensions. Built and maintained by `quiver-embed`;
/// never persisted.
#[derive(Debug, Default)]
pub struct SparseInvertedIndex {
    /// `dim → { slot → weight }`.
    postings: HashMap<u32, HashMap<u32, f32>>,
    /// `slot → the dimensions that slot's document occupies` (for O(terms) cleanup
    /// on update/delete). Empty for a freed slot.
    dims_of: Vec<Vec<u32>>,
    /// `slot → external id`. Empty string marks a freed slot.
    ext_of: Vec<String>,
    /// `external id → slot`.
    slot_of: HashMap<String, u32>,
    /// Freed slots, reused before the backing vectors grow.
    free: Vec<u32>,
    /// `slot → document length` (the sum of the slot's term weights), for BM25
    /// length normalization (ADR-0046). `0.0` for a freed slot.
    doclen: Vec<f32>,
    /// Running sum of all live document lengths, so `avgdl` is O(1) (ADR-0046).
    total_len: f64,
    /// Number of live documents.
    len: usize,
}

/// The conventional BM25 term-frequency saturation parameter (Robertson et al.).
pub const BM25_K1: f32 = 1.2;
/// The conventional BM25 length-normalization parameter.
pub const BM25_B: f32 = 0.75;

impl SparseInvertedIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live documents.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `ext_id` is currently indexed.
    pub fn contains(&self, ext_id: &str) -> bool {
        self.slot_of.contains_key(ext_id)
    }

    /// Insert or replace `ext_id`'s sparse vector. Re-upserting an existing id
    /// first removes its prior postings, so a dimension it no longer carries does
    /// not linger and a changed weight is not double-counted. Duplicate input
    /// dimensions are de-duplicated (last weight wins).
    pub fn upsert(&mut self, ext_id: &str, sv: &SparseVector) {
        let slot = match self.slot_of.get(ext_id).copied() {
            Some(slot) => {
                self.clear_postings(slot);
                slot
            }
            None => self.allocate(ext_id),
        };
        // Drop the slot's prior contribution to the running length total (0 for a
        // fresh or reused slot).
        self.total_len -= self.doclen[slot as usize] as f64;
        // De-duplicate dims (last weight wins) so `dims_of` stays unique and a
        // malformed input can't leave a stale posting after the next cleanup.
        let mut weights: HashMap<u32, f32> = HashMap::with_capacity(sv.indices.len());
        for (&dim, &w) in sv.indices.iter().zip(sv.values.iter()) {
            weights.insert(dim, w);
        }
        // Document length for BM25 = the sum of the term weights (term frequencies
        // for a tokenized text; unused by the dot-product path).
        let dl: f32 = weights.values().copied().sum();
        let mut dims = Vec::with_capacity(weights.len());
        for (dim, w) in weights {
            self.postings.entry(dim).or_default().insert(slot, w);
            dims.push(dim);
        }
        self.dims_of[slot as usize] = dims;
        self.doclen[slot as usize] = dl;
        self.total_len += dl as f64;
    }

    /// Remove `ext_id` and free its slot. Returns whether it was present.
    pub fn remove(&mut self, ext_id: &str) -> bool {
        let Some(slot) = self.slot_of.remove(ext_id) else {
            return false;
        };
        self.clear_postings(slot);
        self.total_len -= self.doclen[slot as usize] as f64;
        self.doclen[slot as usize] = 0.0;
        self.dims_of[slot as usize] = Vec::new();
        self.ext_of[slot as usize] = String::new();
        self.free.push(slot);
        self.len -= 1;
        true
    }

    /// Score every document that shares a nonzero dimension with `query` by
    /// sparse dot product, and return `(ext_id, score)` for those with a positive
    /// score, sorted by score descending then id ascending (a deterministic, total
    /// order). The caller re-checks any payload filter on the ranked ids and
    /// truncates to its depth, so low-scored rows never load a payload. Duplicate
    /// query dimensions are de-duplicated (last weight wins).
    pub fn search(&self, query: &SparseVector) -> Vec<(String, f32)> {
        let mut qweights: HashMap<u32, f32> = HashMap::with_capacity(query.indices.len());
        for (&dim, &w) in query.indices.iter().zip(query.values.iter()) {
            qweights.insert(dim, w);
        }
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for (dim, qw) in qweights {
            if let Some(plist) = self.postings.get(&dim) {
                for (&slot, &w) in plist {
                    *scores.entry(slot).or_insert(0.0) += qw * w;
                }
            }
        }
        let mut out: Vec<(String, f32)> = scores
            .into_iter()
            .filter(|&(_, score)| score > 0.0)
            .map(|(slot, score)| (self.ext_of[slot as usize].clone(), score))
            .collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Score documents against `query_terms` (term ids) with **Okapi BM25**
    /// (ADR-0046), treating each document's stored weights as term frequencies and
    /// using the index's own corpus statistics — document frequency (posting-list
    /// length), document count, and average document length. `k1`/`b` are the usual
    /// BM25 parameters ([`BM25_K1`], [`BM25_B`]). Duplicate query terms count once.
    /// Returns `(ext_id, score)` for documents with a positive score, sorted by
    /// score descending then id ascending. Uses the Lucene-style **smoothed IDF**
    /// `ln(1 + (N − df + 0.5)/(df + 0.5))`, which is always non-negative, so even a
    /// term in most of the corpus contributes a small positive amount (no negative
    /// scores to clamp).
    ///
    /// **Modality note:** BM25's corpus statistics (`N`, average document length,
    /// per-term document frequency) are derived from *every* document in this
    /// index. A collection should use one sparse modality: mixing learned-sparse
    /// vectors (arbitrary float weights, arbitrary dimension ids) with tokenized
    /// text in the same collection pollutes `avgdl`/`df` and skews BM25 scores.
    pub fn bm25_search(&self, query_terms: &[u32], k1: f32, b: f32) -> Vec<(String, f32)> {
        if self.len == 0 {
            return Vec::new();
        }
        let n = self.len as f64;
        let avgdl = (self.total_len / n).max(f64::MIN_POSITIVE);
        let (k1, b) = (k1 as f64, b as f64);
        let mut scores: HashMap<u32, f32> = HashMap::new();
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for &term in query_terms {
            if !seen.insert(term) {
                continue; // a repeated query term scores once
            }
            let Some(plist) = self.postings.get(&term) else {
                continue;
            };
            let df = plist.len() as f64;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (&slot, &tf) in plist {
                let tf = tf as f64;
                let dl = self.doclen[slot as usize] as f64;
                let denom = tf + k1 * (1.0 - b + b * (dl / avgdl));
                let contribution = idf * (tf * (k1 + 1.0)) / denom;
                *scores.entry(slot).or_insert(0.0) += contribution as f32;
            }
        }
        let mut out: Vec<(String, f32)> = scores
            .into_iter()
            .filter(|&(_, score)| score > 0.0)
            .map(|(slot, score)| (self.ext_of[slot as usize].clone(), score))
            .collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    // Allocate a slot for a brand-new id, reusing a freed one when available.
    fn allocate(&mut self, ext_id: &str) -> u32 {
        let slot = match self.free.pop() {
            Some(slot) => {
                self.ext_of[slot as usize] = ext_id.to_owned();
                slot
            }
            None => {
                let slot = self.ext_of.len() as u32;
                self.ext_of.push(ext_id.to_owned());
                self.dims_of.push(Vec::new());
                self.doclen.push(0.0);
                slot
            }
        };
        self.slot_of.insert(ext_id.to_owned(), slot);
        self.len += 1;
        slot
    }

    // Drop every posting a slot currently holds (its `dims_of` entry is rewritten
    // by the caller). Removes a dimension's map once it empties so the index does
    // not accumulate empty posting lists.
    fn clear_postings(&mut self, slot: u32) {
        for dim in std::mem::take(&mut self.dims_of[slot as usize]) {
            if let Some(plist) = self.postings.get_mut(&dim) {
                plist.remove(&slot);
                if plist.is_empty() {
                    self.postings.remove(&dim);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(indices: &[u32], values: &[f32]) -> SparseVector {
        SparseVector {
            indices: indices.to_vec(),
            values: values.to_vec(),
        }
    }

    fn ids(results: &[(String, f32)]) -> Vec<&str> {
        results.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn empty_index_reports_empty_and_searches_to_nothing() {
        let idx = SparseInvertedIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(!idx.contains("x"));
        assert!(idx.search(&sv(&[1, 2], &[1.0, 1.0])).is_empty());
    }

    #[test]
    fn ranks_by_dot_product_and_breaks_ties_by_id() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1, 2], &[1.0, 1.0])); // dot with q = 1*2 + 1*3 = 5
        idx.upsert("b", &sv(&[2, 3], &[1.0, 1.0])); // dot = 1*3 = 3
        idx.upsert("c", &sv(&[1], &[2.0])); // dot = 2*2 = 4
        idx.upsert("tie", &sv(&[1, 2], &[1.0, 1.0])); // dot = 5, ties "a"
        assert_eq!(idx.len(), 4);
        let q = sv(&[1, 2], &[2.0, 3.0]);
        let res = idx.search(&q);
        // a (5) and tie (5) lead; id breaks the tie ("a" < "tie"); then c (4), b (3).
        assert_eq!(ids(&res), vec!["a", "tie", "c", "b"]);
        assert_eq!(res[0].1, 5.0);
        assert_eq!(res[3].1, 3.0);
    }

    #[test]
    fn matches_brute_force_dot_product() {
        let docs = [
            ("a", sv(&[1, 5, 9], &[1.0, 2.0, 3.0])),
            ("b", sv(&[9, 1, 7], &[10.0, 4.0, 1.0])),
            ("c", sv(&[2, 4], &[5.0, 5.0])),
            ("z", sv(&[100], &[5.0])), // shares no query dim → score 0, dropped both sides
        ];
        let mut idx = SparseInvertedIndex::new();
        for (id, v) in &docs {
            idx.upsert(id, v);
        }
        let q = sv(&[1, 9, 4], &[1.5, 0.5, 2.0]);
        let mut expected: Vec<(String, f32)> = docs
            .iter()
            .map(|(id, v)| ((*id).to_owned(), q.dot(v)))
            .filter(|&(_, s)| s > 0.0)
            .collect();
        expected.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        assert_eq!(idx.search(&q), expected);
    }

    #[test]
    fn reupsert_replaces_old_postings_without_double_counting() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1, 2], &[1.0, 1.0]));
        // Re-upsert "a" onto a disjoint dimension; the old dims must not linger.
        idx.upsert("a", &sv(&[3], &[5.0]));
        assert_eq!(idx.len(), 1);
        assert!(idx.search(&sv(&[1, 2], &[1.0, 1.0])).is_empty());
        let res = idx.search(&sv(&[3], &[2.0]));
        assert_eq!(ids(&res), vec!["a"]);
        assert_eq!(res[0].1, 10.0);
    }

    #[test]
    fn remove_drops_from_results_and_reuses_the_slot() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1], &[1.0]));
        idx.upsert("b", &sv(&[1], &[1.0]));
        assert!(idx.remove("a"));
        assert!(!idx.remove("a")); // gone now
        assert!(!idx.contains("a"));
        assert_eq!(idx.len(), 1);
        assert_eq!(ids(&idx.search(&sv(&[1], &[1.0]))), vec!["b"]);
        // The freed slot is reused before the backing vectors grow.
        let before = idx.ext_of.len();
        idx.upsert("c", &sv(&[1], &[1.0]));
        assert_eq!(idx.ext_of.len(), before);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn query_sharing_no_dimension_scores_nothing() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1, 2], &[1.0, 1.0]));
        assert!(idx.search(&sv(&[7, 8], &[1.0, 1.0])).is_empty());
    }

    #[test]
    fn duplicate_dimensions_are_deduplicated_last_wins() {
        let mut idx = SparseInvertedIndex::new();
        // Duplicate dim 1 in the stored vector: last weight (3.0) wins.
        idx.upsert("a", &sv(&[1, 1], &[2.0, 3.0]));
        // Duplicate dim 1 in the query: last weight (10.0) wins.
        let res = idx.search(&sv(&[1, 1], &[5.0, 10.0]));
        assert_eq!(res, vec![("a".to_owned(), 30.0)]);
    }

    #[test]
    fn negative_and_zero_scores_are_dropped() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("neg", &sv(&[1], &[-1.0]));
        idx.upsert("zero", &sv(&[2], &[0.0]));
        // Query overlaps both, but neg scores < 0 and zero scores == 0 → neither kept.
        assert!(idx.search(&sv(&[1, 2], &[1.0, 1.0])).is_empty());
    }

    #[test]
    fn empty_sparse_vector_is_a_live_doc_with_no_postings() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[], &[]));
        assert!(idx.contains("a"));
        assert_eq!(idx.len(), 1);
        assert!(idx.search(&sv(&[1], &[1.0])).is_empty());
    }

    #[test]
    fn debug_is_derivable() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1], &[1.0]));
        assert!(format!("{idx:?}").contains("SparseInvertedIndex"));
    }

    #[test]
    fn bm25_ranks_by_term_frequency() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("hi", &sv(&[1], &[3.0])); // term 1, tf 3
        idx.upsert("lo", &sv(&[1], &[1.0])); // term 1, tf 1
        idx.upsert("other", &sv(&[2], &[5.0])); // does not contain term 1
        let res = idx.bm25_search(&[1], BM25_K1, BM25_B);
        assert_eq!(ids(&res), vec!["hi", "lo"], "other lacks the term");
        assert!(res[0].1 > res[1].1);
    }

    #[test]
    fn bm25_rewards_shorter_documents_at_equal_term_frequency() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("short", &sv(&[1], &[2.0])); // length 2
        idx.upsert("long", &sv(&[1, 2], &[2.0, 8.0])); // same tf for term 1, length 10
        let res = idx.bm25_search(&[1], BM25_K1, BM25_B);
        assert_eq!(
            res[0].0, "short",
            "length normalization favours the shorter doc"
        );
    }

    #[test]
    fn bm25_empty_index_and_unknown_terms_score_nothing() {
        assert!(
            SparseInvertedIndex::new()
                .bm25_search(&[1], BM25_K1, BM25_B)
                .is_empty()
        );
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1], &[1.0]));
        assert!(idx.bm25_search(&[999], BM25_K1, BM25_B).is_empty());
    }

    #[test]
    fn bm25_deduplicates_query_terms() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1], &[1.0]));
        idx.upsert("b", &sv(&[1, 2], &[1.0, 1.0]));
        let once = idx.bm25_search(&[1], BM25_K1, BM25_B);
        let twice = idx.bm25_search(&[1, 1, 1], BM25_K1, BM25_B);
        assert_eq!(once, twice, "a repeated query term scores once");
    }

    #[test]
    fn bm25_tracks_document_length_through_update_and_delete() {
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1, 2], &[1.0, 2.0])); // length 3
        assert_eq!(idx.total_len, 3.0);
        idx.upsert("a", &sv(&[1], &[5.0])); // update → length 5
        assert_eq!(idx.total_len, 5.0);
        idx.upsert("b", &sv(&[1], &[2.0])); // +2 → 7
        assert_eq!(idx.total_len, 7.0);
        assert!(idx.remove("a")); // −5 → 2
        assert_eq!(idx.total_len, 2.0);
        assert_eq!(idx.doclen[idx.slot_of["b"] as usize], 2.0);
    }

    #[test]
    fn clear_postings_tolerates_a_dim_missing_from_postings() {
        // White-box robustness check: if a slot's dim list ever references a
        // dimension with no posting map (an invariant break), cleanup must not
        // panic — it simply skips the absent dimension.
        let mut idx = SparseInvertedIndex::new();
        idx.upsert("a", &sv(&[1], &[1.0]));
        let slot = idx.slot_of["a"];
        idx.dims_of[slot as usize].push(42); // dim 42 has no posting map
        assert!(idx.remove("a"));
        assert!(idx.is_empty());
    }
}
