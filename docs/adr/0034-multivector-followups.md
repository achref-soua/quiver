# ADR-0034: Multi-vector follow-ups — incremental maintenance, native document rows, ColBERTv2/PLAID compression

- **Status:** Accepted
- **Date:** 2026-06-17
- **Deciders:** Achref Soua

## Context

ADR-0028 shipped late-interaction (ColBERT / MaxSim) retrieval in `v0.7.0` on a
deliberately conservative foundation: a document is a **group of ordinary rows**
(one per token vector, ids `<doc-id><US><ordinal>`, payload on the anchor token),
the token pool *is* the collection's ANN-indexed set, and document grouping is
derived in memory — so there was **no on-disk format change** and the `kill -9`
crash gate was untouched by construction. ADR-0028 §6 and its Alternatives
explicitly deferred three optimizations, "each [behind] their own ADRs if
measurement shows they are needed." This is that ADR; it takes all three.

1. **Incremental index maintenance.** Today `upsert_document` / `delete_document`
   write the token rows to the store and mark the collection `stale`, so the next
   `search_multi_vector` rebuilds the whole token-pool ANN index from scratch —
   the `O(N)` cost the rest of the engine has already shed (IVF ADR-0023, HNSW
   ADR-0026, and now the Vamana graph family ADR-0033). It is the last
   write-then-rebuild path left, and with the graph family made incremental in
   `v0.13.0` every underlying index now supports incremental insert + delete, so
   the token pool can be maintained in place.

2. **Native variable-stride document rows.** Token-as-row inflates the id space
   ~`N`× (`N` = tokens/doc) and fans a document write/delete out to `N` row
   operations. ADR-0028's rejected alternative — one document = one
   variable-stride row holding its `[tokens × dim]` matrix and one payload — gives
   document-shaped storage and locality for the re-rank, at the cost of a
   variable-stride segment format that **joins the crash path**. ADR-0028 flagged
   that it "adds risk without removing work," because candidate generation still
   needs a token-pool index. That caveat stands and shapes the design below.

3. **ColBERTv2 residual compression + PLAID centroid pruning.** A ColBERT corpus
   is a large pool of low-dim vectors — Quiver's frugality wedge. ColBERTv2
   compresses each token vector as *(nearest centroid id) + (quantized residual)*;
   PLAID prunes candidate generation by scoring centroids first and only touching
   the token lists of the most promising ones. Together they are the
   state-of-the-art memory + latency story for late interaction.

The hard constraints are unchanged from ADR-0028: do not regress durability
(ADR-0005) or encryption-at-rest (ADR-0010), and reuse the existing engine rather
than fork a parallel one.

## Decision

Take all three, **sequenced by risk** so the low-risk, derived-index work lands
first and the on-disk format change lands last behind its own crash-safety proof.
Each part is opt-in / backward-compatible; the `v0.7.0` token-as-row path remains
the default and is never broken.

### Part A — Incremental multi-vector index maintenance (no on-disk change)

Dispatch token-row writes to the underlying ANN index incrementally instead of
marking the collection `stale`, exactly as single-vector `upsert` / `delete` now
do (ADR-0023/0026/0033):

- `upsert_document` — for a re-upsert, tombstone the document's prior token
  internal ids in the index; then insert each new token row under a fresh internal
  id (HNSW absorbs new ids; IVF inserts; a `FreshVamana`/`FreshDiskVamana` appends
  to its delta). The derived `doc-id → token-count` map is already maintained
  eagerly; the token internal-id bookkeeping joins it.
- `delete_document` — tombstone all of the document's token internal ids in the
  index (`O(tokens)`), no rebuild.
- Consolidation is the **underlying index's own** existing threshold (the IVF
  rebalance, the HNSW 0.2 deleted fraction, the graph `GRAPH_REBUILD_PENDING_FRACTION`):
  a graph/HNSW token pool that crosses it marks the collection `stale` and the
  next search rebuilds — the same amortized path single-vector collections use.

The index stays derived and rebuilt-from-store on open, so **the crash gate is
untouched by construction** (ADR-0023's stance). This is the headline, lowest-risk
win, and it composes directly with the now-incremental graph family.

### Part C — ColBERTv2 residual compression + PLAID pruning (RAM-resident, opt-in)

A new `quiver-index` token-pool structure for `multivector` collections, opt-in via
the descriptor:

- **Residual quantization.** Train `k` coarse centroids over the token pool (the
  existing seeded `kmeans`), assign each token its nearest centroid, and encode the
  **residual** (token − centroid) with the existing `ProductQuantizer`. RAM holds
  centroids + per-token *(centroid id, PQ residual code)*; the exact token vectors
  stay on the encrypted store for the MaxSim re-rank (the ADR-0019 pattern applied
  to tokens).
- **PLAID candidate generation.** A query token scores the centroids first; only
  the token lists under the top centroids are expanded (centroid pruning), then
  approximate MaxSim over the residual codes selects candidate documents, and the
  exact re-rank reads only those documents' token vectors. Recall is tuned by the
  number of probed centroids and the candidate cap, mirroring `nprobe`/`ef_search`.

Reuses `kmeans`, `ProductQuantizer`, and `max_sim`; no new cryptography and no
on-disk format change (codes are derived, RAM-resident, rebuilt on open like any
index). Off by default; the uncompressed token-pool path (Part A) remains.

### Part B — Native variable-stride document rows (on-disk change, crash-gated)

An opt-in storage mode where a document is **one variable-length row**: its slot
holds `[count: u32][count × dim × f32]` and one payload, instead of `N`
fixed-stride rows. Because this changes the segment from fixed- to variable-stride
— it **joins the `fsync`/crash path** — it gets the full ADR-0025 treatment, not a
derived-index shortcut:

- The row write is journaled through the WAL exactly like a fixed row; the segment
  gains a per-row length/offset so a document row is self-describing; recovery
  replays the WAL tail; and the crash-injection suite
  (`crates/quiver-core/tests/crash_recovery.rs`) is extended to SIGKILL mid
  variable-row write and across the directory swap, asserting no torn document row
  is ever read and acknowledged writes survive.
- **Honest caveat, carried from ADR-0028:** candidate generation still needs a
  **token-pool index**, so this mode does not remove the token index — it changes
  how the exact token vectors are *stored and gathered* for the re-rank (one
  contiguous read per document instead of `N` row reads) and collapses the id space
  to one id + one payload per document. The token-pool index (Parts A/C) is built
  by scanning the document rows' token matrices. If, in implementation, this proves
  to "add risk without removing work" (ADR-0028's concern) without a measured
  re-rank/locality win, it stays **opt-in and clearly labelled experimental**, and
  may be deferred to a later increment rather than forced — recorded honestly here
  rather than shipped on faith.

## Scope & sequencing

`v0.14.0` ships, in this order (each its own PR set, `just verify` green per PR):

1. **Part A** — incremental maintenance (derived, no on-disk change). Lands first.
2. **Part C** — ColBERTv2/PLAID compression (RAM-resident, opt-in, no on-disk
   change).
3. **Part B** — native variable-stride document rows (on-disk change), last and
   behind its crash-gate extension; shipped only if the crash proof is green and
   the re-rank/locality benefit is real, else deferred with a note.

The REST/gRPC/MCP surface and the SDKs gain the opt-in flags (compression mode;
native-row mode) additively; the `v0.7.0` document API is unchanged in behaviour.

## Crash-safety

- **Parts A and C are in-memory and derived** — the token-pool index and the
  ColBERTv2 codes are rebuilt from the durable store on open, so they never join
  the `fsync` path; the `kill -9` gate is untouched by construction (ADR-0023/0028
  stance).
- **Part B joins the durability path** and is the one piece that extends the crash
  gate, following ADR-0025: WAL-journaled variable rows, a self-describing segment
  layout, WAL-tail recovery, and new crash-injection points. It does not ship until
  that proof is green.

## Consequences

- **+** The last write-then-rebuild path is gone: a document upsert/delete is
  size-independent, so streaming ColBERT corpora are practical — finishing the
  incremental-maintenance story across every index family.
- **+** ColBERTv2 + PLAID make the token pool genuinely frugal (centroid + residual
  codes in RAM, exact vectors on encrypted SSD) and faster (centroid pruning) — the
  wedge ADR-0028 promised, now realized.
- **+** Native document rows (if shipped) restore a document-shaped id/payload model
  and one-read re-rank locality.
- **−** More moving parts in the multi-vector path: incremental token bookkeeping,
  a compressed token-pool structure, and (Part B) a variable-stride segment format
  that genuinely joins the crash path — the highest-risk surface in the engine,
  which is why it is sequenced last and gated.
- **−** Part B does not remove the token-pool index (ADR-0028's caveat), so its
  payoff is locality/id-space, not work elimination; it must justify its risk by
  measurement or be deferred.

## Alternatives considered

- **Ship only Part A** — the lowest-risk, highest-value slice; rejected as the
  *whole* of `v0.14.0` only because the owner scoped all three follow-ups together,
  but Part A is sequenced first and stands alone if B/C slip.
- **Variable-stride rows instead of token-as-row from the start** — already
  rejected by ADR-0028 (joins the crash path, still needs a token index); revisited
  here as an opt-in mode with the ADR-0025 crash treatment rather than a default.
- **A bespoke ColBERT storage engine** (per-document blocks + residual + PLAID as
  one parallel subsystem) — rejected as in ADR-0028: layer the optimizations onto
  the existing engine instead of forking it.

## Implementation

`v0.14.0` shipped Parts A and C (and the wire/SDK surface for C); Part B is
**deferred**, honestly, per the ship-or-defer commitment in the Decision above.

### Part A — incremental multi-vector index maintenance (shipped)

`upsert_document` / `delete_document` no longer mark the collection `stale` for a
full token-pool rebuild. The per-point index dispatch was extracted into shared
free functions `index_upsert_point` / `index_delete_point` in
`crates/quiver-embed/src/lib.rs` (next to `rebuild_index`), used by **both** the
single-vector `upsert` / `delete` path and the per-token-row path of the document
API. A re-upsert tombstones the document's prior token internal ids and inserts
the new token rows under fresh ids; a delete tombstones all of a document's token
ids. Consolidation is the underlying index's own existing churn threshold (IVF
rebalance, HNSW `0.2` deleted fraction, graph `GRAPH_REBUILD_PENDING_FRACTION`).
The index stays derived and rebuilt-from-store on open, so the `kill -9` crash
gate is untouched by construction. The exact-scan path (≤ `MULTIVECTOR_EXACT_DOC_THRESHOLD`
= 10 000 docs) ignores the index, so incremental token maintenance is observable
above that threshold (candidate generation); HNSW maintains incrementally even
below it.

### Part C — ColBERTv2 residual compression + PLAID pruning (shipped)

`quiver-index::colbert::ColbertIndex`: coarse `kmeans` centroids over the token
pool, each token stored as *(nearest centroid id, PQ residual code)* with the
existing `ProductQuantizer` **trained under `Metric::L2`** for faithful residual
reconstruction (the collection's cosine/dot metric is applied only at scoring).
`search` does PLAID centroid pruning (score centroids first, expand only the top
ones' token lists). It is RAM-resident and derived — in-memory `insert` plus an
`O(1)` deletion set, rebuilt from the store on open — so no on-disk format change
and the crash gate is untouched. Wired through a new `IndexKind::Colbert`
(`quiver-core`, `#[non_exhaustive]` + serde snake_case), `CollectionIndex::Colbert`,
and the build / empty / search / validate / incremental-maintenance dispatch in
`quiver-embed`. `validate_index` rejects `colbert` on a single-vector collection.
ColBERT candidate generation only observably engages above the 10 000-document
exact threshold; at or below it the exact MaxSim path serves the query.

The surface for C (the **C3** sub-task) names `IndexKind::Colbert` on every
edge so a `colbert` multi-vector collection is creatable over the wire: the proto
`IndexKind` enum (`INDEX_KIND_COLBERT`), the REST `IndexKindDto`, the gRPC
from/to-proto maps, the MCP `create_collection` schema enum and string match, and
the Python/TypeScript SDK index-kind enums.

### Part B — native variable-stride document rows (deferred)

Deferred to a later increment, recorded here rather than shipped on faith, for two
reasons established during implementation review:

1. **The payoff is locality / id-space, not work elimination.** As the Decision
   and ADR-0028 state, candidate generation still needs a token-pool index, so
   Part B changes only how the exact token vectors are *stored and gathered* for
   the re-rank — it removes no work, and the shipped token-as-row path is already
   correct and crash-safe by construction.
2. **Its ship precondition can only be met on owner reference hardware.** The
   Decision gates Part B on a *measured* re-rank/locality benefit. Quiver's
   benchmark numbers are produced exclusively on documented reference hardware and
   are never fabricated, so the measurement that would justify joining the
   `fsync`/crash path with a variable-stride segment is an owner step. Shipping the
   on-disk change without it would be shipping on faith — exactly what this ADR
   committed not to do.

A useful note for whoever picks Part B up: the on-disk machinery it needs already
exists. The row-addressed segment (ADR-0020) stores payloads in a variable-length,
offset-addressed `.pay` heap (`(pay_off, pay_len)` per row in the `.dir`,
`mmap`-read, CRC-paged, WAL-journaled, and already covered by the `kill -9` crash
gate). Part B is therefore a *second* variable-length column following the proven
payload-heap pattern (plus a `count`-prefixed token matrix and the directory /
recovery / crash-injection extensions), not a new fsync surface invented from
scratch. It stays opt-in and the default token-as-row layout is never broken.

## Verification

- **Part A:** `multivector_writes_are_incremental_and_match_a_rebuild`
  (`quiver-embed`) asserts that incremental upsert/delete across IVF/HNSW/Vamana
  (cosine) yields, on reopen, the same results as an authoritative full rebuild.
- **Part C:** `quiver-index` candidate-recall tests on **clustered** data with
  4× over-fetch (random data is a degenerate worst case for any quantizer); the
  `quiver-embed` index-loop tests cover IVF/HNSW/Vamana/Colbert; `validate_index`
  rejects single-vector ColBERT.
- **C3 surface:** `colbert_index_round_trip` (`quiver-server`) creates a `colbert`
  multi-vector collection over **REST and gRPC**, reads the kind back over both
  transports, and asserts `colbert`-without-`multivector` is rejected (400); an
  MCP create test and Python/TypeScript SDK create-collection tests cover the rest
  of the surface. `just verify`, `just test-py`, and `just test-ts` are green.
- **Part B:** not shipped, so no crash-gate claim is made. The crash gate is
  unchanged from `v0.13.0` because Parts A and C are derived and never touch the
  `fsync` path.

## References

- O. Khattab, M. Zaharia. *ColBERT: Efficient and Effective Passage Search via
  Contextualized Late Interaction over BERT.* SIGIR 2020.
- K. Santhanam et al. *ColBERTv2: Effective and Efficient Retrieval via Lightweight
  Late Interaction.* NAACL 2022.
- K. Santhanam et al. *PLAID: An Efficient Engine for Late Interaction Retrieval.*
  CIKM 2022.
- ADR-0005 (durability & recovery), ADR-0008 (quantization), ADR-0019 (disk-resident
  index), ADR-0020 (row-addressed segments), ADR-0023 (incremental IVF), ADR-0025
  (durable incremental index — the crash-gate-extension pattern Part B follows),
  ADR-0026 (HNSW incremental delete), ADR-0028 (multi-vector / late interaction,
  which deferred these three), ADR-0033 (graph FreshDiskANN incremental).
