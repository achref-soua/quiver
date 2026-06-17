# ADR-0033: Graph-index incremental updates (FreshDiskANN StreamingMerge)

- **Status:** Proposed
- **Date:** 2026-06-17
- **Deciders:** Achref Soua

## Context

Incremental maintenance has been brought to every index family except the
graph one. IVF inserts and deletes in place and rebalances locally (ADR-0023),
and persists a durable snapshot (ADR-0025); HNSW soft-deletes in `O(1)` with
search-time tombstone filtering (ADR-0026). The **Vamana** family — the
in-memory `Vamana` graph and the disk-resident `DiskVamana` (ADR-0019), Quiver's
memory-frugal serve path — is still **batch-built**: in `quiver-embed` every
upsert, update, or delete on a graph collection sets the handle `stale`, and the
next search rebuilds the whole graph from `Store::scan` (`rebuild_index`). A
single delete on a 10M-vector disk collection schedules a 10M rebuild; an
interleaved insert/delete stream rebuilds repeatedly. This is the exact
`O(N)`-per-update cost ADR-0023 removed for IVF and ADR-0026 removed for HNSW,
and ADR-0023 explicitly deferred the graph case to "a *different* algorithm
(FreshDiskANN's StreamingMerge / in-place edge repair), not LIRE."

The goal, as in every incremental-index ADR: update cost ~independent of
collection size, recall preserved under a long insert/delete stream, the
memory-frugality wedge intact, and — the hard constraint — the `kill -9` crash
gate (R3, ADR-0005) stays green.

FreshDiskANN (Singh et al. 2021) is the published answer for graphs. It keeps a
large **long-term index** (LTI) — the consolidated graph, read-only — alongside a
small in-memory **temporary index** that absorbs recent inserts, plus a
**deletion set**. Queries search both and merge, skipping deleted ids; a
background **StreamingMerge** periodically folds the temporary index and the
deletion set back into the LTI (re-wiring the neighbors of deleted nodes with
RobustPrune and patching in the new ones) and resets them.

## Decision

Adopt FreshDiskANN's **two-tier + deletion-set** architecture for the Vamana
family, and — exactly as ADR-0023 and ADR-0026 did — **keep the whole structure
in memory and derived**, so the durability path (the only thing the crash gate
protects) is untouched by construction. The store (segments + WAL,
ADR-0020/0021) stays the sole source of truth; the graph, the delta, and the
deletion set are reconstructed from it on open.

1. **Long-term graph (read-only).** The existing batch-built `Vamana` (in
   memory) or `DiskVamana` (the immutable, `mmap`-ed, encrypted disk artifact)
   is the LTI. It is **never mutated in place** — the disk artifact in
   particular keeps its write-once contract (ADR-0019), so no new bytes reach
   the `fsync` path on an incremental write.

2. **In-memory delta graph.** Recent inserts go into a small incrementally-built
   in-memory `Vamana` (new `Vamana::insert`: GreedySearch from the medoid →
   RobustPrune → bidirectional edges with neighbor re-prune, the same primitives
   the batch build runs per node). The delta is the FreshDiskANN temporary
   index; it is searched at full precision, so a just-inserted vector is
   immediately findable without a rebuild.

3. **Deletion set.** A delete records the point's internal id in an in-memory
   `HashSet` in `O(1)` (mirroring ADR-0026). Deleted ids are filtered from
   results; the base graph keeps them as connectivity waypoints until the next
   consolidation. To hold recall while tombstones are present, the base-layer
   search beam is **widened by the live fraction** (`l · total / live`), exactly
   as HNSW does, so roughly `l` *live* candidates survive the filter. An update
   is a delete of the old id plus an insert of the new vector under a fresh
   internal id, so the stale copy in the base graph is tombstoned and the new
   copy lives in the delta.

4. **Query = search both, merge.** A search runs the (widened) beam over the base
   graph and the delta, drops any deleted id, and merges the two candidate lists
   by the collection metric's ordering (`score::ordering_distance`, shared with
   the batch path so the key never drifts), de-duplicating by id and taking the
   top `k`. Empty base (not yet built) or empty delta degenerate cleanly to a
   single-graph search.

5. **Consolidation = StreamingMerge, realized as a derived rebuild.** When the
   pending work — `delta.len() + deleted.len()` — reaches a fraction
   (`GRAPH_REBUILD_PENDING_FRACTION`, 0.2) of the base graph size, the handle is
   marked `stale` and the next access rebuilds the consolidated graph from the
   store's live rows (the existing `rebuild_index` / `build_disk_index` path),
   producing a fresh base with an empty delta and empty deletion set. This is
   FreshDiskANN's StreamingMerge in the derived model: the authoritative live set
   already lives in the store, so re-deriving the LTI from it both reclaims
   tombstones and absorbs the delta — without a bespoke in-place merge of two
   on-disk graphs. It bounds both the deletion-driven recall loss and the cost of
   searching the delta alongside the base.

The dispatch lives in `quiver-embed` (which owns the external↔internal id map and
decides when to consolidate); the two-tier search/merge/delete logic is
encapsulated in `quiver-index` as `FreshVamana` (in-memory base) and
`FreshDiskVamana` (disk base), sharing one `GraphDelta` helper, so it is
unit-tested there against brute force exactly as `Ivf`/`Hnsw` are.

## Scope — what `v0.13.0` ships, and what is deferred

**Shipped:** incremental insert/update/delete for both Vamana graph kinds,
maintained in memory and derived (the crash gate is untouched by construction and
re-run green); `Vamana::insert`; the `FreshVamana`/`FreshDiskVamana` two-tier
wrappers with the deletion set, beam widening, and base+delta merge; recall
preserved under an insert/delete stream (tested against a batch-built / brute-force
reference for both the in-memory and disk bases); the consolidation threshold and
its rebuild; `quiver-embed` dispatching graph upserts/deletes incrementally instead
of marking the collection stale on every write. The base graph stays derived and
rebuilt-on-open.

**Deferred, each behind its own ADR if taken:**

- **In-place on-disk StreamingMerge** — true FreshDiskANN consolidation that
  rewrites the disk graph by patching deleted nodes' neighbors and splicing in the
  delta, *without* a full re-derive from the store. Only worth it once the
  derived rebuild is shown to dominate; it would put the disk graph on the
  durability path and need its own crash-safety proof (the ADR-0025 treatment).
- **Durable delta / deletion-set persistence** — unnecessary here because the
  store already makes every write durable and the delta is re-derived on open; a
  persisted delta would only avoid the open-time rebuild, the same trade ADR-0023
  deferred for IVF.
- **Eager in-place edge repair** of a deleted node's neighbors (re-linking around
  it before consolidation) — the consolidation threshold already bounds the
  degradation, as in ADR-0026.

## Crash-safety

The graph index — base, delta, and deletion set — is in-memory and **derived**: it
is rebuilt from the store on open (ADR-0023's stance), and the disk artifact is
written only by a full (re)build, never mutated in place. The durable record of an
insert or delete is the store's row / tombstone (ADR-0020/0021), `fsync`'d before
the call returns; after a crash and reopen the consolidated graph is re-derived
from those durable rows, which subsumes whatever was in the delta and the deletion
set. No index bytes ever join the `fsync` path on an incremental write, so the
`kill -9` crash gate (R3, ADR-0005) is **untouched by construction** — consistent
with ADR-0023 and ADR-0026, and distinct from the durable-index work of ADR-0025.

## Consequences

- **+** A graph insert/update/delete drops from an `O(N)` rebuild to bounded,
  size-independent work (an incremental insert into a small delta, or an `O(1)`
  tombstone); rebuilds amortize to roughly once per 20% churn. Streaming and
  continuously-updated graph collections become practical — including the
  memory-frugal disk path, which is FreshDiskANN's home ground.
- **+** **Zero crash-gate risk.** The index stays derived; the disk artifact keeps
  its write-once contract; the durability path is untouched. The genuinely hard
  piece (in-place on-disk merge) is sequenced into its own future ADR rather than
  bundled in.
- **+** A just-inserted vector is immediately searchable (full-precision delta),
  and no deleted id is ever returned; recall is held by the live-fraction beam
  widening, mirroring the HNSW path.
- **−** Between consolidations a query searches two graphs and traverses tombstones,
  so search work rises with the pending fraction (bounded by the rebuild threshold
  and the beam cap). The delta is built incrementally (single-pass RobustPrune), a
  hair below a two-pass batch build — re-tightened at each consolidation.
- **−** A restart still rebuilds the graph from the store (today's cost) until an
  in-place on-disk merge is taken; the derived rebuild also rewrites the disk
  artifact on each consolidation (disk write amplification proportional to churn,
  not to a single update).

## Alternatives considered

- **Keep rebuild-on-write (status quo)** — rejected for streaming workloads
  (`O(N)` per update, the gap ADR-0023 named); retained as the open-time,
  structural-change, and consolidation fallback, and still correct for
  bulk-load-then-query.
- **LIRE on the graph (reuse ADR-0023's machinery)** — rejected: LIRE rebalances
  inverted posting lists (IVF/SPANN), not proximity graphs; FreshDiskANN is the
  algorithm built for graphs, as ADR-0023 already noted.
- **Full in-place on-disk StreamingMerge now** — rejected for this increment: it
  mutates the disk graph, putting it on the `fsync`/crash path (the riskiest
  piece), and the derived rebuild already delivers the size-independent
  *update* cost. Sequenced behind its own ADR and crash-safety proof, exactly as
  ADR-0023→ADR-0025 sequenced the durable IVF index.
- **A brute-force exact delta instead of a delta graph** — viable (and below the
  full-scan threshold it would even beat ANN on recall), but a real incremental
  Vamana delta is the faithful FreshDiskANN temporary index, generalizes to a
  larger delta before consolidation, and reuses the graph primitives Quiver
  already has. The delta is kept small by the consolidation threshold, so its
  search cost is modest either way.
- **Eager edge repair on delete** — rejected for this increment (intricate and
  error-prone; the consolidation threshold bounds degradation), matching ADR-0026.

## Implementation

Filled in on acceptance.

## References

1. Singh, Subramanya, Krishnaswamy, Simhadri. *FreshDiskANN: A Fast and Accurate
   Graph-Based ANN Index for Streaming Similarity Search.* 2021. (the two-tier
   LTI + temporary index + deletion set, and StreamingMerge)
2. Subramanya, Devvrit, Kadekodi, Krishaswamy, Simhadri. *DiskANN: Fast Accurate
   Billion-point Nearest Neighbor Search on a Single Node.* NeurIPS, 2019.
   (Vamana / RobustPrune, the graph being maintained)
3. ADR-0005 (durability & recovery), ADR-0007 (index roadmap), ADR-0019
   (disk-resident index format), ADR-0020 (row-addressed segments), ADR-0021
   (tombstones & compaction), ADR-0023 (incremental in-place IVF updates, which
   deferred the graph case here), ADR-0025 (durable incremental IVF index),
   ADR-0026 (HNSW incremental delete).
