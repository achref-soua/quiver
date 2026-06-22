// SPDX-License-Identifier: AGPL-3.0-only
//! A small, dependency-free tokenizer for the BM25 / full-text path (ADR-0046).
//!
//! Turns text into term ids so a tokenized string *is* a [`SparseVector`] whose
//! values are term frequencies, reusing the sparse machinery of ADR-0043/0045. The
//! pipeline is deterministic — Unicode-aware splitting on non-alphanumeric
//! boundaries, lowercasing, a small English stop-word filter, and a **light plural
//! stemmer** — so the same text always produces the same terms, and ingest and
//! query tokenize identically.
//!
//! Two deliberate, documented ceilings (each with a clean upgrade path):
//!
//! - The stemmer is a consistency-only plural normalizer (an S-stemmer variant),
//!   *not* a full Snowball/Porter implementation. It conflates `cats`→`cat`,
//!   `boxes`→`box`, `ponies`→`pony`; it is not linguistically exact, but because
//!   ingest and query share it, conflation is consistent. Swap in `rust-stemmers`
//!   behind [`tokens`] if a measured retrieval gain justifies the dependency.
//! - Term ids are a 32-bit FNV-1a hash of the token, so distinct tokens can in
//!   principle collide. For realistic vocabularies this is negligible (and
//!   learned-sparse vocabularies already collide by construction).

use std::collections::HashMap;

use crate::sparse::SparseVector;

/// A compact English stop-word list (closed-class function words). Small on
/// purpose: aggressive stop-word removal hurts more than it helps for short
/// queries, and BM25's IDF already down-weights ubiquitous terms.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// The stable 32-bit dimension id for a token (FNV-1a). Deterministic across runs
/// and platforms.
pub fn term_id(token: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5; // FNV offset basis
    for byte in token.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193); // FNV prime
    }
    hash
}

/// Tokenize `text` into normalized terms: lowercased, split on non-alphanumeric
/// boundaries, stop-words removed, and plural-stemmed. The order is preserved and
/// duplicates are kept (so callers can compute term frequencies).
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // `to_lowercase` is Unicode-correct and may expand one char to several.
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            push_term(&mut out, &current);
            current.clear();
        }
    }
    if !current.is_empty() {
        push_term(&mut out, &current);
    }
    out
}

// Stem and stop-filter one raw (already-lowercased) token, pushing it if kept.
fn push_term(out: &mut Vec<String>, raw: &str) {
    if STOP_WORDS.contains(&raw) {
        return;
    }
    let stemmed = stem_plural(raw);
    // Re-check the stop list after stemming (e.g. a stemmed form could land on one).
    if stemmed.is_empty() || STOP_WORDS.contains(&stemmed.as_str()) {
        return;
    }
    out.push(stemmed);
}

// A light, consistency-only plural stemmer (ADR-0046). Not linguistically exact;
// it conflates common plurals with their singular so query and document terms
// match. Only applied to tokens long enough that stripping is safe.
fn stem_plural(token: &str) -> String {
    let len = token.len();
    if token.ends_with("ies") && len > 4 {
        // ponies -> pony, berries -> berry
        format!("{}y", &token[..len - 3])
    } else if token.ends_with("es") && len > 3 {
        // boxes -> box, dishes -> dish
        token[..len - 2].to_owned()
    } else if token.ends_with('s') && !token.ends_with("ss") && !token.ends_with("us") && len > 3 {
        // cats -> cat
        token[..len - 1].to_owned()
    } else {
        token.to_owned()
    }
}

/// Tokenize `text` into a term-frequency [`SparseVector`]: dimension ids are token
/// ids ([`term_id`]) and values are within-text term counts. The ingest side of
/// the BM25 path (ADR-0046).
pub fn text_to_sparse(text: &str) -> SparseVector {
    let mut tf: HashMap<u32, f32> = HashMap::new();
    for token in tokens(text) {
        *tf.entry(term_id(&token)).or_insert(0.0) += 1.0;
    }
    let mut indices = Vec::with_capacity(tf.len());
    let mut values = Vec::with_capacity(tf.len());
    for (id, count) in tf {
        indices.push(id);
        values.push(count);
    }
    SparseVector { indices, values }
}

/// Tokenize `text` into the de-duplicated query term ids BM25 scores against (a
/// repeated query term counts once). The query side of the BM25 path (ADR-0046).
pub fn query_term_ids(text: &str) -> Vec<u32> {
    let mut seen = std::collections::HashSet::new();
    tokens(text)
        .into_iter()
        .map(|t| term_id(&t))
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_id_is_stable_and_distinguishes_tokens() {
        assert_eq!(term_id("quiver"), term_id("quiver"));
        assert_ne!(term_id("quiver"), term_id("vector"));
        // Known FNV-1a value pins determinism across platforms.
        assert_eq!(term_id(""), 0x811c_9dc5);
    }

    #[test]
    fn splits_lowercases_and_strips_punctuation() {
        assert_eq!(tokens("Hello, WORLD!"), vec!["hello", "world"]);
        assert_eq!(tokens("rust-lang/quiver"), vec!["rust", "lang", "quiver"]);
        assert_eq!(tokens("café Über 2026"), vec!["café", "über", "2026"]);
    }

    #[test]
    fn removes_stop_words_before_and_after_stemming() {
        // "the", "is", "on", "a" are all stop words; only content words survive.
        assert_eq!(tokens("the cat is on a mat"), vec!["cat", "mat"]);
    }

    #[test]
    fn plural_stemmer_conflates_consistently() {
        assert_eq!(stem_plural("cats"), "cat");
        assert_eq!(stem_plural("boxes"), "box");
        assert_eq!(stem_plural("ponies"), "pony");
        // Guards: short words and -ss/-us are left alone.
        assert_eq!(stem_plural("is"), "is");
        assert_eq!(stem_plural("class"), "class");
        assert_eq!(stem_plural("bus"), "bus");
        assert_eq!(stem_plural("cat"), "cat");
        // Query and document forms conflate.
        assert_eq!(tokens("cats")[0], tokens("cat")[0]);
        assert_eq!(tokens("boxes")[0], tokens("box")[0]);
    }

    #[test]
    fn text_to_sparse_counts_term_frequencies() {
        // "the" is a stop word; "cats"/"cat" conflate to one term seen twice.
        let sv = text_to_sparse("the cat cats");
        assert_eq!(sv.indices.len(), 1);
        assert_eq!(sv.values, vec![2.0]);
        assert_eq!(sv.indices[0], term_id("cat"));
        assert!(text_to_sparse("the of and").is_empty());
    }

    #[test]
    fn query_term_ids_are_deduplicated() {
        let ids = query_term_ids("cat cat dog");
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], term_id("cat")); // order preserved, first occurrence
        assert_eq!(ids[1], term_id("dog"));
        assert!(query_term_ids("the a of").is_empty());
    }
}
