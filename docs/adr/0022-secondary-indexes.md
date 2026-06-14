# ADR-0022: Secondary indexes

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

Phase-1 filtering is **post-filter only**: vector search returns candidates and a
predicate ([`quiver_query::Filter`]) is evaluated against each candidate's
payload. That is correct but pays the cost of fetching and scoring matches that
the filter then discards, and it cannot answer a highly selective filter cheaply.
The row-addressed storage (ADR-0020) and roaring tombstones (ADR-0021) give us a
stable per-segment row model; this ADR adds the per-segment **secondary index**
(`.sec`) that turns an indexable predicate into a set of rows, the substrate the
hybrid-search planner builds on.

## Decision

**A filterable schema, fixed at creation.** A collection declares `filterable`
fields — each a dot-path plus a type, `Keyword` (exact-match string) or `Numeric`.
The schema is immutable, so every segment indexes exactly those fields.

**Per-segment `.sec`, built at flush.** When a segment is sealed (and rebuilt at
compaction), the engine parses each row's JSON payload, extracts each filterable
field, and writes `seg-NNN.sec`. The `.sec` is immutable like `.vec`/`.pay`/`.dir`
(deletes are handled by the `.del` bitmap and the primary index, never by
rewriting `.sec`). A collection with no filterable fields writes no `.sec`.

**Order-preserving keys.** Each field is stored as sorted `(key → roaring bitmap
of rows)`. Keys are encoded so byte-lexical order equals value order: UTF-8 for
keywords, and a sign-flipped big-endian encoding for numerics (so negatives sort
correctly). Equality is then a binary search and a range is a contiguous scan;
the same encoding evaluates predicates against un-indexed active rows, keeping the
two paths in exact agreement.

**Query = per-segment union, liveness via the primary index.** `Store::matching_ids`
queries each segment's `.sec`, then keeps a hit only if the primary index still
points at that exact `(segment, row)`. This single check subsumes both deletes
(the id is gone from the index) and updates (the id points at a newer segment,
whose own `.sec` reflects the new value) — so an id is counted once, with its
live payload's membership. Active (un-checkpointed) rows are scanned directly.
The result is a sorted, de-duplicated id set.

**`matching_ids` is a public primitive; the planner builds on it.** This ADR
shipped the index and the query primitive (a `pub` API, fully tested). The query
planner now lives in `quiver-embed`: it decomposes a `Filter` into the indexable
predicates (`And` intersects, `Or` unions, anything unrepresentable widens to
unconstrained — always a sound superset), resolves the candidate ids through
`matching_ids`, and when the set is selective (below a full-scan threshold) scans
those rows exactly instead of post-filtering ANN hits. Both arms re-check the
full `Filter`, so results are exact. Declaring `filterable` fields is exposed over
REST/gRPC (and the MCP server), so hybrid search is reachable end to end.

## Consequences

- **+** Selective filters resolve to a small id set without scanning vector
  results; the planner can pre-filter; the index is encrypted and CRC-checked like
  every other paged file; arbitrary numeric ranges and keyword equality/`in` are
  answered from sorted keys.
- **−** `quiver-core` now parses JSON at flush (the payload was previously opaque)
  — a deliberate, spec'd coupling (the store owns field extraction). A non-JSON
  payload simply contributes no indexed fields.
- **−** Pre-filter soundness assumes payloads are valid JSON and fields are
  declared; the embeddable API validates payloads as JSON, and the planner must
  fall back to post-filtering for non-indexed fields. A linear scan of distinct
  keys answers a range (fine for v1; a key binary-search bound is a later tweak).
- **−** Keyword indexing covers JSON strings (not bools); a bool predicate is
  post-filtered. Extendable without a format change.

## Alternatives considered

- **Hash-only equality index** — rejected: cannot answer ranges; order-preserving
  keys cost nothing extra and unlock `<`, `>`, `between`.
- **One global index across segments** — rejected: breaks segment immutability and
  cheap compaction; per-segment indexes merge at query time.
- **Index every payload field automatically** — rejected: unbounded cost; the
  schema declares what is worth indexing.
- **Keep filtering purely post-hoc** — rejected: the memory-frugal disk path needs
  a way to avoid materializing discarded candidates for selective filters.
