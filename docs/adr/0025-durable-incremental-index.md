# ADR-0025: Durable on-disk incremental index (IVF)

- **Status:** Proposed — design-first; the implementation lands as a later
  `v0.5.0` increment and flips this to Accepted once the crash gate covers it.
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

Incremental IVF updates (ADR-0023) keep the index current under a long
insert/delete stream without an `O(N)` rebuild — but only *in memory*. The index
is **derived**: `Database::open` marks every collection `stale` and
`rebuild_index` reconstructs it from the collection's stored vectors (retraining
the coarse quantizer and re-assigning every point). That was a deliberate
simplification: because the index is never written to disk, a `kill -9` can never
leave a torn index, so the crash gate (R3, ADR-0005) was *untouched by
construction* (ADR-0023).

The cost lands on **open**. For a large collection the derive is an `O(N)`
k-means train plus `O(N · nlist)` assignment on every restart — which negates the
very benefit ADR-0023 bought (cheap incremental maintenance) the moment the
process restarts. A database that absorbed millions of updates should reopen in
seconds, not re-cluster from scratch.

This ADR makes the IVF index **durable**: persisted at checkpoint and recovered
on open, so a restart *loads* the index instead of *rebuilding* it. The hard
constraint is unchanged from ADR-0005 — a process kill at any point recovers with
no lost acknowledged writes and no corrupted state — but it now includes the
index, which joins the durability path. The crash gate must extend to cover it.

## Decision

**Snapshot the index at checkpoint; recover it from the WAL.** The index reuses
the engine's existing durability machinery (ADR-0005 WAL + checkpoint/manifest,
ADR-0021's crash-safe write protocol) rather than introducing a parallel one.

**1. Manifest-referenced index snapshot.** A checkpoint additionally seals each
incrementally-maintained collection's index to an immutable, encrypted on-disk
**snapshot** (`idx-NNN` per collection, sealed with the collection's page codec
exactly like a segment — ADR-0010/0019). The snapshot captures the full IVF state
needed to resume — centroids, posting lists, the resident vectors / PQ codes, the
id↔node maps, the free lists, and the split counter — and is referenced from the
collection's manifest entry together with the checkpoint LSN it reflects. The
manifest swap that publishes a checkpoint therefore publishes a **consistent
`(segments, index)` pair** at one LSN, flipped atomically (ADR-0004).

**2. The data WAL is the index's write-ahead log — there is no second log.**
Between checkpoints the index is *not* separately journaled. Every mutation is
already captured by the `Upsert` / `Delete` WAL records that ADR-0005 `fsync`s
before acknowledging. On open those records are replayed through the normal
`insert` / `remove` path, which re-applies the post-snapshot mutations to the
loaded index (re-triggering split/merge). Split and merge are pure functions of
the operation stream, so replay yields a correct index; exact byte-reproduction
of the pre-crash layout is neither required nor relied upon — only that every
live point is present and assigned to a near-nearest cell.

**3. Recovery (extends ADR-0005).** On open, for each collection: load the
manifest → if it references an index snapshot at `last_checkpointed_lsn`, load and
decrypt it; otherwise fall back to the current full rebuild (cold start, a
pre-0025 store, or a torn/absent snapshot). Then replay WAL records with
`LSN > last_checkpointed_lsn` into both the active segment *and* the index. The
result is the index as of the last acknowledged write.

**4. Crash-safety rests on three facts** — the same shape as ADR-0021, not on any
new cross-file atomicity:

1. **Atomic publish.** The snapshot is written to a temp file, `fsync`'d, and made
   live only by the atomic manifest swap. A crash mid-write leaves the *previous*
   manifest — and therefore the previous snapshot, or none — never a torn one.
   Orphan snapshot files (written, never referenced) are GC'd on open like orphan
   segments.
2. **Immutability.** A published snapshot is read-only; the next checkpoint writes
   a new file and swaps. There is no in-place on-disk index mutation to tear.
3. **WAL backstop.** Mutations after the snapshot's LSN are reconstructed by WAL
   replay (already `fsync`'d before acknowledgement), exactly as the active
   segment is. A snapshot a crash left *behind* the WAL is caught up by replay; a
   checkpoint a crash *lost* falls back to the last good snapshot plus a longer
   replay.

**5. Scope: IVF only.** This covers the incrementally-maintained IVF index of
ADR-0023. HNSW (in-memory, rebuilt) and Vamana / the disk graph (batch-built,
ADR-0019) keep deriving on open; durable incremental *graph* updates
(FreshDiskANN) are a separate, later increment with their own ADR. A collection
without a snapshot always recovers via the existing rebuild, so the change is
backward-compatible and effectively opt-in per index kind.

## Consequences

- **+** A restart **loads** the index (sequential read, map-and-decrypt, `O(N)`
  I/O) instead of **rebuilding** it (`O(N)` k-means + assignment) — the ADR-0023
  incremental benefit now persists across restarts, which is the whole point.
- **+** No second write-ahead log, no new record types, no cross-WAL ordering: the
  snapshot is "just another checkpointed artifact," and the existing recovery,
  GC, and encryption paths carry it.
- **+** The `(segments, index)` pair is consistent at every published LSN, so a
  recovered index can never disagree with the data it indexes.
- **−** The index becomes **first-class durable state**: the crash gate's surface
  grows, and a checkpoint now also writes the snapshot (more checkpoint I/O and
  disk footprint — bounded by reusing the immutable-then-swap discipline and
  reclaiming superseded snapshots like superseded segments).
- **−** Only IVF is durable; graph indexes still rebuild on open until their own
  increment.
- **−** A very long WAL tail after a missed checkpoint lengthens replay; mitigated
  by checkpoint cadence (unchanged from ADR-0005) and the rebuild fallback.

## Verification

The crash gate (R3, ADR-0005; `quiver-core/tests/crash_recovery.rs`) **extends to
the index**: build an IVF collection, drive an incremental insert/delete stream
that triggers splits and merges, checkpoint (snapshotting the index), then
`SIGKILL` the writer at randomized points — mid-snapshot-write, mid-WAL-append,
and between snapshot `fsync` and manifest swap. On reopen assert: every
acknowledged upsert is findable by search; no torn snapshot is ever loaded
(AEAD/CRC + atomic rename reject it); and the recovered index's membership equals
a freshly-rebuilt index over the same data. As today, the gate must stay green in
isolation (`cargo test -p quiver-core --test crash_recovery`), since it can flake
under parallel load.

## Alternatives considered

- **A separate index WAL** journaling split/merge/insert operations — rejected: a
  second log demands its own framing, LSNs, `fsync` policy, and ordering against
  the data WAL. The data WAL already records the logical mutations; replaying it
  re-derives the index for free (mirrors ADR-0021 rejecting generation-numbered
  `.del` in favour of reusing existing mechanisms).
- **Per-mutation incremental persistence** (append posting-list deltas to disk on
  every insert) — rejected: write amplification on the hot path for a durability
  guarantee the WAL already provides; snapshot-at-checkpoint plus replay is
  simpler and equally crash-safe.
- **Keep deriving the index (status quo)** — rejected for large collections: an
  `O(N)` k-means rebuild on every open defeats incremental maintenance across
  restarts; acceptable only while collections are small, which is not the target.
- **A manifest-independent sibling file** (like the `.del` bitmap, loaded if
  present) — rejected for the index: unlike a *monotonic* tombstone set, the index
  must be consistent with the data *at the same LSN*, so it belongs inside the
  atomic manifest swap, not beside it.
- **Superseding ADR-0023's "index stays derived"** — not a supersession:
  ADR-0023's in-memory derivation remains the right model for cold start, small
  collections, and the rebuild fallback; this ADR adds a durable fast-path on top.
