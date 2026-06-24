# Concurrency & the off-lock rebuild

Quiver is **single-writer, many-reader**. The server guards the engine with a
reader–writer lock (ADR-0057): a search takes the *shared* lock, so many searches
run in parallel; a write takes the *exclusive* lock. Durability is unchanged —
this is about read *throughput* and read *visibility*, never the WAL-`fsync`
acknowledgement (see [Snapshots & backup](./snapshot.md) and the crash gate).

## Deferred rebuilds, and why they used to hurt

Some writes cannot be absorbed into the index in place — a bulk load, an HNSW
in-place update of an existing id, a delete, a replicated write. The engine
**defers** the rebuild: it marks the collection stale and keeps the prior, still
valid index. The question is what the *next* reader does about it.

Before v0.22.0, that reader rebuilt the index **under the exclusive lock** before
serving — correct, but it blocked every other reader for the whole build. The
reproducible harness (`crates/quiver-embed/tests/mvcc_measurement.rs`) measured
the stall on a dev box (HNSW, dim 128, indicative):

| Collection size | single-thread rebuild | steady read p99 | reader stall during rebuild |
|---|---:|---:|---:|
| 20 000 | 7.3 s | 422 µs | **8.1 s** |
| 50 000 | 26.7 s | 379 µs | **29.7 s** |
| 100 000 | 68.7 s | 408 µs | **76.6 s** |

The stall is four to five orders of magnitude above the steady-state p99, and
grows with collection size.

## The fix — rebuild off the exclusive lock (ADR-0062)

The server now **serves the prior snapshot** while it rebuilds the index
**off-lock**:

1. **Serve the prior snapshot when stale.** A stale read returns results from the
   prior index (still a valid graph over the prior ids) instead of blocking — the
   snapshot-isolation contract sanctioned by ADR-0053.
2. **Rebuild with no lock held.** The rebuild inputs are captured under the
   *shared* read lock (other reads continue), the new index is built holding **no
   lock at all**, and only the final pointer-swap takes a brief exclusive lock.
   One rebuild runs per stale collection (deduped by an in-flight set).
3. **A write-generation guard.** A per-collection counter is bumped on every
   write; if it moved during a build, the collection stays stale and another
   rebuild is scheduled — so **no write is lost**.

The result: the seconds-long stall collapses to the cost of serving the prior
snapshot (sub-millisecond) plus the brief swap. No `unsafe`, no lock-free data
structure, no `loom` — just `Arc` and the existing `RwLock`.

### What this changes (and what it doesn't)

- **Server reads are eventually consistent across a rebuild window:** a read may
  briefly miss a write committed a moment ago, but never sees a half-applied one.
- **Embedded `&mut` callers keep read-your-writes:** the in-process
  `search`/`hybrid_search`/`search_multi_vector` wrappers still rebuild
  synchronously, so a single-threaded program always sees its own write.
- **Durability and the `kill -9` crash gate are byte-for-byte unchanged.**

The fully **lock-free** path — reads proceeding *during* the swap over an
atomically-swapped snapshot (arc-swap + epoch reclamation, ADR-0053) — remains
designed and staged; the measurement showed the seconds were in the rebuild's
*lock scope*, not the read's lock *acquire*, so Quiver fixed the former first.
