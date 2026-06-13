# ADR-0021: Tombstones and compaction

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

The row-addressed storage engine (ADR-0020) leaves deleted and shadowed
(superseded-by-update) rows physically present in immutable segments. Without a
tombstone record they would be resurrected on recovery, and without compaction
they would accumulate forever. This ADR records how tombstones are represented
and made durable, and how compaction reclaims the space — both under the hard
constraint that the `kill -9` crash gate (R3, ADR-0005) stays green.

## Decision

**Per-segment roaring tombstone bitmaps (`seg-NNN.del`).** Each segment has an
optional `.del` file: a `roaring` bitmap of *its own* row indices that are no
longer live. A delete or an update looks up the id's current location; if it is a
sealed row, that `(segment, row)` is recorded and, at the next checkpoint, merged
into the segment's `.del`. A row tombstoned in its segment is skipped on recovery,
so each external id is live in exactly one segment.

**`.del` is written atomically; it is not embedded in the manifest nor
generation-numbered.** Unlike the immutable `.vec`/`.pay`/`.dir`, a `.del` is
rewritten as rows die, so it is written to a temp file and `rename`d into place
(then the directory is `fsync`'d). The manifest does not reference `.del` — a
segment's bitmap is simply loaded if the sibling file exists. This keeps the
manifest small (it is rewritten every checkpoint) and avoids generation
bookkeeping.

**Crash-safety rests on three facts**, not on cross-file atomicity between `.del`
and the manifest:

1. Each `.del` write is atomic (temp + rename), so a crash never leaves a torn
   bitmap — only the previous `.del`, or its absence.
2. Deletes and shadows are **monotonic**: a row, once dead in an immutable
   segment, is dead forever. So a `.del` that a crash left *ahead* of the
   committed manifest only marks rows that are genuinely dead — never a false
   tombstone.
3. The **WAL is the backstop**: a delete/update is `fsync`'d to the WAL before any
   `.del` is touched, and the WAL tail is replayed on open. So a `.del` update a
   crash *lost* (it had not yet been written) is reconstructed by WAL replay,
   which re-derives the same dead row.

**Compaction merges a collection's sealed segments into one fresh segment** that
holds only its live rows, via the same crash-safe protocol as a flush: write the
new segment + `fsync`, swap the manifest atomically, then reclaim the old files
(after dropping their `mmap`s). Compaction does not touch the WAL or
`last_checkpointed_lsn` — it only reorganizes already-checkpointed data — and
leaves the active buffer untouched, so un-checkpointed rows are unaffected and
still recovered from the WAL. A crash before the swap leaves the pre-compaction
state intact; after it, the old segments are orphans, GC'd on the next open.

**Compaction is both explicit and automatic.** `compact()` merges any collection
with reclaimable space; a checkpoint additionally auto-compacts a collection once
it has accumulated many segments or a large fraction of dead rows.

## Consequences

- **+** Deletes/updates are O(1) bitmap edits; recovery skips dead rows directly;
  compaction reclaims space and bounds segment fan-out; the crash gate is
  unaffected (it exercises checkpoint *and* auto-compaction under `SIGKILL`).
- **−** A delete is only physically reclaimed at the next compaction, not
  immediately (standard for an LSM-style store).
- **−** Compaction currently materializes a collection's live rows to rewrite them
  densely; streaming compaction and a page-buffer cache are future optimizations.

## Alternatives considered

- **String-id tombstone lists in the directory** (the v2 interim) — rejected:
  no per-segment row bitmap for compaction or for the hybrid-search pre-filter,
  and liveness needs replay to compute.
- **Embed the dead-set in the manifest** — rejected: bloats a structure rewritten
  every checkpoint; bitmaps belong in per-segment sibling files.
- **Generation-numbered `.del` referenced by the manifest** — rejected as
  unnecessary: atomic rename + monotonic deletes + WAL backstop already give
  crash-safety without the bookkeeping.
- **In-place delete-on-write into segments** — rejected: breaks segment
  immutability, snapshots, and `mmap` safety.
