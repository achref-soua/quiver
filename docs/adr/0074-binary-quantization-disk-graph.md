# ADR-0074: Binary quantization for the disk graph

- **Status:** Accepted
- **Date:** 2026-07-21
- **Deciders:** Achref Soua

## Context

Quiver ships three trained quantizers ([ADR-0008](0008-quantization.md)):
`ScalarQuantizer` (~4×), `ProductQuantizer` (up to ~32×, asymmetric-LUT/ADC
distance), and `BinaryQuantizer` (1 bit/dim, ~32×, Hamming pre-filter). All three
implement the same `Quantizer`/`CodeScorer` trait and are unit-tested. But only
PQ and the (exact) flat path are wired into a serving index — `BinaryQuantizer`
has been dead outside its own tests since it landed. The roadmap's
"binary-quantization frugal-at-scale path" is that wiring.

The disk-resident Vamana index ([ADR-0063](0063-durable-disk-vamana-index.md),
`disk.rs`) is the natural home. It already holds **compact codes in RAM** for graph
navigation and keeps the **full-precision vectors on SSD**, reading them for
expanded nodes and **re-ranking the shortlist with exact distances**. Its search
returns exact metric distances regardless of how coarse the navigation codes are —
the codes only steer the beam. Today those codes are always PQ.

Binary codes are a good fit for that navigation role: at ~32× compression they are
the same order as PQ but the per-code scoring is a hardware-`popcount` Hamming
distance (`quiver_simd::hamming_u64`) rather than an ADC table lookup, and the
lossiness is fully absorbed by the exact re-rank. The other candidate seam, IVF,
returns PQ-*approximate* distances with **no** exact re-rank (`ivf.rs`), so a
Hamming pre-filter there would surface meaningless (Hamming-count) distances to the
caller — rejected.

## Decision

Make the disk graph's **resident navigation quantizer pluggable**: PQ (default) or
Binary. A collection selects binary via a new `IndexSpec.binary` flag; the disk
index build trains a `BinaryQuantizer` instead of a `ProductQuantizer`, and search
navigates by Hamming. Nothing else about the disk path changes — the node blocks
still store full-precision vectors, and the exact re-rank still produces the
reported distances.

Concretely:

- **`ResidentQuant { Pq(ProductQuantizer), Binary(BinaryQuantizer) }`** (`disk.rs`),
  `Serialize`/`Deserialize`, delegating `dim`/`metric`/`code_len`/`encode_into`/
  `scorer` to the inner quantizer. `DiskVamana`'s `pq` field and `disk::write`'s
  `pq` parameter become this enum; the single navigation site (`self.pq.scorer`)
  becomes `self.quant.scorer`. The codebook blob (already `postcard(pq)`) becomes
  `postcard(ResidentQuant)`.
- **`IndexSpec.binary: bool`** (`descriptor.rs`), `#[serde(default)]` so every
  existing descriptor still deserializes to `false` (PQ). Only meaningful for the
  disk graph; `pq_subspaces` is ignored when `binary` is set. `embed`'s
  `build_disk_index` branches on it.
- **Disk `FORMAT_VERSION` 1 → 2.** The codebook framing changed (bare
  `ProductQuantizer` → tagged enum), so a v1 file no longer decodes. The disk index
  is a **derived, rebuildable artifact that never joins the crash path**
  ([ADR-0019](0019-crash-recovery.md)/[ADR-0063](0063-durable-disk-vamana-index.md)):
  the version gate already rebuilds from the store on any mismatch, so this is a
  transparent rebuild-on-open, **no migration**.

## Consequences

- **Frugal-at-scale with exact distances.** A binary disk collection holds ~1 bit
  per dimension resident (≈ the PQ order, ~32× under full float) and returns exact
  re-ranked distances — the accuracy the beam loses to 1-bit codes is recovered by
  the on-disk re-rank, the same contract the PQ disk path already relies on.
- **Cheaper per-node scoring.** Navigation is a `popcount` Hamming over packed
  `u64` words instead of a PQ ADC lookup. Recall depends more on `l_search` (beam
  width) than with PQ, since binary codes are coarser; the beam is the knob.
- **No format migration burden.** Old disk indexes rebuild on first open; the
  store, WAL, and crash semantics are untouched (the disk index is never
  authoritative recovery state).
- **Reference-hardware recall/latency numbers are deferred**, not claimed here.
  The in-tree recall gate proves binary navigation + exact re-rank recovers
  recall@10 = 1.0 on SIFTSMALL; head-to-head RSS/latency at scale belongs on the
  dedicated box and is never fabricated (the house rule).

## Alternatives considered

- **Wire BQ into IVF instead.** Rejected: IVF has no exact re-rank stage, so it
  would report Hamming counts as distances. BQ's whole premise is pre-filter +
  re-rank, which only the disk path provides.
- **`Box<dyn Quantizer>` instead of an enum.** Rejected: the codebook must
  round-trip through `postcard`, and a closed two-variant enum serializes cleanly
  and keeps the dispatch monomorphic. A third variant (e.g. scalar) is a one-line
  addition if ever wanted.
- **Reconstruct-from-codes re-rank (no disk read).** Rejected: binary
  reconstruction is far too lossy to rank on; the disk path's full-vector re-rank
  is exactly what makes 1-bit navigation viable.
