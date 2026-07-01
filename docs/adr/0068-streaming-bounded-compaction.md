# ADR-0068: Streaming, memory-bounded, per-checkpoint-bounded segment compaction

- **Status:** Accepted
- **Date:** 2026-07-01
- **Deciders:** Achref Soua

## Context

Compaction merges a collection's sealed segments into one, dropping dead (deleted
or shadowed) rows — it bounds read/recovery fan-out and reclaims space. Two
properties of the pre-0068 implementation did not scale:

1. **It materialised the whole live set in RAM.** `compact_collection` built a
   `Vec<(String, Vec<u8>, Vec<u8>)>` of every live row, then `write_segment`
   built the full `.vec` and `.pay` blobs on top of that — roughly *2× the
   collection's on-disk size* resident, transiently, per compaction. For a large
   or disk-resident collection (whose vectors otherwise live on SSD, not in RAM)
   that is a real memory blow-up and an OOM risk.

2. **It ran synchronously inside `checkpoint`, on the single-writer path.**
   `checkpoint` called `auto_compact()` at its tail, and `auto_compact` compacted
   *every* over-threshold collection in one pass. A checkpoint — the frequent,
   latency-sensitive durability operation — could therefore stall for the whole
   duration of a fan-out compaction, blocking every writer (and, at the server,
   every reader that needs the write lock).

The engine is **single-writer**: `Store` mutations take `&mut self`; there is no
concurrency *inside* `quiver-core`. Concurrency lives at the server, behind the
`RwLock<Database>` (ADR-0057), and the one place we have already moved heavy work
off that lock is the deferred **index rebuild** (ADR-0062: capture inputs under
the shared lock → build with no lock → commit under a brief exclusive lock, with
a write-generation guard against races).

## Decision

Two changes, both preserving the atomic-manifest-swap commit and the crash
contract (ADR-0005), and neither altering any on-disk format.

### 1. Stream the merge (bounds memory)

Add a streaming block-file writer, `blockfile::BlockWriter`, that seals and
appends each 16 KiB page as its body fills — holding one page in memory instead
of the whole column. `write_blocks` becomes a thin wrapper over it, so there is a
single pagination path and the on-disk bytes are provably unchanged regardless of
how the body is chunked (a byte-for-byte test pins this).

`segment::write_segment_streaming` drives two `BlockWriter`s (`.vec`, `.pay`) from
a **pull generator** (`FnMut() -> Result<Option<(id, vector, payload)>>`).
`compact_collection` now:

- plans the merge from directory metadata only — the ordered `(segment, row)` of
  every live sealed row (O(rows) of 8-byte locations, never the bytes), in the
  deterministic `primary`-key order the merged segment already used; then
- streams each row's vector + payload straight from the source segments' `mmap`s
  into the writer, one row resident at a time.

The old segments stay valid until the manifest swap, so an interrupted compaction
leaves the pre-compaction state intact and the half-written segment orphaned for
GC — identical to an interrupted checkpoint, and now covered by a dedicated test.

**Honest scope of the memory bound.** The dominant term — the vector and payload
*bytes* — is now bounded to one page per column. Two residuals remain O(rows) /
O(payloads) and are *not* streamed: the row directory (`.dir`: ids + offsets,
assembled then `postcard`-encoded as one blob) and, only for a collection with
filterable fields, the secondary index (`SecIndex::build` needs all payloads at
once). These are the smaller / opt-in costs; streaming them would require a
chunked `.dir` format and an incremental `SecIndex` builder, deferred until a
profile says they matter.

### 2. Bound the per-checkpoint work (keeps it off the checkpoint's critical path)

`auto_compact` now compacts **at most one collection per checkpoint** (the first
over-threshold in id order). A checkpoint's added latency is therefore a single
collection's streamed, memory-bounded merge — not a fan-out across every
over-threshold collection. Remaining collections compact on subsequent
checkpoints, so multi-collection compaction amortises across the checkpoint
stream.

### Deferred: fully off-lock background compaction

The remaining availability cost is that a *single* large collection's merge still
runs under the write lock for its duration. Moving that fully off the lock is the
documented next step, and it is the ADR-0062 shape applied to compaction:

- **plan** (brief lock): snapshot the immutable source `SealedSegment`s to merge
  and the live-row plan; the sources are sealed and immutable, so they can be read
  without the lock;
- **build** (no lock): stream the merged segment to disk with `BlockWriter`;
- **commit-or-abort** (brief lock): if a concurrent checkpoint changed that
  collection's segment set since the plan, *abort* (discard the built files as
  orphans, retry later); otherwise do the atomic manifest swap + repoint. Abort-
  on-race keeps correctness simple — compaction is best-effort maintenance, so a
  wasted build is acceptable.

It is deferred, not done, because it requires threading the plan/build/commit seam
through the core→server boundary and sharing immutable segment handles across a
lock drop — an XL with real concurrency surface, not justified at today's
single-box scale, where streaming (bounds memory) plus one-collection-per-
checkpoint (bounds the fan-out) removes the acute pain.

## Consequences

- **+** Compaction memory is bounded to ~one page per column plus the row
  directory — no more 2× materialisation of the live set; disk-resident
  collections compact without pulling the dataset into RAM.
- **+** A checkpoint absorbs at most one collection's compaction, so checkpoint
  latency no longer fans out across collections.
- **+** No on-disk or wire format change; `BlockWriter` is byte-identical to
  `write_blocks`; the crash contract (old segments valid until the swap) is
  preserved and tested.
- **−** A single very large collection's merge still holds the write lock for its
  duration (the deferred off-lock worker addresses this).
- **−** The row directory and (filterable-only) secondary index are still built
  in RAM during a merge.

## Alternatives considered

- **Ship the full off-lock background worker now.** The higher-value availability
  fix, but XL and concurrency-heavy across the core/server boundary; deferred with
  its design recorded above rather than rushed.
- **Partial compaction (cap segments merged per pass)** to bound a single
  collection's per-checkpoint time. Bounds time, but needs segment-index remapping
  (merge a subset, keep the rest) with real correctness risk; not worth it before
  the off-lock worker, which subsumes the concern.
- **Leave compaction inside checkpoint unbounded.** The status quo; rejected — it
  couples the frequent durability op to the occasional heavy maintenance op.
