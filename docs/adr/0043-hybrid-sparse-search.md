# ADR-0043 — Hybrid (dense + sparse) search with RRF fusion

**Status:** Proposed
**Date:** 2026-06-19
**Deciders:** Achref Soua

---

## Context

The state-of-Quiver assessment names **hybrid / sparse search** as Quiver's single
biggest capability gap versus Qdrant, Weaviate, and Milvus for RAG. Today Quiver
does excellent *dense* ANN with exact metadata pre-filtering, but it has no way to
combine a dense embedding with a **sparse** signal — learned sparse vectors
(SPLADE, BGE-M3) or classic lexical term weights — which is what makes hybrid
retrieval beat dense-only on out-of-domain queries, rare terms, and exact-match
recall.

Two design forces:

1. **Don't break the crash gate.** Quiver's contract (ADR-0004/0005/0020) is "the
   store is the source of truth; every index is *derived* and rebuilt on open, so
   `kill -9` mid-write never corrupts." Any hybrid feature must preserve this.
2. **Don't fragment the architecture.** Sparse vectors should reuse the existing
   row store and the derived-index discipline rather than introduce a parallel
   storage engine.

## Decision

Add **sparse vectors** and **hybrid search** to the embeddable engine, fused with
**Reciprocal Rank Fusion (RRF)**, with no on-disk format change.

### Sparse vectors (no format change)

A point may carry a sparse vector in its payload under the reserved key
`__quiver_sparse__`:

```json
{ "__quiver_sparse__": { "indices": [4, 17, 2090], "values": [0.7, 1.2, 0.3] } }
```

Because it lives in the payload, sparse vectors ride the existing encrypted row
store — **no new column, no on-disk format version bump, crash gate untouched**.
`indices` are u32 dimension ids (a sparse vocabulary can be huge, e.g. 30k–250k);
`values` are the weights. The pair is validated (equal length, sorted/unique
indices) on upsert.

### Derived inverted index

A collection that has seen any sparse vector maintains an **in-memory inverted
index** (`dim → [(row, value)]`), built by scanning payloads on open and updated
incrementally on upsert/delete — exactly like the other derived indexes, so it
needs no persistence and the crash gate is unchanged. Sparse search accumulates a
dot-product score over the query's nonzero dimensions via the posting lists and
returns the top candidates (a standard term-at-a-time scan).

### Hybrid search + RRF

A new engine call `hybrid_search(collection, dense_query, sparse_query, k, filter,
…)` runs the dense ANN search and the sparse search independently (each honouring
the same exact payload pre-filter), then fuses the two ranked lists with **RRF**:

```
score(d) = Σ_listsᵢ  1 / (k0 + rankᵢ(d))      # k0 = 60 by convention
```

RRF is rank-based, so it needs no score normalisation between the (incomparable)
dense distance and sparse dot-product scales — the property that makes it the
standard, robust hybrid fuser. `k0` is tunable. Either query may be omitted to get
pure dense or pure sparse search through the same path.

### Surfaces (this ADR vs fast-follow)

- **This ADR:** the embeddable engine (`quiver-query` RRF + sparse types,
  `quiver-embed` inverted index + `hybrid_search`), the **REST** endpoint, and the
  **Python SDK** (sync + async) — the RAG-critical path — all tested, with the
  cost limits (ADR-0040) extended to the sparse query (cap nonzero terms).
- **Fast-follow (tracked, same ADR):** gRPC + MCP + TypeScript parity, and a
  **BM25 / full-text** path (a tokenizer that *produces* sparse term-weight vectors
  from a text field, so lexical search reuses this exact machinery) — deferred so
  this change stays reviewable, not because it is out of scope.

### Cost limits

The sparse query's nonzero-term count is bounded by a new `max_sparse_terms`
(ADR-0040 family, generous default) so a pathological query can't blow up the
posting-list scan.

## Consequences

- Quiver gains genuine hybrid retrieval — dense + learned-sparse/lexical, fused by
  the industry-standard RRF — closing the headline RAG gap, with the engine and
  REST/Python surfaces shipping first.
- No on-disk format change; the inverted index is derived and rebuilt on open, so
  the `kill -9` crash gate is untouched and no migration is needed.
- Sparse vectors in payload cost a little JSON overhead; a native sparse column is
  a possible future optimisation (measured, ADR-gated) but not needed for
  correctness.
- BM25/full-text becomes a thin layer on top (tokenizer → sparse vector), not a
  second retrieval engine.

## Alternatives considered

- **A native on-disk sparse column.** Rejected for now: it changes the on-disk
  format and touches the crash gate for a payload-shaped optimisation; the
  derived-index approach delivers the capability with zero format risk, and a
  native column can follow if measurement justifies it.
- **Score-normalisation fusion (weighted sum of normalised dense+sparse scores).**
  Rejected as the default: it requires per-query min/max normalisation across
  incomparable scales and is brittle; RRF is rank-based and robust. (A weighted
  variant can be offered later.)
- **Bolt on an external lexical engine (e.g. Tantivy).** Rejected: adds a second
  storage/index engine and a dependency, against the "one derived-index store"
  architecture; learned-sparse + a built-in tokenizer cover the need.
