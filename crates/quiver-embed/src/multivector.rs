// SPDX-License-Identifier: AGPL-3.0-only
//! Multi-vector document machinery (ADR-0028): the token-id encoding that maps a
//! document's token vectors onto store rows. Split out of the crate root;
//! re-exported by `lib.rs`, so no reference elsewhere changes.
#![allow(clippy::wildcard_imports)]

// The byte separating a multi-vector document id from a token ordinal in a token
// row's external id (`<doc-id><US><ordinal>`): the ASCII Unit Separator, which is
// disallowed in user document ids (ADR-0028).
pub(crate) const DOC_TOKEN_SEP: char = '\u{1f}';

// At or below this document count a multi-vector search scores every document
// exactly; above it, nearest-neighbour candidate generation over the token pool
// kicks in (mirrors the single-vector planner's full-scan threshold).
pub(crate) const MULTIVECTOR_EXACT_DOC_THRESHOLD: usize = 10_000;

// Per-query-token candidate breadth for the large-corpus path: each query token
// retrieves about `k × this` nearest token rows before the documents are unioned.
pub(crate) const MULTIVECTOR_CANDIDATE_FACTOR: usize = 4;

// The external id of a multi-vector document's `ordinal`-th token row.
pub(crate) fn token_id(doc_id: &str, ordinal: usize) -> String {
    format!("{doc_id}{DOC_TOKEN_SEP}{ordinal}")
}

// Split a token row's external id back into its document id and ordinal, or `None`
// if it is not a token id. Splits from the right, so a document id (which cannot
// contain the separator) is recovered intact.
pub(crate) fn parse_token_id(ext: &str) -> Option<(&str, u32)> {
    let (doc, ordinal) = ext.rsplit_once(DOC_TOKEN_SEP)?;
    Some((doc, ordinal.parse().ok()?))
}

// Reject the single-vector API on a multi-vector collection.
