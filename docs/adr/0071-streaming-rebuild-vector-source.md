# ADR-0071: Lock-free vector source for the streaming rebuild (Increment B)

- **Status:** Accepted
- **Date:** 2026-07-04
- **Deciders:** Achref Soua

## Context

[ADR-0070](0070-streaming-index-build.md) landed **Increment A** — `Ivf::build_streaming`,
a two-pass (reservoir-sample train, stream-encode) IVF+PQ build over a re-iterable
vector source, byte-identical to `Ivf::build` on the same rows and seed. That is
the tested primitive, but it changed no live path: the off-lock rebuild (ADR-0062)
still materialized the whole corpus into `RebuildScan.flat` (`n · dim · 4` bytes —
~51 GiB at 100M×128) before building, so the RAM wall was untouched in practice.

**Increment B** is the piece that turns the primitive into a live memory win: feed
`build_streaming` from the store's immutable segments instead of `flat`. ADR-0070
sketched this as a `stream_vectors(collection) -> Iterator<Item = &[f32]>` that
*borrows* the store's already-`mmap`'d `.vec` files. Implementing it against the
real storage/rebuild seam surfaced two constraints that reshaped the design.

**1. The off-lock build runs on another thread.** The server captures rebuild
inputs under the read lock, then builds on a `spawn_blocking` worker
(`RebuildInputs` is `Send + 'static`). A borrowed iterator tied to the store's
lifetime cannot cross that boundary — the source must **own** everything it reads.

**2. The store's segments are not shareable as-is.** A `SealedSegment` owns its
`mmap` (not an `Arc`), and a checkpoint mutates sealed segments in place
(`get_mut` on the tombstone bitmap during compaction). Sharing the store's
segments with an in-flight rebuild — via `Arc<SealedSegment>` — would make that
`Arc::get_mut` fail whenever a rebuild holds a clone, coupling checkpoint liveness
to rebuild progress. That is exactly the lock-model hazard ADR-0070 flagged.

## Decision

Capture an **owned, re-iterable, lock-free** `VectorSource` under the read lock;
iterate it off-lock with no store lock held.

**1. Reopen, don't share.** `Store::capture_vector_source(collection)` reopens the
referenced segments' immutable `.vec` columns as **fresh, independent `mmap`s**
(`segment::open_vec_column` — just the vector block file, not the payload
directory, tombstones, or secondary index a vector scan never touches). The
store's own `sealed` vector is untouched, so a concurrent checkpoint may compact,
mutate, or even unlink those segments freely — the source's independent mappings
keep their pages valid for the life of the build. This sidesteps the `Arc::get_mut`
hazard entirely with zero change to the store's hot read path or core types.

**2. Own the bounded active rows.** Rows still in the in-RAM active buffer are
cloned into the source (bounded by the checkpoint cadence, so small). A row that a
re-upsert has moved from a sealed segment into the active buffer is captured from
the buffer, exactly as `scan` resolves it — the source is the live set in primary
(external-id) order, identical to `scan`.

**3. Yield owned vectors, surface read errors out-of-band.** `iter()` yields
`Vec<f32>` (owned; the vectors are spread across segments, so there is no single
borrowable slice — the same reason ADR-0070 rejected the borrowed-slice
alternative). A read/decrypt error ends the stream early and is stashed in a
`Cell`; `build_streaming` then fails with a row-count mismatch, and
`build_ivf_streaming` swaps in the real `take_error()` cause. A short stream can
never silently omit a row.

**4. Route at the rebuild entry points, not through `build_in_memory_index`.**
`scan_collection` captures a source (and skips `flat`) exactly for a
streaming-eligible collection; both the off-lock `RebuildInputs::build` and the
synchronous `rebuild_index` then call `build_ivf_streaming` when a source is
present, and the unchanged `flat` path otherwise. Keeping the routing at the two
entry points avoids threading a source/`flat` enum through the generic
`build_in_memory_index` signature that serves every index kind.

**5. Scope: all single-vector, server-ranked IVF.** Streaming covers
`IndexKind::Ivf` that is not multi-vector and not client-side-encrypted (no
server index). The (usually empty) derived sparse index is still rebuilt from a
**payload-only** scan (`Store::scan_payloads`) — payloads are small; the vectors
are the wall. IVF-Flat streams too: it saves no *resident* memory (its vectors are
resident by definition) but drops the separate `flat` scan arena and keeps one
build path. Non-IVF kinds keep the materialized path until they grow streaming
primitives of their own.

## Consequences

- **+** The live IVF rebuild — the server's off-lock path and the embedded
  open/`ensure_indexed` path — no longer allocates the `n · dim` corpus. Peak
  build memory for the vectors drops from `O(n · dim)` to
  `O(sample + nlist · dim + n · m_bytes)`; the corpus stays on disk, read through
  `mmap` on demand. This is a provable, code-level elimination of the arena, not a
  heuristic.
- **+** No new durability or lock surface: the source reuses the write-once
  segment guarantee, holds no lock while building, and cannot be disturbed by a
  concurrent checkpoint. The store's core types and read path are unchanged.
- **−** Reopening duplicates the `.vec` `mmap`s for the build's duration (address
  space, not resident RAM — pages are shared/evictable via the page cache) and
  re-`mmap`s each referenced segment once under the lock (cheap: no vector bytes
  are read during capture).
- **−** The **primary index** (`int_to_ext`/`ext_to_int`, ~316 B/point) is still
  fully resident — the other O(n) cost ADR-0070 named. Increment C (on-disk /
  sharded primary index) remains the next step for a 15 GiB-box 100M build; on a
  larger box the streaming build alone now unlocks it.
- **−** The reference-hardware RSS measurement (ADR-0070's `≥ 20M` scale-harness
  validation) is **not run here** and **no number is claimed** — it needs the
  dedicated box the scale characterization reserves. The change is landed on
  correctness (a rebuild test spanning both a reopened sealed `.vec` and the active
  buffer, plus the existing off-lock IVF suite), with the memory win argued from
  the code, not a fabricated benchmark.

## Alternatives considered

- **Share the store's segments via `Arc<SealedSegment>`.** Rejected: the
  checkpoint's in-place `get_mut` on a shared segment would fail while a rebuild
  holds a clone, coupling checkpoint liveness to rebuild progress and touching
  every `sealed` access site. Reopening independent `mmap`s is lower blast radius
  and lock-model-safe.
- **Borrowed `Iterator<Item = &[f32]>` (the ADR-0070 sketch).** Rejected: it cannot
  cross the `spawn_blocking` boundary the off-lock build uses, and the vectors span
  multiple segments so there is no single slice to borrow.
- **Read vectors under the lock but discard them (drop `flat`, keep `scan`).**
  Rejected: it removes the RAM wall but still decrypts every vector under the read
  lock only to throw it away, then reads them again off-lock — double work and a
  longer lock hold. The payload-only scan plus the off-lock source reads each
  vector once, off-lock.
