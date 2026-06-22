# ADR-0045 — Hybrid everywhere + fast ingest (inverted index, parity, bulk upsert)

**Status:** Proposed
**Date:** 2026-06-22
**Deciders:** Achref Soua

---

## Context

ADR-0043 shipped hybrid (dense + sparse) search with RRF fusion, but deliberately
scoped the first cut to the engine, REST, and the Python SDK, and left three loops
open. The post-v0.18.0 enhancement review and the deep benchmark (ADR-0041) sharpen
them into the v0.19.0 slice — *"hybrid, everywhere + fast ingest"*:

1. **The sparse side scans the store O(N) per query.** ADR-0043 described a derived
   inverted index but implemented the correctness-first version: `sparse_ranked_ids`
   loads every row, parses its payload, and scores. Correct under the incremental
   upsert/delete path, but linear in collection size — the wrong asymptote for the
   one structure ADR-0043 already named.
2. **Hybrid is REST + Python only.** It is the headline v0.18.0 feature, yet gRPC,
   MCP, and the TypeScript SDK can't reach it — a parity gap, not a design gap.
3. **The worst benchmark column is the REST upload (build) time, not engine speed.**
   ADR-0038 already gave `upsert_batch` a single WAL `fdatasync`, but the in-memory
   index is still maintained **one point at a time** even inside a batch. For a
   fresh bulk load (the benchmark's workload) that is N incremental inserts where a
   single build pass would be far cheaper (k-means once for IVF, one graph build for
   Vamana), and the per-request `max_batch_size` (1000) caps how much a client can
   hand us at once.

The non-negotiables hold: the store is the source of truth, every index is derived
and rebuilt on open, `kill -9` mid-write never corrupts, and we don't fragment the
architecture or fabricate a number.

## Decision

### 1. Derived sparse inverted index (closes ADR-0043's open loop)

Implement the inverted index ADR-0043 described, in `quiver-query` as a pure,
store-free `SparseInvertedIndex`:

- Postings are `dim → { doc-slot → weight }`. Document ids are **interned** to dense
  `u32` slots (a `Vec<String>` + a free list), so a posting carries a 4-byte slot,
  not a cloned id String — the memory-frugal representation that matches Quiver's
  wedge. A per-slot dim list lets `upsert`/`remove` clean the prior postings in
  O(terms) hash operations, so there are **no tombstones, no generations, and no
  compaction pass** — memory stays tight under churn.
- `search(query)` is term-at-a-time: walk only the query's nonzero dims' posting
  maps, accumulate the dot-product score per slot, and return `(id, score)` sorted
  by score then id. The caller (the engine) re-checks the exact payload filter on
  the ranked ids and truncates to depth — so low-scored rows never load a payload.

`quiver-embed` holds an `Option<SparseInvertedIndex>` per collection handle, built
when a collection's index is rebuilt (scanning payloads for `__quiver_sparse__`,
exactly as the dense rebuild already scans the store) and maintained incrementally
on the same `upsert`/`upsert_batch`/`delete` paths. `sparse_ranked_ids` uses the
index when present and **falls back to the existing store scan** when it isn't
(client-side-encrypted collections, or any not-yet-built handle) — the scan stays
as the correctness backstop. The index is in-memory and derived: **no on-disk format
change, crash gate untouched**, same discipline as every other Quiver index.

### 2. Hybrid parity across gRPC, MCP, and TypeScript

Expose the existing `hybrid_search` on the three surfaces that lack it:

- **gRPC:** a `HybridSearch` RPC + `HybridSearchRequest`/`HybridSearchResponse`
  (dense vector optional, sparse vector optional, filter JSON, `k`, `ef_search`,
  `rrf_k0`, `with_payload`/`with_vector`) on the tonic handler, delegating to the
  same `AppState::hybrid_search` the REST route uses.
- **MCP:** a `hybrid_search` tool with the same arguments, so an agent can do hybrid
  retrieval, not just dense.
- **TypeScript SDK:** a `hybridSearch` method + `SparseVector` type mirroring the
  Python client (which already has it).

No engine change — pure surface wiring, each tested.

### 3. Bulk ingest (single index-build pass)

Add an explicit bulk path for the load-then-query workload:

- `quiver-embed::upsert_bulk` writes the batch through the existing single-`fsync`
  `Store::upsert_batch`, then — instead of N incremental index inserts — marks the
  handle **stale** so the next search does **one** `rebuild_index` (a single build
  pass over the whole collection). This is strictly the bulk-load optimization; the
  steady-state `upsert_batch` keeps its incremental maintenance for
  query-after-each-write latency.
- REST `POST /v1/collections/{name}/points:bulk` (the AIP-136 custom-method spelling)
  routes to it, bounded by a new, larger `max_bulk_batch_size` limit (default
  50,000; `QUIVER_MAX_BULK_BATCH_SIZE` override) and the existing request-body cap,
  rather than the 1000-point `max_batch_size` of the steady-state endpoint.

A client-streaming gRPC `Upsert` is the natural next step for unbounded loads; it is
left to a fast-follow so this slice stays reviewable (the REST bulk path already
reverses the benchmark's build-time column for batched HTTP loaders).

## Consequences

- The sparse half of hybrid drops from O(N-rows) to O(Σ posting-list lengths over
  the query's terms) per query — the asymptote a vector DB needs at scale — while
  the store scan remains as a proven fallback.
- Hybrid becomes reachable from every Quiver surface; the SDKs and an MCP agent get
  feature parity with REST + Python.
- Bulk loads pay one index build instead of N incremental inserts, addressing the
  benchmark's worst column, with no change to steady-state write semantics.
- No on-disk format change anywhere; the inverted index is derived and rebuilt on
  open, so the `kill -9` crash gate is untouched and there is no migration.
- The inverted index costs memory proportional to the number of (doc, term) pairs.
  Interning ids to u32 slots keeps that tight; a collection with no sparse vectors
  pays nothing (the handle's `Option` stays `None`).

## Alternatives considered

- **Tombstone/generation inverted index** (skip dead postings at query, compact
  later). Rejected: it trades query work and a compaction pass for O(1) deletes, but
  Quiver's wedge is memory-frugality — the O(terms) hash-remove keeps postings exact
  and tight with no reclamation pass, which fits better and is simpler to reason about.
- **Persisting the inverted index.** Rejected: it would join the crash gate for a
  derived structure that rebuilds from the store cheaply on open, against ADR-0004/
  0020. Same call as every other Quiver index.
- **Raising `max_batch_size` instead of a separate bulk endpoint.** Rejected: the
  steady-state endpoint maintains the index incrementally (right for small writes);
  a distinct `:bulk` method makes the deferred-rebuild, large-batch semantics
  explicit and keeps the two cost limits independent.
- **A native on-disk sparse column.** Still rejected (ADR-0043): payload-shaped data
  doesn't justify a format change and crash-gate work; the derived index delivers
  the speed.
