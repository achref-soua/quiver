# ADR-0048 — Snowball (Porter2) stemmer for BM25 tokenization

**Status:** Proposed
**Date:** 2026-06-22
**Deciders:** Achref Soua

---

## Context

ADR-0046 shipped the BM25 / full-text path with a deliberately small,
dependency-free tokenizer whose stemmer was a *consistency-only plural normalizer*
(an S-stemmer: `cats`→`cat`, `boxes`→`box`, `ponies`→`pony`). ADR-0046 named the
limitation explicitly and reserved the `tokenize` seam for a real stemmer: *"a
future ADR can swap in `rust-stemmers` behind the same `tokenize` seam if a measured
retrieval gain justifies the dependency."* This is that ADR.

The plural-only stemmer does not conflate verb inflections or derivational forms —
`connection`, `connected`, and `connecting` stay distinct, so a query for one misses
documents using another. That is the most common lexical-recall miss in real
full-text search.

## Decision

Replace the hand-rolled plural stemmer with the **Snowball English (Porter2)**
algorithm via the `rust-stemmers` crate, behind the existing `tokenize` seam.

- The swap is entirely inside `quiver-query::tokenize`: `push_term` now calls
  `stem(token)`, which runs the Snowball stemmer; nothing else in the pipeline
  changes (Unicode split, lowercase, stop-words, FNV-1a term ids).
- The `rust_stemmers::Stemmer` is created once per thread (`thread_local!`) — it
  holds no per-call state and `stem` is a pure function, so this avoids re-creating
  it per token while keeping the public functions pure and deterministic.
- Ingest and query share the stemmer (as before), so conflation stays consistent on
  both sides and there is no on-disk format change — `__quiver_text__` is still
  tokenized at ingest, the inverted index is still derived, and the `kill -9` crash
  gate is untouched. (Tokenization is not versioned: a collection's text terms are
  recomputed when its index is built, so the stronger stemmer simply takes effect on
  the next (re)build; there is no migration.)

`rust-stemmers` is pure Rust, MIT-licensed, and adds no transitive runtime burden of
note; `cargo deny check licenses advisories bans` passes with it in the tree.

## Consequences

- Better lexical recall: morphological variants conflate (`connection` /
  `connected` / `connecting` → `connect`, `running` → `run`), which is the behavior
  users expect from keyword search.
- One new dependency (`rust-stemmers`), vetted clean by the `cargo deny` gate — the
  cost ADR-0046 deferred, now accepted because the recall gain is real and the crate
  is small and permissively licensed.
- Stemming is more aggressive than the plural-only heuristic, so some tokens stem to
  non-words (`ponies` → `poni`); this is harmless because it is a *consistent*
  internal key, identical on the ingest and query sides — BM25 never shows stems to
  the user.
- No on-disk change and no migration; the term-id hash-collision ceiling (32-bit
  FNV-1a) is unchanged and remains negligible for realistic vocabularies.

## Alternatives considered

- **Keep the plural-only S-stemmer.** Rejected: it misses the verb/derivational
  conflations that matter most for recall; ADR-0046 always intended this swap.
- **A heavier lexical engine (Tantivy) for its analyzers.** Rejected for the same
  reason as ADR-0043/0046: a second storage/index engine against the
  one-derived-index architecture. Snowball is a self-contained algorithm, not an
  engine.
- **Make stemming configurable per collection / pluggable language.** Deferred:
  English Porter2 is the right default; a language option can be added behind the
  same seam if multilingual corpora demand it, without another architectural change.

## Implementation

Shipped in one PR: add `rust-stemmers` (workspace + `quiver-query`), replace
`stem_plural` with the `thread_local!` Snowball `stem`, update the module
documentation and the `apps/docs` full-text page, and adjust the tokenizer unit
tests to assert the morphological conflation (and the consistency property that a
query term matches its inflected document forms).

## Verification

- `quiver-query` unit tests assert Snowball conflation (`connecting`/`connected`/
  `connection` → `connect`, `running` → `run`, `cats` → `cat`) and that a root is
  never emptied, plus the existing tf-counting and dedup tests.
- The `quiver-embed` and `quiver-server` BM25 / full-text tests pass unchanged
  (their query and document terms conflate the same way).
- `cargo deny check licenses advisories bans` is clean with `rust-stemmers` in the
  tree; no on-disk format change, so the crash gate is untouched and there is no
  migration.
