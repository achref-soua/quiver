# ADR-0064: Lock-free MVCC reads — implementation design

- **Status:** Accepted (implementation design; supersedes the design-only
  [ADR-0053](0053-lock-free-mvcc-reads.md)). Built in staged increments, each its
  own flag-gated PR; increment 1 lands in `v0.24.0`.
- **Date:** 2026-06-24
- **Deciders:** Achref Soua
- **Relates to:** [ADR-0006](0006-concurrency-model.md) (single writer / single
  mutex), [ADR-0057](0057-concurrent-reads-rwlock.md) (the `RwLock` read path this
  supersedes), [ADR-0062](0062-rebuild-off-the-exclusive-lock.md) (off-lock rebuild
  — the existing immutable-snapshot swap this builds on), [ADR-0053](0053-lock-free-mvcc-reads.md)
  (the high-level design).

## Context

Today the server holds `Arc<RwLock<Database>>` (ADR-0057): reads take `.read()`
and run concurrently with each other, but **any write takes `.write()` and blocks
every read** for its duration. A write holds the lock for an in-memory index
mutation plus a WAL append+fsync (~ms; bulk batches one fsync — ADR-0038). ADR-0053
proposed moving reads off the writer lock with an arc-swap MVCC snapshot, but left
**the** hard problem unresolved for Quiver's actual structures, which this ADR must
confront before any code:

**The indexes are mutated in place.** `index_upsert_point` calls `h.insert` /
`ivf.insert` / `fresh.insert` directly on the live `CollectionIndex`; HNSW
soft-delete, IVF LIRE, and FreshDiskANN all mutate in place under the write lock.
A lock-free reader cannot share that index by `Arc` and read it while the single
writer mutates it — that is a data race (UB), the exact thing `RwLock` prevents
today. So "publish an immutable snapshot the reader loads" cannot simply alias the
live index. The three escapes each have a cost:

1. **Clone the index into a new immutable snapshot per write** — O(index) per write;
   under read-heavy load `Arc::make_mut` always clones. Rejected: prohibitive.
2. **Lock-free / epoch-safe index structures** (concurrent-read HNSW/IVF/Vamana) —
   the "correct" XL endgame, but a per-structure rewrite with notoriously hard
   reclamation bugs. Deferred, not the first step.
3. **Publish an immutable base at coarse points; carry writes-since-publish in a
   small, cheaply-republished overlay** — the FreshDiskANN base+delta idea applied
   at the *serving* layer. Bounded cost, no index rewrite. **Chosen.**

## Decision

**Per-collection arc-swap of an immutable serving snapshot, published by the single
writer, with a small copy-on-write overlay for writes-since-publish.** Reads load
two atomics (no lock) and merge; the writer keeps its single-writer discipline and
*publishes* versions instead of locking readers out. MVCC changes **visibility,
not durability** — the WAL-fsync acknowledgement (ADR-0005) and the `kill -9` crash
gate are untouched.

### The snapshot

```
CollectionSnapshot {                     // immutable, behind ArcSwap<Arc<…>>
    base_index: Arc<CollectionIndex>,    // as of the last rebuild/consolidation commit
    int_to_ext: Arc<Vec<String>>,        // id map for the base
    descriptor: Arc<Descriptor>,
    segments:   Arc<SegmentSet>,         // immutable sealed segments (already shareable)
    overlay:    Arc<Overlay>,            // writes since the base was published
}
Overlay {                                // small — bounded by the publish cadence
    upserts: Vec<(u64 /*internal id*/, Arc<[f32]> /*vector*/, ext_id)>,
    tombstones: HashSet<u64>,
    active_records: Arc<HashMap<String, Record>>,   // active-buffer rows, for fetch/filter
}
```

- **Base index** is the immutable artifact the off-lock rebuild (ADR-0062) already
  produces and installs at `commit_rebuild`. We publish it into the snapshot there —
  the swap point already exists; we make it an `arc-swap` `store` instead of a
  write-locked field assignment.
- **Overlay** carries each incremental upsert/delete since the base was published.
  It is **small** (bounded by the rebuild/consolidation cadence — the same 20%
  churn threshold that already triggers consolidation), so the writer republishes a
  fresh `Arc<Overlay>` per write at O(overlay) cost, not O(index). At a rebuild
  commit the new base absorbs the overlay and it resets to empty.
- **Segments / active records.** Sealed segments are already immutable and shared
  by `Arc`. The active buffer (pre-checkpoint rows) is captured into the snapshot as
  an immutable `Arc<HashMap>` so payload/vector fetch and the filter pre-scan read it
  lock-free; it too is bounded by checkpoint cadence and copy-on-write per write.

### The read path (lock-free)

`search_snapshot` loads `ArcSwap::load()` once (an atomic, no lock), then:

1. search `base_index` for candidates (the bulk of recall),
2. search/scan the small `overlay` for recent points,
3. merge by the metric ordering and drop `overlay.tombstones` (the existing
   `quiver_index::fresh::merge` math — reused, not reinvented),
4. fetch payload/vector and apply filters against `segments` ⊕ `overlay.active_records`.

Freshness therefore **matches today** (recent writes are visible via the overlay),
while reads never touch a lock. A read is snapshot-isolated: it sees one consistent
`(base, overlay)` pair; a write that lands mid-read is simply the next snapshot.

### The writer

Unchanged single writer. After each committed mutation it builds the next
`Arc<Overlay>` (cheap) and `ArcSwap::store`s a new `CollectionSnapshot` reusing the
unchanged `Arc`s (base, segments). On `commit_rebuild` it publishes a new base and
an empty overlay. **Reclamation** is the `Arc` refcount: a superseded snapshot drops
when its last in-flight reader finishes — no epoch GC, no hazard pointers (the
coarse, whole-snapshot case ADR-0053 anticipated).

### Crash gate / durability

No on-disk change. Acknowledgement stays WAL-fsync before return; recovery is
unchanged. The overlay and active-record map are **in-memory visibility state**
derived from the same WAL the store already replays, so a crash loses neither (the
store recovers them; the snapshot is rebuilt on open). The `kill -9` gate is
untouched by construction.

## Increments (each a flag-gated PR, default off until proven)

Behind a `QUIVER_MVCC_READS` runtime flag (off by default) so the proven `RwLock`
path stays the default until the MVCC path is loom- and benchmark-validated:

1. **Snapshot infra + pure-vector reads.** Introduce `arc-swap`, the
   `CollectionSnapshot`/`Overlay` types, publish-on-commit, and route the
   no-filter/no-payload `search_snapshot` through the lock-free load+merge. Loom
   model of the publish/load; reader-during-write consistency test; no torn reads.
2. **Filtered / payload / hybrid reads.** Lock-free `segments ⊕ active_records`
   fetch + filter pre-scan; hybrid (sparse/BM25) over the snapshot.
3. **Validation + cutover.** `loom` exhaustive model of the swap + overlay reset;
   a saturated read-**during-write** benchmark (ADR-0061 driver extended with a
   concurrent writer) proving the QPS win on real hardware; then flip the default
   and retire the `RwLock` read path (ADR-0057) once green.

## Consequences

- **+** Reads never block on the writer; read throughput scales with cores under
  mixed read/write load — the standing ADR-0006 "Phase-2" caveat closed.
- **+** Builds on the existing off-lock-rebuild swap and the FreshDiskANN merge —
  no new index data structures, no per-structure lock-free rewrite.
- **+** Snapshot isolation is a cleaner correctness story than "the mutex serializes
  everything"; durability and the crash gate are untouched.
- **−** Two live snapshot versions during a swap (bounded; old drops when readers
  drain) plus the overlay/active-record copies — bounded extra memory, the
  copy-on-write per write is O(overlay), not O(index).
- **−** The overlay duplicates, at the serving layer, the recent-write set the
  index also tracks internally — a deliberate, documented redundancy that buys
  lock-free reads without rewriting the indexes. Reconsidered if/when increment-3
  motivates lock-free index structures.
- **−** Reclamation is `Arc` refcount only (coarse). Fine for whole-snapshot
  granularity; finer granularity would need epochs — explicitly out of scope.

## Alternatives considered

- **Publish only at rebuild (no overlay)** — simplest, but reads would miss every
  incremental upsert until the next consolidation (up to the 20% churn window),
  a freshness *regression* vs today's read-lock reads. Rejected: the overlay keeps
  freshness at parity for a small, bounded cost.
- **Clone the index per write / `Arc::make_mut`** — O(index) per write under read
  load. Rejected (cost).
- **Lock-free index structures** — the XL endgame; deferred behind a measured need
  (ADR-0053's own guidance), and increment-3's benchmark is what would justify it.
- **Keep the `RwLock` (do nothing)** — correct and simple; the write-lock window is
  only ~ms. This ADR is built behind a default-off flag precisely so the `RwLock`
  remains the default until MVCC is measured to beat it on real hardware.
