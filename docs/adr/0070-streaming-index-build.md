# ADR-0070: Streaming, memory-bounded index build

- **Status:** Accepted
- **Date:** 2026-07-02
- **Deciders:** Achref Soua

## Context

The [scale characterization](../benchmarks/scale-characterization.md) proved Quiver
serves millions of vectors on a laptop-class box and pinned the single remaining
ceiling for a true **single-box 100M** build. Storage is frugal (~530 B/vector on
disk) and query-time memory is frugal (IVF+PQ keeps only centroids + codes
resident), but the **batch index build is not**: it materialises every vector in
RAM at once.

Concretely, `quiver_embed::scan_collection` reads a collection's live rows into one
resident `flat: Vec<f32>` and hands it to `build_in_memory_index` → `Ivf::build`.
That arena is `n · dim · 4` bytes — ~5 GiB at 10M×128 and **~51 GiB at 100M×128** —
so a modest box tops out near 10M regardless of how frugal the resulting index is.
v0.31.0 already elided the *second* copy (the normalised `prepared` arena for
L2/Dot) and moved codebook training onto a 262k sample, but the primary `flat`
materialisation remains.

The immutable, sealed on-disk segments are the key: a collection's vectors live in
write-once `.vec` block files (ADR-0004/0020), already `mmap`'d. A build does not
need them copied into one contiguous heap arena — it needs to *read each row once*
to assign it to a cell and encode its PQ code. Everything the built index keeps
resident (centroids, PQ codes, postings) is already frugal; only the transient
full-precision scan is not.

## Decision

Add a **streaming build path** that reads vector rows from the immutable sealed
segments in bounded chunks and never holds the whole corpus resident. It is used
by the IVF + PQ (quantised) build — the memory-frugal path that is the point of a
100M-on-one-box story — and is transparent to callers.

**1. A chunked vector source over the sealed segments.** The store exposes a
`stream_vectors(collection) -> impl Iterator<Item = Result<&[f32]>>` (backed by the
already-`mmap`'d `.vec` block files of the live segment set at the current manifest
version). It yields each live row's vector by reference in row order without
allocating an `n·dim` arena. The segment set is immutable and write-once, so the
stream is consistent and needs **no writer lock** — it composes with the off-lock
rebuild (ADR-0062), which already captures the live segment set before building.

**2. Two bounded passes, both O(1) in resident vectors.**
- **Sample/train pass** — draw a deterministic `TRAIN_SAMPLE` (262k) reservoir
  sample from the stream and train the coarse k-means + PQ codebooks on it (already
  sample-based since v0.31.0). Resident: the sample (bounded) + codebooks.
- **Encode pass** — stream every row again; for each, find its nearest centroid
  (append to that cell's posting) and encode its PQ code into the resident code
  arena. Resident: centroids (`nlist·dim`), codes (`n·m` bytes — the frugal index
  itself), postings (`n` u32s). No full-precision `n·dim` arena ever exists.

**3. IVF-Flat (exact) keeps the batch path.** A `Flat` (non-PQ) collection stores
the full vectors resident *by definition*, so streaming saves nothing there; it
uses the existing in-RAM build. Streaming is specifically the **PQ path**, which is
the one that must scale.

**4. No on-disk format change; the crash gate is untouched by construction.** The
index is derived and rebuilt from the store; the streaming build only changes *how*
it is computed from the same immutable segments, not what is written. `kill -9`
recovery, the manifest protocol, and the write path are all unchanged.

## Consequences

- **+** Removes the build's `n·dim` RAM wall: peak build memory drops from
  `O(n·dim)` to `O(sample + nlist·dim + n·m_bytes)`. On a box with enough RAM for
  the codes + primary index, **100M becomes buildable on a single node** — the
  frugal wedge finally holds for the build, not just storage and query.
- **+** Reuses the immutable-segment guarantee for a lock-free read stream; no new
  durability surface.
- **−** The **primary index** (`ext_id → location`, `int_to_ext`/`ext_to_int`) is
  still fully resident (~316 B/point, ~31 GiB at 100M). Streaming the *vectors*
  does not remove that O(n) cost; a true 100M build on a 15 GiB box also needs the
  **on-disk / sharded primary index** (a separate, sequenced item). On a larger box
  (≈40 GiB+) the streaming build alone unlocks 100M.
- **−** Two passes over the segments read each row twice (the sample pass could be
  folded into the encode pass with an online codebook, but two clean passes are
  simpler and the read is `mmap`-cheap; the extra pass is I/O, not RAM).
- **−** Slightly more code on the build path (a streaming variant beside the
  in-RAM one); mitigated by sharing the k-means/PQ/posting primitives.

## Implementation

Shipped incrementally, correctness-first:

- **Increment A (this ADR):** `Store::stream_vectors` over the live segments'
  `mmap`'d `.vec`; an `Ivf::build_streaming` that takes the chunked source and runs
  the sample-train + encode-stream passes, byte-identical in result to `Ivf::build`
  on the same rows and seed (a property test asserts parity on small collections);
  `build_in_memory_index` routes the IVF+PQ path through it while keeping IVF-Flat
  and the other index kinds on the in-RAM path. A scale-harness tier (`≥ 20M`)
  proves the build peak RSS no longer tracks `n·dim`.
- **Sequenced next:** the on-disk / sharded primary index (the other O(n) resident
  cost), then a 100M reference-hardware run recorded honestly.

Until Increment A lands and is measured, the ceiling stays exactly what the scale
characterization states: **~10M full build on a 15 GiB box, 100M pending.** No
100M number is claimed before it is run.

## Alternatives considered

- **Do nothing / "use a bigger box."** Rejected as the answer for the frugal wedge:
  the whole product claim is *frugal on modest hardware*; a build that needs 51 GiB
  of transient RAM for data that lives frugally on disk is the one place the wedge
  leaks.
- **`mmap` the vectors and pass a borrowed slice to the existing build.** Rejected:
  vectors are spread across multiple immutable segments, not one contiguous file, so
  there is no single slice to borrow; the stream abstraction is what bridges that.
- **Online single-pass build (train the codebooks incrementally while encoding).**
  Deferred: it removes the second read but complicates determinism and the
  codebook-quality story; two clean, deterministic passes are the safe first cut,
  and the extra pass is `mmap` I/O, not resident RAM.
- **Fold this into the disk-resident DiskVamana path (ADR-0063).** Rejected as the
  general answer: DiskVamana is one index kind; the RAM wall is in the shared
  scan→build seam, so the fix belongs there and benefits IVF+PQ (the frugal ANN
  default) directly.

## Verification

- **Parity:** `Ivf::build_streaming` and `Ivf::build` produce byte-identical indexes
  on the same rows + seed (property test over random small collections).
- **Memory:** a scale-harness tier builds `≥ 20M` vectors and asserts peak build RSS
  is bounded by `sample + codes + primary index`, not `n·dim` — i.e. it does **not**
  scale with the full-precision corpus (measured, never fabricated).
- **Crash gate:** the existing `kill -9` recovery and rebuild-from-store tests pass
  unchanged (the streaming build is a derived computation over the same immutable
  segments).
