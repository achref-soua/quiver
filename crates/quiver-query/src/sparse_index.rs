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
    /// Number of live documents.
    len: usize,
}

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
        // De-duplicate dims (last weight wins) so `dims_of` stays unique and a
        // malformed input can't leave a stale posting after the next cleanup.
        let mut weights: HashMap<u32, f32> = HashMap::with_capacity(sv.indices.len());
        for (&dim, &w) in sv.indices.iter().zip(sv.values.iter()) {
            weights.insert(dim, w);
        }
        let mut dims = Vec::with_capacity(weights.len());
        for (dim, w) in weights {
            self.postings.entry(dim).or_default().insert(slot, w);
            dims.push(dim);
        }
        self.dims_of[slot as usize] = dims;
    }

    /// Remove `ext_id` and free its slot. Returns whether it was present.
    pub fn remove(&mut self, ext_id: &str) -> bool {
        let Some(slot) = self.slot_of.remove(ext_id) else {
            return false;
        };
        self.clear_postings(slot);
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
