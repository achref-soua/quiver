# ADR-0057: Concurrent reads behind a reader–writer lock (and the staged arc-swap path)

- **Status:** Accepted
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

The engine is single-writer behind one lock at the server (ADR-0006): every
operation — reads included — was serialized behind a single `Mutex<Database>` and
offloaded with `spawn_blocking`. Vector search is CPU-bound ANN traversal, so
under read-heavy load that mutex serialized queries that could have run in
parallel. ADR-0053 designed the eventual lock-free MVCC read path (arc-swap
versioned snapshots) and explicitly named a **reader–writer lock** as the viable
intermediate step toward it.

Two things made reads need `&mut self`, blocking a plain `RwLock` swap:

1. **A read could rebuild the index.** `search` / `hybrid_search` /
   `search_multi_vector` lazily rebuilt a collection's index when a prior write
   left it `stale` — a deliberate batching optimization (a bulk write defers one
   rebuild to the next read instead of rebuilding per row).
2. **The full lock-free path is a larger change.** A truly lock-free read that
   returns payloads cannot share the live `Store` across readers while the writer
   mutates it by `&mut self` — that is aliasing UB. Doing it correctly means the
   immutable snapshot must own its own segment readers, separate from the
   writer's store: a real restructuring of `quiver-core`'s read-ownership model
   plus epoch reclamation. That is the ADR-0053 XL, not a one-release change.

This ADR takes the honest intermediate step now and records the phased plan to
the lock-free target.

## Decision

**Serve reads concurrently behind a `RwLock<Database>`; keep the single writer.**

- **Split the read API.** Each search gains a `&self` `*_snapshot` method
  (`search_snapshot`, `hybrid_search_snapshot`, `search_multi_vector_snapshot`)
  that reads the collection's current built index and returns the internal
  `Error::IndexStale` if a prior write deferred the rebuild. `fetch` and the
  accessors were already `&self`. The existing `&mut self`
  `search`/`hybrid_search`/`search_multi_vector` stay as thin convenience
  wrappers (`search_with_retry`) for embedded, single-threaded callers: they try
  the snapshot read and, on `IndexStale`, rebuild via the new
  `Database::ensure_indexed` and retry. So every existing caller — the in-process
  MCP server, the tests — is unchanged.
- **Lock split at the server.** `Arc<Mutex<Database>>` → `Arc<RwLock<Database>>`.
  Writes take the exclusive lock (`write_blocking`); ordinary reads take the
  shared lock (`read_blocking`) and **run concurrently**. A search uses
  `search_blocking`: the hot path takes only the read lock; if the snapshot read
  reports `IndexStale`, it takes the write lock once, calls `ensure_indexed`, and
  searches while still holding it (no window for another writer to re-stale it
  between rebuild and read). After that one rebuild, reads are concurrent again.
- **Writer and durability unchanged.** Still single-writer; the crash gate, WAL,
  and `fsync` acknowledgement (ADR-0005) are byte-for-byte unchanged — this moves
  *read visibility*, not durability. `IndexStale` is an internal control signal
  caught by the `&mut self` wrappers and the server; it never reaches a client.

## Consequences

- **+** Read throughput scales with cores under read-heavy load instead of
  serializing on one mutex; the standing "single mutex" caveat is gone.
- **+** No change to the write path, durability, or crash recovery; no on-disk
  format change; no migration.
- **+** Snapshot/`ensure_indexed` is the seam the lock-free successor slots into:
  the readers already call a `&self` snapshot method.
- **−** A `RwLock` lets reads run concurrently *with each other* but a write still
  excludes readers for its duration, and the rare stale read briefly takes the
  write lock to rebuild. The fully lock-free path (reads proceed *during* writes)
  is the next phase.
- **−** Writer starvation is theoretically possible under a saturating read load
  with `std::sync::RwLock` (platform-dependent fairness). Acceptable for a
  write-then-read-heavy vector workload; `parking_lot::RwLock` (writer-preferring)
  is the drop-in upgrade if a real workload shows starvation.

## Phased plan to lock-free reads (the ADR-0053 target)

1. **(this ADR) RwLock + `&self` snapshot reads.** Concurrent reads; writer and
   crash gate unchanged. *Done.*
2. **Arc-swapped per-collection read snapshot.** The writer publishes an
   immutable `Arc<ReadSnapshot>` (built index + id maps + the immutable sealed
   segment readers it needs to materialize records) and atomically swaps it on
   the rebuild/maintenance points. Readers `load()` the `Arc` with no lock and
   traverse it; the old version drops when its last reader finishes (Arc refcount
   *is* the coarse epoch guard). This requires `quiver-core` to expose immutable,
   shareable segment readers so a snapshot can read records without aliasing the
   writer's `&mut Store`.
3. **Active-buffer handling + epoch reclamation hardening.** Copy-on-write the
   small pre-checkpoint active buffer into each snapshot (or layer the read over
   immutable-snapshot ⊕ a lock-free read of the active buffer); validate the
   swap/reclamation with `loom` and property tests before flipping the default.

Granularity stays coarse (one snapshot per collection) — full per-row MVCC with a
transaction manager remains rejected as over-built for vector search (ADR-0053).

## Alternatives considered

- **Jump straight to lock-free arc-swap (ADR-0053).** The target, but it
  restructures `quiver-core`'s read ownership and needs loom/property validation
  — a multi-release effort. Shipping the RwLock step first delivers concurrent
  reads now with no correctness risk to the storage engine, behind the same
  `*_snapshot` seam the lock-free path will reuse.
- **Keep the single mutex.** Correct and simple, but serializes reads that could
  run in parallel — the ceiling this ADR lifts.
- **Sharded/striped mutex per collection.** Reduces cross-collection contention
  but not intra-collection; the RwLock already lets a hot collection's reads run
  concurrently.
