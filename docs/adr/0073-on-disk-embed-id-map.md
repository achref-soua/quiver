# ADR-0073: On-disk id map for the embedded index (Increment D)

- **Status:** Accepted
- **Date:** 2026-07-11
- **Deciders:** Achref Soua

## Context

[ADR-0072](0072-on-disk-primary-index.md) (Increment C) moved the **store's**
primary index — the live external-id → row-location map — off the heap into an
on-demand-paged `.ids` column plus a compact resident forward index, collapsing
the last O(n) RAM wall in `quiver-core` from ~316 B/pt to ~16 B/pt. It explicitly
scoped out a second, parallel resident term (C2 note, ADR-0072):

> the embedded index's own `int_to_ext`/`ext_to_int` residency is a separate
> resident term (indexed collections only) and is left to a follow-up.

That follow-up is this ADR. Every indexed collection in `quiver-embed` keeps its
own id maps on `CollectionHandle` (`lib.rs:274`):

1. `int_to_ext: Vec<String>` — the ANN index's dense internal id (`0..n`,
   assigned in `store.scan()` order at rebuild) → external id. The read-path
   authority: a query returns internal neighbour ids, resolved back to ext ids
   here (`lib.rs:1131`, `1384`, `1699`).
2. `ext_to_int: HashMap<String, u64>` — the reverse, the write-path oracle: every
   incremental upsert probes it to decide reuse-or-allocate, every delete to find
   the internal id to tombstone.

The external ids are thus held **twice**, resident, plus the `HashMap`'s control
bytes and duplicated key strings. At ~100M rows with ~15–20-byte ids this is
~90–110 B/pt ≈ ~9–11 GiB — now the dominant resident term for an indexed
collection at scale, and (post-C) the one that gates 100M-indexed on a modest box.

Key facts that shape the design (from the lifecycle trace):

- **The maps are a rebuildable cache, not a source of truth.** They are
  reconstructed wholesale from the store on every open/rebuild
  (`scan_collection`, `lib.rs:2663`); the durable index snapshot's version gate
  already falls back to a full rebuild on any mismatch (`load_index`,
  `lib.rs:2311`). Nothing durable is lost by changing how they are stored — the
  bar is far below C's `.dir` (which *was* the source of truth).
- **The base is exactly `store.scan()` order** at the last rebuild — a fixed set,
  captured cheaply while the scan already walks the ids in order. Between rebuilds
  it only *grows* (a tail of new internal ids); a compaction re-locates rows but
  does not change index membership, so the base survives it.
- **Persistence is heterogeneous.** The IVF envelope stores the whole `Ivf` bytes
  + whole `int_to_ext`; the DiskVamana envelope already splits an immutable
  `mmap` base (`base_row_count`) from an implied in-RAM delta. Both re-serialize
  the **entire** `int_to_ext` on every checkpoint (`lib.rs:1843`, `1855`) — an
  O(n) clone + postcard encode per checkpoint, not just a resident-RAM cost.
- **MVCC reads never touch these maps** (ADR-0064): the lock-free serving
  snapshot carries its own `base_int_to_ext: Arc<Vec<String>>` frozen at
  `publish_base`, resolved via `CollectionSnapshot::ext_id`. The writer keeps
  `handle.ext_to_int` as the update/delete oracle. Two representations already
  kept coherent; the migration must preserve that.
- **`docs` and `sparse` are ext-keyed** and never consult the id maps
  (`lib.rs:290`, `297`) — out of scope.

## Decision

Move the **base** id strings off the heap into an on-demand-paged, codec-sealed
**id column** (the `BlockFile` pattern that already backs `.vec`/`.pay`/`.ids`),
with a compact resident forward index — mirroring C's shape one layer up. Keep
only the small **post-rebuild tail** and the touched-this-window overlay resident.
Ship as one PR (the map is a rebuildable cache, so the C1/C2 split — tested
primitive, then live wiring — is not needed for durability safety here).

**The primitive — `ExtIdColumn` (`quiver-core`).** A collection-wide, codec-sealed
column written once per index rebuild. Logical layout in one `BlockFile`:

```
[ n: u64 | offsets: (n+1) × u64 | sorted: n × u32 | id bytes … ]
```

- `write(path, codec, ids)` streams the bytes through a `BlockWriter` (one page
  resident, not a second full copy of the corpus).
- `open(path, codec)` reads the fixed-width `offsets` + `sorted` into RAM
  (~12 B/pt — the resident working set) and `mmap`s the id bytes for on-demand
  reads.
- `read(internal) -> String` decrypts one page (`read_range`).
- `lookup(ext) -> Option<u64>` binary-searches `sorted`, decrypting the candidate
  row's id to compare — decrypt-on-compare, exactly as C's `SealedSegment::lookup`.

Reuses `BlockFile`/`BlockWriter`/`write_blocks` (kept `pub(crate)`); `ExtIdColumn`
is the only new public surface.

**The embed layer — `IdMap`.** `CollectionHandle`'s two fields collapse to one
`IdMap` encapsulating:

- `base: Option<ExtIdColumn>` + `base_count` — the on-disk base from the last
  rebuild (sealed under the collection's own codec into `index_dir/idmap.qic`,
  atomic tmp-rename like the DiskVamana base).
- `tail: Vec<String>` — internal ids `≥ base_count` (post-rebuild writes),
  resident.
- `overlay: HashMap<String, Option<u64>>` — ext → `Some(current internal)` if
  touched-live this window, `None` if deleted this window. Covers tail-new and
  base-shadowed/-deleted ids; the write-path oracle.

Resolution:
- `ext(internal)` (read path): `internal < base_count` → `base.read`; else
  `tail[internal − base_count]`.
- `internal(ext)` / `contains(ext)` (write path): consult `overlay`
  (`Some(i)`→i, `None`→absent); else `base.lookup`.

Result: base id **strings** leave the heap; resident id RAM for an indexed
collection drops from ~90–110 B/pt to ~12 B/pt (the base `offsets`+`sorted`) plus
the bounded tail — ~9–11 GiB → ~1.2 GiB at 100M. The checkpoint envelope stops
carrying the base strings (it carries only the bounded tail), so the per-checkpoint
O(n) id serialization goes away for the common (bulk / MVCC) paths.

**Pinned decisions (2026-07-11):**

- **Base rewritten at rebuild only**, not per checkpoint — so there is no O(n)
  column rewrite or read-back on the checkpoint path. The tail (since the last
  rebuild) is what a checkpoint serializes.
- **Envelope format bump** (`INDEX_ENVELOPE_VERSION 1 → 2`): drop `int_to_ext`,
  add `base_count` + `tail`. A stale (v1) snapshot fails the version gate and
  **falls back to a rebuild** — the existing, already-exercised path; no migration
  code, no data at risk (the snapshot is a cache).
- **No IVF rebuild-trigger change.** Non-MVCC incremental IVF upserts absorb
  forever without a rebuild (no staleness mark), so their tail — and its resident
  strings — can grow unbounded between rebuilds. The two scale-relevant paths do
  *not* hit this: bulk ingest marks stale → rebuilds (sealing a fresh base), and
  MVCC bounds the tail via its churn trigger. The pathological path is no worse
  than today (strings move from `int_to_ext` to `tail`, same residency), just
  un-improved. Left as the named ceiling below rather than changing IVF's
  rebuild behaviour in this PR.

## Consequences

- **Win:** the ~9–11 GiB@100M resident id term collapses to ~1.2 GiB of
  fixed-width metadata plus a pageable `mmap`; the checkpoint envelope no longer
  re-encodes the base ids. Validating the RSS is deferred to the dedicated-box
  `scale` run (`crates/quiver-embed/tests/scale.rs`) — never fabricated here.
- **Cost — resolution now costs page decrypts.** A query result resolves in one
  `read_range` decrypt per hit (k is small); a write-path `lookup` costs
  `O(log n)` decrypt-on-compare probes, versus one `HashMap` probe before.
  Bounded and acceptable until measured otherwise. *ponytail: decrypt-on-compare;
  upgrade path = a resident id fingerprint (8-byte hash beside the `u32` in
  `sorted`) resolving most comparisons without a decrypt, added only if write
  latency at scale demands it.*
- **Cost — open reads ~12 B/pt of fixed-width metadata** into RAM (`offsets` +
  `sorted`), versus the old whole-`int_to_ext` decode. Smaller, and it is the
  resident working set anyway. *ponytail: the `offsets` table could itself page to
  disk (a second on-demand read per lookup) if 1.2 GiB@100M ever matters; not
  built speculatively.*
- **Ceiling — unbounded non-MVCC incremental IVF tail** (above). Named, not
  fixed. *ponytail: give IVF a tail-fraction staleness trigger mirroring the
  graph's `GRAPH_REBUILD_PENDING_FRACTION` / MVCC's churn cap, so the base
  re-seals periodically; added only when that path is exercised at scale.*
- **Risk — read/write correctness across the base/tail seam and MVCC.** Guarded
  by: a byte-format round-trip test on `ExtIdColumn`, an `IdMap` unit test over
  the base+tail+overlay transitions, and the existing embed rebuild / off-lock
  rebuild / MVCC / crash-recovery suites, which exercise every resolution site.

## Alternatives considered

- **Reuse the store's `.ids` columns instead of a new embed column.** The store
  already holds every ext-id on disk (C1) and resolves ext↔Loc. But the ANN
  index's internal id is a dense `0..n` decoupled from the store's `(segment,row)`
  `Loc`, and a `Loc` is *not stable* across compaction — a cached internal→Loc map
  would dangle. No cheap ride from the store; the embed index needs its own
  internal-id-addressed column. Rejected.
- **Segment the embed column (one file per checkpoint, searched newest-first),**
  so the base is incremental like the store's. That is re-implementing C's segment
  machinery in the embed layer for a rebuildable cache — far more code and crash
  surface than the value. Rewriting the base only at rebuild sidesteps it.
  Rejected.
- **Cheaper partial win: drop only the `ext_to_int` HashMap,** deriving membership
  from a resident `int_to_ext` + the store. Halves the term but leaves ~40 B/pt of
  resident strings — a partial win that does not deliver the 100M-indexed thesis.
  The on-disk column is the point.
- **Resident id fingerprints from the start.** Avoids most lookup-time decrypts,
  adds collision handling + ~8–12 B/pt. Deferred as the named upgrade path; start
  with decrypt-on-compare and measure.
