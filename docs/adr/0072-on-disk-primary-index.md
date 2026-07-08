# ADR-0072: On-disk primary index (Increment C)

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** Achref Soua

## Context

[ADR-0070](0070-streaming-index-build.md) (Increment A) and
[ADR-0071](0071-streaming-rebuild-vector-source.md) (Increment B) removed the
`n · dim · 4`-byte vector materialization from the index-rebuild path: the
corpus now streams from disk through `mmap`, so the vectors are no longer a RAM
wall. That leaves the **other** O(n) resident structure — the primary index.

Per collection the store holds `primary: BTreeMap<String, Loc>`
(`store.rs:244`): the live external-id → row-location map, authority for
`get`/`len`/`scan`. It is fully resident, and the external ids are in fact held
**twice** per sealed row:

1. `SealedSegment.row_ids: Vec<String>` (row order) — the reverse map, read from
   the `.dir` on open and needed to resolve a scan / secondary-index hit back to
   an id (`segment.rs:97`).
2. The `primary` `BTreeMap` — the same ids again, sorted, for forward lookup,
   rebuilt by walking every segment's `row_ids` on open (`store.rs:395`).

At ~100M rows with ~15–20-byte ids this is measured at ~316 B/pt ≈ ~31 GiB — the
dominant remaining term, and the one that keeps 100M off a modest box. The
`Loc` value itself is trivial (`Copy`, 12 B); the cost is the id strings, the
duplication, and `BTreeMap` node overhead (~2/3 of the total).

Key facts that shape the design:

- **The `.dir` already is the on-disk primary index**, in row order. The
  resident `BTreeMap` is a pure forward-lookup accelerator; it is discarded and
  rebuilt from the segments on every open. Nothing is lost by making it
  disk-resident — the durable source of truth is unchanged.
- The `.dir` is a paged, codec-sealed, `postcard`-serialized `SegmentDir` at
  `SEGMENT_FORMAT_VERSION = 3`, gated hard on open (version mismatch → error;
  `segment.rs:376`). Any layout change is a format bump, not a compat shim.
- **The `.dir` is decrypted *wholesale* on open** (`read_paged` reads the whole
  file, decrypts every page, reassembles one blob; `paged.rs:97`). That is *why*
  the ids are resident. The `.vec`/`.pay` columns, by contrast, use
  `BlockFile::read_range` — on-demand, per-page decrypt — which is exactly how
  vectors/payloads stay "on disk, decrypted on demand." Getting ids off the heap
  therefore means moving them into an on-demand-paged column too; there is no
  zero-copy `mmap` view of encrypted pages.
- Segment count is bounded by streaming compaction (ADR-0068), so a
  per-segment forward index searched newest-first is O(segments · log rows) per
  `get`, not O(n).
- The active (unsealed) rows and this-window tombstones/shadows are already
  separate, small, bounded-by-checkpoint-cadence structures (`active`,
  `active_index`, `dead_this_window`). Only the **sealed** portion of `primary`
  is the wall.

## Decision

Move sealed ext-ids into an **on-demand-paged id column** (the `.vec`/`.pay`
`BlockFile` pattern), and replace the resident `BTreeMap` with a **compact
resident forward index**. Hold the ids off the heap; hold only a small, fixed-
width lookup structure and the active/overlay set in RAM. Two internal
increments, mirroring the A→B rhythm (tested primitive first, live-path wiring
second):

**C1 — the id column + forward-index primitive (no live-path change).** Extend
the segment format (bump to v4) so a sealed segment stores its ext-ids in an
on-demand-paged **id column** written in row order — `row → ext_id` becomes a
`read_range` decrypt of one page, off the heap, exactly as payloads already work.
The resident forward index is a **sorted parallel array `[(row: u32)]` ordered by
each row's id** (4 B/pt), binary-searched by decrypting the candidate row's id on
demand to compare. Ship it as a self-contained, tested primitive: `write_segment`
emits the column + sorted order; `open_segment` loads only the fixed-width index;
`fn lookup(&self, id) -> Option<u32>` binary-searches. `row_ids()` semantics
preserved (now a lazy read, or a batch decrypt for scans).

**C2 — wire it into the store (the live-path change).** `primary` in RAM shrinks
to an **overlay only**: active-buffer rows, plus sealed rows shadowed
(re-upserted into the active buffer) or deleted. `get(ext_id)`: consult the
overlay; if not shadowed/dead there, search sealed segments **newest-first** via
each segment's `lookup`, honoring tombstones. `scan`/`len` walk the segments'
row order + tombstones as today (ids read on demand for output). Drop the global
sealed-row `BTreeMap` build on open entirely.

Result: resident primary-index RAM drops from ~316 B/pt to ~4 B/pt (the sorted
`u32` array) + the small paylocs (~12 B/pt) + the bounded overlay — roughly
~31 GiB → ~1.6 GiB at 100M. Ext-ids live in pageable, on-demand-decrypted
columns.

**Pinned decisions (2026-07-08):**

- **v4 on-disk layout.** New `seg-*.ids` on-demand-paged `BlockFile` holds each
  row's ext-id bytes in row order. `SegmentDir` (v4) drops `external_id` from
  `RowEntry`, which becomes `(pay_off, pay_len, id_off, id_len)`, and gains a
  resident `sorted_rows: Vec<u32>` — row numbers ordered by their id, the forward
  index binary-searched by `lookup`. One format bump covers both increments.
- **Migration = fail-loud.** A v3 segment opened by a v4 build errors on the
  existing version gate (`segment.rs:376`); the operator migrates by compacting.
  No auto-rewrite path — quiver is freshly public, so the upgrade papercut is
  acceptable and the alternative is materially more code.
- **Forward index = decrypt-on-compare.** `lookup` binary-searches `sorted_rows`,
  decrypting the candidate row's id from `.ids` to compare. No resident id
  fingerprint yet; it stays the named upgrade path.

## Consequences

- **Win:** the ~31 GiB@100M resident term collapses to a bounded overlay plus
  pageable `mmap`s. This is the term that gates 100M-on-a-modest-box; validating
  the RSS is deferred to the dedicated-box run (never fabricated here).
- **Cost — forward `get` now costs `O(segments · log rows)` page decrypts**, not
  one `BTreeMap` probe: each binary-search comparison decrypts the candidate
  row's id page (~27 decrypts/segment at 100M). Bounded by compaction; acceptable
  until measured otherwise. *ponytail: decrypt-on-compare + per-segment search;
  upgrade path = a small resident id fingerprint (e.g. 8-byte hash beside the
  `u32`, resolving most comparisons without a decrypt) and/or a merged global
  sorted index, added only if lookup latency at scale demands it.*
- **Cost — format bump v3 → v4.** Existing segments must be rewritten (a compact
  migrates them). Documented as a breaking on-disk change; the version gate makes
  a stale open fail loud, not silently corrupt.
- **Risk — durability-critical.** The `.dir` is the source of truth for id →
  location; a wrong offset table loses rows. Guarded by: byte-format tests in
  C1, the existing crash/recovery + MVCC suites in C2, and a round-trip
  (`write_segment` → `open_segment` → `lookup`/`row_ids` parity) regression.

## Alternatives considered

- **Decrypt the whole id blob into RAM at open, drop only the `BTreeMap`.** Keeps
  one resident id copy (row order) + a compact sorted index, no on-demand column.
  Simpler, but ids stay resident (~40+ B/pt) — a partial win that does not deliver
  the 100M thesis. The on-demand id column is the point.
- **Resident id fingerprints from the start (8-byte hash beside each `u32`).**
  Avoids nearly all lookup-time decrypts but adds collision handling and ~12 B/pt.
  Deferred as the named upgrade path; start with decrypt-on-compare and measure.
- **Embed a KV store (redb/sled) for the primary index.** A heavy dependency
  against a security-first, memory-frugal DB, duplicating paging/crypto/fsync we
  already own; sled is effectively unmaintained. Rejected.
- **Global merged on-disk sorted index (single SSTable across segments).** True
  `O(log n)` forward lookup, but reintroduces a global structure to rebuild/merge
  at every checkpoint — LSM-tree machinery we do not need while compaction bounds
  segment count. Deferred as the named upgrade path, not built speculatively.
