# ADR-0076: Resident per-segment id-range fingerprint

- **Status:** Accepted
- **Date:** 2026-07-24
- **Deciders:** Achref Soua

## Context

[ADR-0072](0072-on-disk-primary-index.md) moved the primary index off the heap:
external ids live in an on-demand-paged `.ids` column, and a forward lookup
binary-searches a resident sorted-row index, **decrypting the candidate row's id
from disk on every comparison** (decrypt-on-compare). Segment count is bounded by
compaction (`COMPACT_MIN_SEGMENTS`, ADR-0068), so `Store::locate` probes each
sealed segment newest-first at `O(segments · log rows)` *decrypts*.

Every write goes through `locate`: `apply_upsert` and `apply_delete` call it to
find whether the id already lives in a sealed segment (so the old row can be
tombstoned), and `get` calls it directly. Ingest is therefore single-threaded and
**slows as rows accumulate** — each new id, though absent from every sealed
segment, still pays a full decrypting binary search per segment before `locate`
returns `None`.

A 100M-on-the-laptop trial (2026-07-21) confirmed this is the ingest bottleneck at
scale. ADR-0072 named the fix as its own upgrade path: *"a small resident id
fingerprint … resolving most comparisons without a decrypt."*

## Decision

Give each sealed segment a **resident min/max id-range fingerprint** and
range-reject in `lookup` before the decrypting binary search:

- `SealedSegment` gains `id_range: Option<(String, String)>` — the segment's
  smallest and largest external id. `sorted_rows` is already id-sorted, so the
  min/max are the ids of its first and last rows. `open_segment` decrypts **exactly
  those two ids** and holds them; empty segments carry `None`.
- `lookup(id)` returns `None` immediately when `id` sorts outside `[min, max]`,
  using the same lexicographic `str` ordering the binary search uses. In range, the
  existing decrypt-on-compare search runs unchanged.

**Why min/max range, not a bloom filter.** The hard constraint is *RAM unchanged* —
ADR-0072 spent real effort getting resident state from ~31 GiB to ~1.6 GiB@100M,
and a bloom at ~8 bits/pt would add ~100 MiB there and require an on-disk format
bump to persist. The range is **two strings per segment** (segment count is
bounded), **derived on open from data that already exists**, so there is **no
on-disk format change and no per-point resident term**. It is the laziest fix that
holds the constraint.

**Honest limit.** Pruning power depends on id sortability. For monotonic/sortable
ids (sequences, ULIDs, timestamps — the realistic large-N shape) every new id
sorts beyond every sealed segment's max, so `locate` skips all segments with one
string compare each and **zero decrypts**. For high-entropy random ids the ranges
overlap and `lookup` falls through to the existing binary search — no speedup, but
**no regression** (one compare added). A per-segment bloom filter remains the named
upgrade path for the random-id case, to be added *only if* a measured workload
demands it.

## Consequences

- **Win — measured.** Scale harness, 1M vectors, dim 128, IVF+PQ m=16, sealing
  every 100k rows on the shared dev box (`crates/quiver-embed/tests/scale.rs`,
  `QUIVER_SCALE_INGEST_ONLY=1`):

  | | ingest wall | rate | peak RSS |
  |---|---|---|---|
  | before (decrypt-on-compare per probe) | 113.7 s | 8,792 vec/s | 567 MiB |
  | after (id-range fingerprint) | 18.7 s | 53,516 vec/s | 568 MiB |

  **~6.1× faster ingest at 1M; peak RSS unchanged** (567 → 568 MiB is noise). The
  harness ids were zero-padded (`p{idx:012}`) so they sort in ingest order — the
  realistic sortable-id case this optimization targets, and the one a 100M laptop
  run needs to be wall-clock-feasible.

- **Cost — two id decrypts per segment on open** and two resident strings per
  segment. Both are negligible and bounded by compaction.

- **No format change, no crash-gate impact.** The fingerprint is derived on open
  from the existing `.ids` column + `sorted_rows`; nothing new is written, so the
  on-disk format and the `kill -9` recovery path are untouched by construction.

- **Correctness.** The range endpoints are the true id-sorted min/max (not
  write-order), so an out-of-range id genuinely cannot be in the segment; in-range
  ids still run the full search. Guarded by a byte-format regression
  (`resident_id_range_is_the_true_min_and_max`) and the existing lookup/round-trip
  suites.

## Alternatives considered

- **Per-segment bloom filter (~8 bits/pt).** Prunes random ids too, but adds a
  resident per-point term and an on-disk format bump to persist it — both against
  the RAM-unchanged constraint ADR-0072 fought for. Deferred as the named upgrade
  path for a measured random-id workload.
- **8-byte id hash beside each `u32` in `sorted_rows`.** Avoids most compare-time
  decrypts but adds ~12 B/pt resident and collision handling (ADR-0072's own
  deferred option). Heavier than the range for the same RAM objection.
- **Global merged sorted index across segments.** True `O(log n)` lookup but
  reintroduces a global structure to merge every checkpoint — LSM machinery not
  needed while compaction bounds segment count.
