# ADR-0023: Incremental in-place index updates (SpFresh / LIRE)

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

Through `v0.3.0` the vector index is a **derived artifact**: a pure function of
the store, rebuilt from `Store::scan` on open. The in-memory HNSW absorbs a
brand-new id incrementally, but every *update-in-place*, every *delete*, and
every write to a batch index (Vamana / IVF / DiskVamana) marks the collection
`stale`, and the next search rebuilds the **whole** index from scratch
(`quiver-embed`'s `rebuild_index`). This is deliberately simple — the store is
the single source of truth (ADR-0020/0021), so the index never has to be made
crash-consistent — and it is the right trade for bulk-load-then-query.

It does not scale to **streaming / continuously-updated** workloads. A single
delete on a 10M-vector collection schedules a 10M rebuild; an interleaved
insert/delete stream rebuilds repeatedly. This is exactly the problem SpFresh
(Xu et al., *SOSP* 2023) was built for, and the gap ADR-0007 and
[`../index/design.md`](../index/design.md) flagged as "incremental updates
(later)". The headline of the Phase-4 backlog is to close it.

The goal: update cost ~independent of collection size, recall preserved under a
long insert/delete stream, the memory-frugality wedge intact, and — the hard
constraint, as in every storage ADR — the `kill -9` crash gate (R3, ADR-0005)
stays green.

## Decision

Adopt SpFresh's **LIRE** (Lightweight Incremental REbalancing) for the **IVF**
family first, because IVF *is* the SPANN-style inverted-list structure LIRE was
designed for (Chen et al., *NeurIPS* 2021). Graph indexes are a different problem
(see Scope). The first increment, shipping in `v0.4.0`, is deliberately scoped to
keep the index off the durability path:

1. **The index stays a derived artifact; LIRE runs on the in-memory IVF.** The
   store (segments + WAL, ADR-0020/0021) remains the sole source of truth and the
   only structure the crash gate protects; on open the index is still
   reconstructed from the store. **The crash gate is therefore unaffected by
   construction** — no index bytes ever reach the `fsync` path. Durable
   on-disk incremental posting lists (SPFresh's actual disk model) are a *later*
   increment with its own ADR and its own crash-safety proof (see Scope).

2. **Incremental IVF operations** replace the `stale → full rebuild` path for IVF
   collections:
   - `insert(id, vector)` — assign to the nearest coarse centroid and append to
     that posting list (encoding the PQ code, or storing the vector in Flat
     mode). Cost `O(nlist + |list|)`, independent of `N`.
   - `remove(id)` — record the id in a per-index **deletion set**: skipped at
     search time, its slot reclaimed lazily by rebalancing/compaction. `O(1)`.
   - In `quiver-embed`, an IVF upsert of a new id appends, an update is a
     remove-then-insert, and a delete marks the deletion set — instead of
     setting `stale`. A full rebuild is then needed only on a **structural**
     change (dimensionality / metric / index kind) or at first open.

3. **LIRE rebalancing keeps posting lists balanced as the distribution drifts**,
   so recall does not decay the way a frozen partitioning would under a long
   update stream. It is *local*, never global — the SpFresh contribution:
   - **Split** a posting list that exceeds `max_postings`: local 2-means over its
     members yields two centroids that replace the old one; members are
     repartitioned between them.
   - **Merge** a posting list that falls below `min_postings`: fold its members
     into the nearest neighboring centroid's list and drop the centroid.
   - **Reassign** the boundary: after a split or merge, re-evaluate the members of
     the affected and *adjacent* lists against the changed centroids and move any
     whose nearest centroid changed. This maintains SPANN's invariant — *each
     vector lives in the posting list of its nearest centroid* — which is what
     protects recall, and it is bounded to the local neighborhood.
   - Rebalancing is triggered by the size thresholds and done as **bounded work
     amortized over the writes that caused it**. The execution model is unchanged:
     a single writer already serializes mutations behind a lock, and a search may
     already trigger work today — it just becomes cheaper.

4. **Parameters are per-collection and defaulted** — `max_postings` and
   `min_postings` (as multiples of the target `N/nlist`) alongside the existing
   `nlist` / `nprobe` / `quantization` knobs (ADR-0007/0008). Builds and
   rebalancing stay reproducible via the existing seeded k-means.

## Scope — what `v0.4.0` ships, and what is deferred

**Shipped in `v0.4.0` (this increment):** incremental, LIRE-rebalanced **IVF**
maintained in memory; `quiver-embed` dispatches IVF upserts/deletes incrementally
instead of marking the collection stale; recall preserved under an insert/delete
stream (tested against a batch-built reference); balance invariants tested; the
crash gate re-run and green (it never touched the index, and still does not). The
index remains derived and rebuilt-on-open.

**Deferred, each behind its own ADR when taken:**

- **Durable on-disk incremental posting lists** — SPFresh's disk model: persist
  posting lists as segments and recover the index from the WAL so a restart need
  not rebuild. This is when the index *joins the durability path* and the crash
  gate must be extended to cover index mutations (atomic posting-list writes +
  WAL backstop, mirroring ADR-0021). Higher risk; explicitly out of `v0.4.0`.
- **Graph-index incremental updates (Vamana / DiskVamana)** — a *different*
  algorithm (FreshDiskANN's StreamingMerge / in-place edge repair, Singh et al.
  2021), not LIRE. Until then, graph collections keep the rebuild-on-write path.
- **HNSW incremental delete** — today HNSW rebuilds on delete; a soft-delete set
  + lazy edge repair is a small follow-on, tracked but not required for the IVF
  headline.

## Consequences

- **+** IVF update cost drops from an `O(N)` rebuild to `O(nlist + |list|)` per
  operation; streaming and continuously-updated IVF collections become practical
  — the SpFresh win, delivered on Quiver's most SpFresh-shaped index.
- **+** **Zero crash-gate risk this increment.** The index stays derived, so the
  durability path — the only thing the gate protects — is untouched. The genuinely
  hard part (durable on-disk index recovery) is sequenced into its own ADR with
  its own crash-safety argument rather than bundled into the first step.
- **+** LIRE's local rebalancing preserves recall under drift without periodic
  global rebuilds and their recall sawtooth.
- **−** A restart still rebuilds the in-memory IVF from the store (the same cost
  as today) until the durable-index increment lands.
- **−** More state and code in the IVF index (a deletion set, dynamic
  centroids/postings, rebalancing); mitigated by recall-under-stream and balance
  tests and by reusing the existing seeded k-means.
- **−** Rebalancing performs bounded work on the write path; an adversarial
  insert pattern could still cluster — bounded by the thresholds, with an
  explicit `rebalance()` escape hatch and the open-time rebuild as a backstop.

## Alternatives considered

- **Keep rebuild-on-write (status quo)** — rejected for streaming workloads
  (`O(N)` per update); retained as the open-time and structural-change fallback,
  and it remains correct for bulk-load-then-query.
- **Ship the durable on-disk incremental index in `v0.4.0`** — rejected *for this
  increment*: it puts the index on the `fsync`/crash path and is the riskiest
  piece. Sequenced next, behind its own ADR and crash-safety proof, rather than
  taken in one leap.
- **Do the graph (Vamana) incrementally first** — rejected: it needs a different
  algorithm (FreshDiskANN), carries higher risk, and the graph already owns the
  disk-frugality story. LIRE on IVF is the cleaner, lower-risk first win and the
  literal SpFresh structure.
- **Periodic global rebuild on a timer/size threshold (LSM-style index
  compaction)** — rejected as the primary mechanism: still `O(N)` per rebuild and
  recall sawtooths; the existing rebuild is kept only as the open-time/structural
  fallback.

## References

1. Xu, Liang, Li, Xu, Chen, Zhang, Li, Yang, Yang, Yang, Cheng, Yang. *SpFresh:
   Incremental In-Place Update for Billion-Scale Vector Search.* SOSP, 2023. (LIRE)
2. Chen et al. *SPANN: Highly-efficient billion-scale ANN search.* NeurIPS, 2021.
   (the in-memory-centroids + posting-lists structure LIRE rebalances)
3. Singh, Subramanya, Krishnaswamy, Simhadri. *FreshDiskANN: A Fast and Accurate
   Graph-Based ANN Index for Streaming Similarity Search.* 2021. (the deferred
   graph path)
4. ADR-0005 (durability & recovery), ADR-0007 (index roadmap), ADR-0008
   (quantization), ADR-0020 (row-addressed segments), ADR-0021 (tombstones &
   compaction).
