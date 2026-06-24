# ADR-0062: Move the index rebuild off the exclusive lock (the measured lock-free win, without `unsafe`)

- **Status:** Accepted
- **Date:** 2026-06-24
- **Deciders:** Achref Soua

## Context

ADR-0057 shipped concurrent reads behind a `RwLock<Database>` and proved (the
v0.22.0 concurrency benchmark, `docs/benchmarks/results/comparison-v0.22.0/`)
that *fresh* reads scale with cores — 1.76× at `ef=256`. It then named a phased
path to a fully lock-free arc-swap MVCC read (ADR-0053): the writer publishes an
immutable `Arc<ReadSnapshot>` and readers `load()` it with no lock. Both ADRs
assumed that path needs `unsafe` — a `quiver-core` restructuring so a snapshot
*owns* immutable segment readers (today `Store::get` is `&self` but writes are
`&mut Store`, so a truly lockless read aliases the writer) plus epoch reclamation
validated with `loom`. That is an XL with real correctness risk.

Before paying for `unsafe`, this slice did what the brief demanded: **measure the
actual win first.** A `RwLock` already lets reads run concurrently *with each
other*. The only thing a lock-free read adds is that readers do not block while
the single writer holds the exclusive lock — and the only place the writer holds
it for a meaningful span is a **deferred index rebuild**.

The engine defers a rebuild (sets `handle.stale`, leaving the prior built index
intact) whenever a write cannot be absorbed in place: a `upsert_bulk`, an HNSW
in-place update of an existing id, a delete, a replicated write. The *next* read
then pays the rebuild on the server's `search_blocking` cold path
(`crates/quiver-server/src/lib.rs`) — and it does so under the **exclusive write
lock**, so every other reader blocks for the whole rebuild.

## The measurement

Harness: `crates/quiver-embed/tests/mvcc_measurement.rs` (`#[ignore]`d — not a CI
gate). It models the server faithfully: `Arc<RwLock<Database>>`, readers take the
shared lock and call `search_snapshot`; on `IndexStale` the cold path takes the
exclusive lock once, `ensure_indexed`, and searches under it. A bulk upsert marks
the index stale; the next read pays the rebuild. Reproduce:

```text
cargo test -p quiverdb-embed --release --test mvcc_measurement -- --ignored --nocapture
```

Result (dev box, WSL2; HNSW, dim 128, 4 readers; indicative, not reference
hardware):

| N (vectors) | single-thread rebuild | steady-state read p99 (concurrent) | reader stall during rebuild |
|---|---|---|---|
| 20 000  | 7.3 s  | 422 µs | **8.1 s**  |
| 50 000  | 26.7 s | 379 µs | **29.7 s** |
| 100 000 | 68.7 s | 408 µs | **76.6 s** |

The stall ≈ the rebuild duration, grows with collection size, and is **four to
five orders of magnitude above the steady-state p99** — borne by *every* read
that arrives during the rebuild. On the disk-Vamana path a 1 M rebuild is minutes
(slice-5 evidence: ~17 min single-threaded), so the stall is worse at scale.

So the win is **large and real** — this is *not* the "delta too small, advance
honestly" case the brief allowed for.

## Decision

**Eliminate the stall by moving the rebuild off the exclusive lock and serving
the prior snapshot during it — with `Arc` and the existing `RwLock`, no `unsafe`,
no `loom`.** The measurement changed the design, not just the priority: the entire
stall comes from the rebuild holding the *exclusive* lock, and a `RwLock` already
permits concurrent readers. We do not need a lockless read that aliases the
writer's store; we need the *rebuild* to not exclude readers. Concretely:

1. **Serve the prior snapshot when stale.** When `handle.stale` is set the prior
   built `handle.index` is still a valid ANN structure over the prior internal
   ids (a deferred write only appends new ids / tombstones; it never invalidates
   the old graph). The `*_snapshot` reads serve it instead of returning
   `IndexStale`. A read may miss a write committed after its snapshot — the
   snapshot-isolation contract ADR-0053 already sanctioned for vector search.
2. **Rebuild off the exclusive lock, single-flight.** Split the rebuild into
   `prepare_rebuild(&self) -> RebuiltIndex` (clones the rebuild inputs and builds
   the new index + sparse index — runs under the *shared* read lock, so other
   reads proceed concurrently) and `commit_rebuild(&mut self, RebuiltIndex)`
   (installs it under a brief exclusive lock and clears `stale`). The server
   drives one rebuild per stale collection (deduped by an in-flight set), so N
   concurrent readers never each kick off a full build. If writes arrive during a
   build, `stale` stays set and another rebuild is scheduled — the signal is
   never lost.
3. **Embedded callers keep read-your-writes.** The `&mut self`
   `search`/`hybrid_search`/`search_multi_vector` wrappers stay synchronous
   (rebuild-then-read), so an in-process, single-threaded caller still sees its
   own write immediately. The off-lock, eventually-consistent path is the
   *server's* read path, where concurrency is the point.

The writer, durability, WAL, `fsync` acknowledgement, and the crash-recovery gate
stay **byte-for-byte unchanged** — this moves *read visibility and the rebuild's
lock scope*, never durability.

This **refines ADR-0057's phase 2 and supersedes ADR-0053's premise that the win
needs an `unsafe` `quiver-core` read-ownership restructuring** for the common
rebuild case. A fully lockless read (no lock at all on the hot path, shaving the
~150 µs `RwLock` read-acquire) remains the eventual ceiling and stays the
`unsafe`/`loom` XL of ADR-0053 — but the measurement says it is not where the
seconds are, so it is not this slice's target.

## Consequences

- **+** The seconds-to-minutes reader stall during a rebuild collapses to the
  cost of serving the prior snapshot (~p99, sub-millisecond) plus a brief
  exclusive lock for the pointer swap.
- **+** No `unsafe`, no lock-free data structure, no `loom` — the change is
  reviewable with ordinary concurrency tests and preserves the storage engine's
  correctness model intact.
- **+** `prepare_rebuild`/`commit_rebuild` is the same seam the eventual lockless
  read (ADR-0053) would build on.
- **−** Server reads become snapshot-isolated/eventually-consistent across a
  write window (a read can miss a just-committed write until the rebuild commits).
  Sanctioned by ADR-0053; embedded `&mut` callers are unaffected.
- **−** `prepare_rebuild` holds the shared read lock for the build duration, so a
  *writer* still waits behind an in-flight rebuild. Acceptable for a
  write-then-read-heavy workload; the lockless ADR-0053 path is the upgrade if a
  write-heavy workload ever needs it.
- **−** Transient memory: the build clones its inputs (≈ N·dim·4 bytes — 51 MB at
  100 k×128) and holds two index versions until the old one drains. Bounded;
  named here so it is a known ceiling, not a surprise.

## Build plan (the follow-up PR)

1. Engine: `prepare_rebuild`/`commit_rebuild`; `*_snapshot` serves the prior
   index when stale; keep the `&mut` wrappers synchronous. Unit + property tests:
   prior-snapshot correctness, prepare/commit round-trip, write-during-build
   leaves `stale` set, single-flight dedup.
2. Server: drive the off-lock rebuild with an in-flight dedup set; cold path
   serves the prior snapshot instead of blocking. Re-run
   `mvcc_measurement.rs` to show the stall → ~p99.
3. Docs: `architecture/overview.md` concurrency note; the "Quiver, Explained"
   PDF MVCC timeline figure (slice 7).

## Alternatives considered

- **Ship the full lock-free arc-swap now (ADR-0053).** The measurement says the
  seconds are in the rebuild's *lock scope*, not the read's lock *acquire*, so
  the `unsafe`/`loom` XL buys little over moving the rebuild off the lock. Deferred
  as the eventual ceiling, not this slice.
- **Keep the deferred-rebuild-under-write-lock model.** The measured 8–77 s stall
  is the cost; rejected.
- **Eager rebuild inside the write that marks stale.** Moves the cost onto the
  writer and still serializes against reads for the whole build; the bulk-write
  batching optimization (one rebuild per batch) exists precisely to avoid this.
- **Distributed sharding to scale reads (ADR-0051).** Orthogonal and far heavier
  (per-shard Raft is multi-quarter, ADR-0051); the stall is an *intra-node* lock
  issue a single node fixes directly. Sharding stays design-only.
