# ADR-0046 — BM25 / full-text over the sparse path

**Status:** Proposed
**Date:** 2026-06-22
**Deciders:** Achref Soua

---

## Context

ADR-0043/0045 gave Quiver hybrid (dense + sparse) search, a derived inverted index,
and parity across every surface — but the sparse side still requires the caller to
supply a `SparseVector` (learned-sparse from SPLADE/BGE-M3, or hand-built lexical
weights). The most common lexical need — *"give me text, search by words"* — is the
last open Tier-1 loop (roadmap #4). It is also the half of hybrid most users expect:
BM25-style keyword retrieval fused with dense vectors.

The constraints that shaped ADR-0045 still hold: don't fragment the architecture
(no second storage/index engine, no Tantivy dependency), keep the engine
model-agnostic for *embeddings*, preserve the derived-index discipline and the
crash gate, and add no heavy dependency the `cargo deny` gate would have to vet.

Two facts make this a thin layer rather than a new engine:

1. The derived inverted index (ADR-0045) already holds exactly the corpus
   statistics BM25 needs: **document frequency** per term (a posting list's
   length), **N** (the live document count), and — with one small addition —
   **document length** and the average. So BM25 is a *scoring function over the
   existing index*, not new storage.
2. A tokenizer that turns text into term-weight pairs *produces a `SparseVector`*,
   so ingestion and the inverted index are reused verbatim.

## Decision

Add a built-in **tokenizer** and a **BM25 scoring mode** over the existing sparse
inverted index, exposed as a *text* convenience on top of the sparse path. No new
storage, no new index, no new dependency.

### Tokenizer (`quiver-query`, dependency-free)

A small, deterministic tokenizer: Unicode-aware splitting on non-alphanumeric
boundaries, lowercasing (Unicode), an optional English stop-word filter, and
**light suffix stemming** (plurals and common verb endings). Each token is mapped
to a `u32` dimension id by a stable hash (FNV-1a, truncated), so a tokenized text
*is* a `SparseVector` whose values are term frequencies — reusing ADR-0043 end to
end.

The stemmer is a deliberately simple, dependency-free heuristic (a documented
ceiling): it is not a full Snowball/Porter implementation, and a future ADR can
swap in `rust-stemmers` behind the same `tokenize` seam if a measured retrieval gain
justifies the dependency. Hash collisions are possible but astronomically rare for
realistic vocabularies and are accepted (documented), exactly as learned-sparse
vocabularies already collide by design.

### BM25 scoring over the inverted index (`quiver-query`)

The `SparseInvertedIndex` (ADR-0045) gains:

- per-document **length** tracking (the sum of a document's term frequencies) and a
  running total, so `avgdl` is O(1);
- a `bm25_search(query_terms, k1, b)` method that, for each query term, walks its
  posting list and accumulates the Okapi BM25 score

  ```
  Σ_{t∈q}  IDF(t) · ( tf_{t,d}·(k1+1) ) / ( tf_{t,d} + k1·(1 − b + b·|d|/avgdl) )
  IDF(t) = ln( 1 + (N − df(t) + 0.5) / (df(t) + 0.5) )
  ```

  with the standard defaults `k1 = 1.2`, `b = 0.75`. This sits *beside* the existing
  dot-product `search` (learned-sparse keeps using dot); a query picks the scorer.

BM25 is rank-based for fusion purposes, so it drops into the same RRF fusion as the
dot-product sparse path — hybrid `dense ⊕ BM25` works through the existing planner.

### Text ingestion and query (`quiver-embed` + surfaces)

- **Ingest:** a point may carry a `text` field; the engine tokenizes it into a tf
  `SparseVector` stored under the existing `__quiver_sparse__` key — so it rides the
  same payload, the same inverted index, the same crash gate, with **no on-disk
  format change**. (A caller may still supply an explicit sparse vector instead.)
- **Query:** a `query_text` parameter (REST/gRPC/MCP/SDKs) is tokenized server-side
  into query terms and scored with BM25 over the index; it fuses with a dense query
  exactly like a supplied sparse vector does. The sparse-term cost limit
  (`QUIVER_MAX_SPARSE_TERMS`) bounds the tokenized query.

The tokenizer is deterministic and pure — not an ML model — so embedding the text
path keeps the engine model-agnostic for *vectors* while removing the "I have to
build sparse vectors myself" friction for lexical search.

## Consequences

- Quiver gains real keyword/full-text retrieval (BM25) and `dense ⊕ BM25` hybrid
  from text alone, reusing ADR-0043/0045 — no second engine, no new dependency.
- The inverted index carries one extra `u32` per document (its length) and a running
  total; BM25 scoring is the same posting-list walk as the dot path with a different
  per-posting term.
- No on-disk format change; tokenization is deterministic and the index stays
  derived, so the crash gate is untouched and there is no migration.
- The simple stemmer is a known quality ceiling; the `tokenize` seam isolates it for
  a later Snowball upgrade. Term-id hashing can collide (documented, negligible).

## Alternatives considered

- **Bolt on Tantivy (or another lexical engine).** Rejected (as in ADR-0043): a
  second storage/index engine and a large dependency, against the one-derived-index
  architecture. BM25 over the existing inverted index covers the need.
- **A full Snowball/Porter stemmer dependency now.** Deferred: the dependency-free
  light stemmer is enough for a first cut; swap it in behind `tokenize` when a
  measured gain justifies the `cargo deny` vetting.
- **Client-side tokenization only (no `text`/`query_text`).** Rejected as the
  default: it would re-impose the "build your own sparse vector" friction this ADR
  exists to remove; callers who *want* full control can still pass a `SparseVector`.
- **Storing BM25 weights at ingest instead of raw tf.** Rejected: BM25's IDF and
  length-normalization depend on corpus-wide stats that change with every write;
  storing raw tf and scoring at query time keeps results correct under incremental
  upsert/delete (the same reason the index is derived).
