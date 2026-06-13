# Index Design

The index engine (`quiver-index`) is the centerpiece. Indexes are **pluggable per collection** behind a common trait, so a collection picks the point on the recall / latency / **memory** surface it needs. The decisions are recorded in [ADR-0007](../adr/0007-index-roadmap.md) (index roadmap) and [ADR-0008](../adr/0008-quantization.md) (quantization); the distance math is in [`distance-kernels.md`](distance-kernels.md).

## The tradeoff surface

| Index | RAM resident | Disk | Recall | Latency | Build | Best for |
|---|---|---|---|---|---|---|
| **HNSW** (Phase 1) | graph + vectors | — | very high | lowest | medium | small/hot collections in RAM |
| **Vamana / DiskANN** (Phase 2) | PQ codes + node cache | graph + full vectors | high | low–med (SSD-bound) | slow | large collections, frugal RAM |
| **IVF (+PQ / SPANN)** (Phase 2) | centroids (+ codes) | posting lists | med–high | med | fast | predictable RAM, fast build, fallback |

Memory frugality is the headline; the **disk-resident path is risk R1** and is de-risked with the analytical budget below before any code is written.

## HNSW — Phase 1, in-memory

Hierarchical Navigable Small World graphs (Malkov & Yashunin, *IEEE TPAMI* 2020; arXiv:1603.09320). A multi-layer proximity graph: greedy descent through sparse upper layers to an entry region, then an `ef`-bounded best-first search at the dense base layer.

- **Parameters:** `M` (neighbors/node/layer; base layer `2M`), `efConstruction`, `efSearch`, level factor `mL = 1/ln(M)`. Recall/latency tuned by `efSearch` at query time.
- **Neighbor selection:** the paper's *heuristic* (keep diverse neighbors, not merely the nearest) — materially better recall on clustered data than naive top-`M`.
- **Memory layout:** base-layer adjacency in a flat arena of fixed `2M` `u32` slots per node for cache locality; upper-layer lists (held only by the few nodes promoted above L0) in a compact side structure. Vectors are referenced by row id into the columnar store — full-precision for exact distance, or quantized codes when compressed.
- **Concurrency:** built by the single writer; traversed lock-free by readers via atomic adjacency publication + EBR (see [`../concurrency/model.md`](../concurrency/model.md)).

## Vamana / DiskANN — Phase 2, disk-resident (the memory-frugal core)

DiskANN (Subramanya et al., *NeurIPS* 2019). A single flat **Vamana** graph (degree `R`, build list `L`, prune slack `α≈1.2`) laid out on SSD so each node co-locates its adjacency *and* its full-precision vector in one disk block — **one random read per hop**.

- **RAM holds only PQ-compressed vectors** (to navigate with approximate distances) plus a hot-node cache; the graph and full vectors stay on SSD. The candidate set returned by the beam search is **re-ranked with exact distances** by fetching full-precision vectors from SSD.
- **Parameters:** `R` (e.g. 64–128), `L`, `α`, beam width `W` (parallel SSD reads/hop).

## IVF (+ PQ / SPANN) — Phase 2, the predictable-memory fallback

Inverted file: a coarse k-means quantizer partitions space into `nlist` Voronoi cells; a query probes the `nprobe` nearest cells. Combined with PQ (IVFADC; Jégou et al., *IEEE TPAMI* 2011) or with on-disk posting lists (SPANN; Chen et al., *NeurIPS* 2021, which keeps centroids in RAM and balanced posting lists on SSD). IVF gives a **tighter, more predictable RAM profile** (essentially just centroids) and fast builds, at somewhat lower recall-per-IO than a good graph — hence its role as the **R1 fallback** if a Vamana RAM budget slips.

## Quantization (ADR-0008)

- **Scalar (SQ):** f32 → int8 per-dim min/max. 4× smaller, fast, good default for light compression.
- **Product (PQ):** split `dim` into `m` subspaces, k-means (256 centroids → 1 byte/subspace); asymmetric distance via precomputed lookup tables. The workhorse for RAM-resident codes.
- **Binary (BQ):** 1 bit/dim (sign); Hamming via SIMD popcount as a fast **pre-filter**, then exact re-rank. ~32× smaller; strong for high-dim normalized embeddings.
- **Re-rank flow:** approximate distance (SQ/PQ/BQ) → candidate set → exact full-precision distance → final top-k. The candidate multiplier is the recall ↔ latency/memory knob.

## Analytical memory budget — de-risking R1 (768-dim f32 embeddings)

| Representation | Bytes / vector | vs full |
|---|---|---|
| Full precision (f32) | 3072 | 1× |
| SQ int8 | 768 | 4× |
| PQ, m=192 | 192 | 16× |
| PQ, m=96 | 96 | 32× |
| Binary | 96 | 32× |
| HNSW base adjacency (M=16) | 128 | — |

**10M × 768-dim:** full vectors = **30.7 GB** (won't fit a laptop). DiskANN keeps **PQ codes (m=96) ≈ 0.96 GB** in RAM + a node cache, with full vectors + graph (~32 GB) on SSD → **serve 10M from ~1–2 GB RAM**. **100M** ⇒ ~9.6 GB PQ in RAM (a 32 GB workstation) + ~320 GB SSD. **Honest scope:** *billion*-scale DiskANN needs a server (~64 GB RAM); on a 16–32 GB laptop/workstation the disk path comfortably serves **tens to a few hundred million** vectors. SIFT1M (128-dim) full-precision HNSW is ~0.5 GB of vectors + ~0.13 GB adjacency — trivially in RAM.

**De-risk plan:** (1) this budget + a recall model from the cited papers (now); (2) a `.scratch` spike on a public 1–10M set measuring real recall vs RAM for chosen `(R, m, W)` before Phase 2 implementation; (3) prove at 10M+ in Phase 2 benchmarks. Fallback: IVF+PQ.

## Filtered search

The planner chooses **pre-filter** (build the allowed-row roaring bitmap from secondary indexes, constrain traversal to allowed nodes) when the predicate is selective, vs **post-filter** (search, then drop) when it is broad — because aggressive pre-filtering can disconnect a proximity graph. v1 uses this selectivity-based strategy; specialized filtered-graph search (e.g. Filtered-DiskANN, Gollapudi et al. *WWW* 2023; ACORN, Patel et al. *SIGMOD* 2024) is a later enhancement. Details in the query design.

## Incremental updates (later)

v1 rebuilds/compacts index segments on flush. In-place incremental update for the disk graph (SpFresh; Xu et al., *SOSP* 2023) is a Phase 4 item; until then, updates land in the in-memory HNSW segment and are folded into the disk index at compaction.

## References

1. Malkov, Yashunin. *Efficient and robust ANN search using HNSW graphs.* IEEE TPAMI, 2020.
2. Subramanya et al. *DiskANN: Fast, accurate billion-point NN search on a single node.* NeurIPS, 2019.
3. Chen et al. *SPANN: Highly-efficient billion-scale ANN search.* NeurIPS, 2021.
4. Jégou, Douze, Schmid. *Product quantization for nearest neighbor search.* IEEE TPAMI, 2011.
5. Xu et al. *SpFresh: Incremental in-place update for billion-scale vector search.* SOSP, 2023.
6. Gollapudi et al. *Filtered-DiskANN.* WWW, 2023. · Patel et al. *ACORN.* SIGMOD, 2024.
