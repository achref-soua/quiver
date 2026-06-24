# ADR-0053: Lock-free MVCC reads (design only)

- **Status:** Accepted (high-level design) → implementation design in
  [ADR-0064](0064-mvcc-reads-implementation.md), which resolves the in-place
  index-mutation tension this ADR left open and stages the build. Implemented in
  increments behind a default-off `QUIVER_MVCC_READS` flag from `v0.24.0`.
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

The engine is single-writer, single-mutex (ADR-0006): every operation — reads
included — is serialized behind one `Mutex<Database>` at the server layer and
offloaded with `spawn_blocking`. This is simple and correct, and the read path
is CPU-bound ANN traversal, so under read-heavy load the mutex serializes
queries that could run concurrently. ADR-0006 explicitly named lock-free MVCC
reads as the Phase-2 evolution. The server module doc still reads "The lock-free
MVCC read path is Phase 2."

This ADR records the intended design without committing to build it.

## Decision (intended design)

Move reads off the writer mutex using **multi-version concurrency control** with
an epoch-based reclamation scheme:

- **Versioned snapshots.** The writer publishes an immutable, atomically-swapped
  view of the searchable state — the index handle(s) + the segment set + the id
  maps — behind an `arc-swap`-style pointer. A reader `load()`s the current
  `Arc<Snapshot>` (one atomic load, no lock) and traverses it; the writer builds
  the next snapshot and swaps the pointer. Readers in flight keep their `Arc`
  alive; the old version is dropped when its last reader finishes (Arc refcount
  *is* the epoch guard for the coarse-grained, whole-snapshot case).
- **Granularity.** Start coarse: one snapshot per collection, swapped on
  checkpoint / index rebuild and on the incremental upsert/delete maintenance
  points. The active in-memory buffer (pre-checkpoint mutations) is the only
  part needing care — either copy-on-write the small active map into each
  snapshot, or layer a read over (immutable snapshot ⊕ a lock-free read of the
  active buffer). The immutable sealed segments and the built graph are already
  shareable as-is.
- **Writer.** Still single-writer (no change to the write-correctness model or
  the crash gate) — it just publishes new versions instead of holding readers
  out. Acknowledgement is still WAL-fsync (ADR-0005); MVCC changes *visibility*,
  not durability.
- **Consistency.** Reads are snapshot-isolated: a query sees a consistent
  point-in-time view, never a half-applied batch. A read may miss a write that
  committed after its snapshot load — acceptable and expected for vector search.

## Consequences

- **+** Concurrent, lock-free reads — read throughput scales with cores under
  read-heavy load instead of serializing on the mutex; removes the standing
  Phase-2 caveat.
- **+** Snapshot isolation is a cleaner correctness story than "the mutex
  happens to serialize everything."
- **+** The write path, durability, and crash gate are unchanged — MVCC is a
  read-visibility mechanism layered on the existing single writer.
- **−** Memory: two live versions during a swap (bounded — old drops when its
  readers drain). The active-buffer handling is the subtle part and needs
  careful, well-tested copy-on-write or a lock-free structure.
- **−** Epoch/reclamation bugs are notoriously hard; needs loom/stress tests and
  a careful audit. Real but contained effort (L–XL).

## Alternatives considered

- **RwLock instead of MVCC** — lets reads run concurrently with each other but
  still blocks them during any write/maintenance; under steady incremental
  upserts that is frequent. MVCC lets reads proceed *during* writes. RwLock is a
  smaller, weaker step; viable as an interim but not the target.
- **Sharded mutex (lock striping) per collection** — reduces contention across
  collections but not within a hot collection; orthogonal, and MVCC subsumes the
  intra-collection win.
- **Full per-row MVCC with a global version clock** (database-style) — rejected
  as over-built for vector search: snapshot-per-collection captures the benefit
  without a transaction manager.
- **Do nothing** — the *current* decision; the single mutex is correct and the
  read path is fast per query. Build when a measured read-concurrency ceiling on
  real hardware justifies the reclamation complexity.
