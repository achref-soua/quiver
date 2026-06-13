# ADR-0020: Row-addressed segment storage

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

Phase 1 shipped a deliberately simple segment format: each checkpoint sealed a
`postcard` blob of `{ rows: Vec<{external_id, vector, payload}>, tombstones:
Vec<String> }`, and the engine held every live row in RAM (`BTreeMap<String,
Row>`). This was correct and crash-safe but is not the format the design
specifies ([`../storage/on-disk-format.md`](../storage/on-disk-format.md),
ADR-0004): it keeps all vectors and payloads resident, has no stable per-row
address, and re-serializes whole rows on every change — leaving no substrate for
roaring tombstones, compaction, or secondary indexes.

This ADR records the decisions made when replacing it with the row-addressed
format, the last high-risk piece of the Phase-2 storage work. The hard constraint
is that the `kill -9` crash-recovery gate (R3, ADR-0005) stays green throughout.

## Decision

**Per-segment files.** Each sealed segment is three companion files named by a
monotonic segment id:

- `seg-NNNNNNNNNN.vec` — the vector column: each live row's raw little-endian
  bytes packed tightly at `row × stride`, read through an `mmap`.
- `seg-NNNNNNNNNN.pay` — the payload heap: each row's payload bytes concatenated,
  also `mmap`-read.
- `seg-NNNNNNNNNN.dir` — the row directory: a paged `postcard` blob of per-row
  `{external_id, pay_off, pay_len}` plus the ids tombstoned in this window.

`.vec` and `.pay` are flat sequences of codec-sealed 16 KiB pages
([`crate::blockfile`]); a record may straddle a page boundary, so there is no
per-record overhead and no dimensionality cap. Integrity is end-to-end: every
touched page is CRC-checked (and AEAD-authenticated when encrypted) on read, so
corruption is detected and never served.

**Vectors and payloads live on disk.** The engine reads them on demand through
the `mmap` and decrypts only the pages a query touches. Only the working set is
resident — the memory-frugality goal of the storage engine.

**The primary index is rebuilt on open, not separately persisted.** The
authoritative external-id → location map is reconstructed by reading the segment
directories oldest-to-newest (a later row shadows an earlier one for the same id;
a segment's tombstones remove ids) and then replaying the WAL tail. This is
strictly correct, mirrors the Phase-1 recovery, and reads only ids and offsets
(not vectors). A checkpointed on-disk primary index — the spec's eventual "hash
index checkpointed with the manifest" — is a deferred open-latency optimization,
not a correctness requirement.

**Snapshot-delta recovery semantics are preserved for now.** A checkpoint still
seals only the rows changed since the previous one, and deletes are still carried
as an id list in the directory's `tombstones`. This keeps the recovery algorithm
— and therefore the crash gate — identical in behavior to Phase 1; only the file
layout and the read path change. Roaring `.del` bitmaps over row ids, and the
compaction that reclaims shadowed/tombstoned rows, follow in the next PR (they
need this stable row-id model first).

**Format version bump, no in-place migrator.** The segment schema version goes
from 1 (the Phase-1 blob) to 2 (this layout). Quiver is pre-1.0 and this is a
documented layout change (ADR-0004's versioning policy): a v0.1.0 data directory
is not read in place — re-create collections on upgrade. No real deployment
predates v0.2.0, so a migrator would be dead weight.

## Consequences

- **+** Vectors/payloads are off the engine's heap and served from `mmap`; rows
  have a stable `(segment, row)` address that roaring tombstones, compaction, and
  `.sec` secondary indexes build on; arbitrary dimensionality (vectors straddle
  pages); the same crash-safety guarantee, unchanged.
- **−** A read of a sealed row decrypts and CRC-checks its page(s) every time —
  more work than the old pure-RAM read. A buffer/page cache (ADR-0004 names a page
  manager) is the natural next optimization; correctness comes first.
- **−** Open reads every segment's directory to rebuild the primary index — O(live
  ids), not O(bytes). Acceptable now; the checkpointed primary index removes it
  later.
- **−** The in-memory HNSW still holds vectors in its own arena, so for the
  default HNSW collection this does not by itself cut serving RAM — the
  memory-frugal serve path is the disk-resident index (ADR-0019). This work makes
  the *store* frugal and spec-correct and unblocks compaction and hybrid search.

## Alternatives considered

- **Keep rows in RAM, only change the file layout** — rejected: defeats the
  memory-frugality purpose and the on-disk format's reason for existing.
- **Persist the primary index now** — deferred: more write-path and crash-recovery
  surface for an open-latency win we do not yet need; rebuild-on-open is simpler
  and provably correct.
- **One combined segment file** (interleaving vectors, payloads, directory) —
  rejected: separate columns keep the vector column densely stride-addressable for
  cache-friendly scans and let `.del`/`.sec` slot in as sibling files.
- **Write a Phase-1 → Phase-2 migrator** — rejected for a pre-1.0 format with no
  real data; a documented re-create is honest and cheaper.
