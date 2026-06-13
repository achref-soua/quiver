# Quantization & Index Tradeoffs

Quiver lets each collection pick its point on the **recall ↔ latency ↔ memory**
surface. This page documents the knobs and their effects so a tradeoff is a
deliberate choice, not a guess. The algorithms are specified in
[`../index/design.md`](../index/design.md) (ADR-0007) and
[ADR-0008](../adr/0008-quantization.md); the measurement rules are in
[`methodology.md`](./methodology.md).

> Compression ratios and code sizes below are exact (arithmetic from the
> implementation). Recall figures are the **host-independent** properties
> measured by the crate's recall tests; throughput (QPS) and resident-set
> (RSS) are hardware-dependent and are published only from the documented
> reference hardware — never from the shared dev box, and never fabricated.

## Quantizers

All three share one **approximate → exact re-rank** flow: rank candidates by a
cheap approximate distance over compact codes, then re-rank a shortlist of
`k × rerank_factor` with full-precision distances. `rerank_factor` is the
recall ↔ latency/memory dial.

| Quantizer | Code size (per vector) | 768-dim example | Compression | Approximate distance | Best for |
|---|---|---|---|---|---|
| none (exact) | `dim × 4` B | 3072 B | 1× | — | small / hot collections in RAM |
| **Scalar (SQ)** | `dim × 1` B | 768 B | 4× | dequantize + exact | light compression, high recall |
| **Product (PQ)** | `m` B | `m`=96 → 96 B | up to 32× (set by `m`) | asymmetric LUT (ADC) | RAM-resident navigation codes; the workhorse |
| **Binary (BQ)** | `dim / 8` B | 96 B | 32× | Hamming pre-filter (SIMD popcount) | high-dim normalized embeddings; coarse pre-filter |

Notes:

- **PQ** compression is `(dim × 4) / m`. `m` must divide `dim`; each subspace
  uses 256 centroids (1 byte/code). Larger `m` ⇒ less compression, higher
  fidelity.
- **BQ** is the coarsest. It needs a **deep re-rank pool** and shines at high
  dimensionality; at low dim, sign-pattern collisions crowd the candidate pool
  (so it is offered as an explicit fast-filter mode, not a default).
- **Cosine** collections are unit-normalized before quantization, so cosine
  reduces to inner product on the sphere.

### Recall behavior (measured, host-independent)

From the quantizer recall tests (clustered data, recall@10, with re-rank):
SQ ≥ 0.95 (4×), PQ ≥ 0.90 (16×), BQ ≥ 0.85 at 128-dim with a deep pool (32×).
Re-rank recall is **monotonic in `rerank_factor`** — a deeper pool never lowers
recall — so the dial trades latency/IO for recall predictably. The SIFT1M and
Deep10M operating points are reported in [`results/`](./results) once measured on
reference hardware.

## Indexes

| Index | RAM resident | On disk | Recall | Latency | Build | Use when |
|---|---|---|---|---|---|---|
| **HNSW** | graph + full vectors | — | very high | lowest | medium | fits in RAM, lowest latency |
| **Vamana / DiskANN** | PQ codes + node cache | graph + full vectors | high | low–med (SSD) | slow | large data, frugal RAM (the headline) |
| **IVF (Flat)** | centroids + full vectors | — | high (∝ `nprobe`) | med | fast | fast builds, exact, predictable RAM |
| **IVF + PQ** | centroids + PQ codes | — | med (PQ) | med | fast | tightest RAM, approximate |

The **disk-resident DiskANN** path is the memory-frugality headline: PQ codes
stay in RAM to navigate while the graph and full-precision vectors live on
(encrypted) SSD, and the candidate set is re-ranked with exact distances read
from disk — so a 10M-vector index serves from roughly its PQ-code footprint plus
the OS-resident working set (analytical budget in
[`../index/design.md`](../index/design.md)).

## Knobs

| Knob | Index | Effect of increasing |
|---|---|---|
| `ef_search` | HNSW | ↑ recall, ↑ latency |
| `M`, `ef_construction` | HNSW (build) | ↑ recall & graph quality, ↑ build cost & RAM |
| `R` (degree), `L` (build list), `α` (prune slack) | Vamana | ↑ recall & reach, ↑ build cost, ↑ index size |
| `l_search` | Vamana / DiskANN | ↑ recall, ↑ latency & SSD reads |
| `nlist` | IVF (build) | ↑ cells ⇒ ↓ per-probe scan, but need higher `nprobe` for recall |
| `nprobe` | IVF | ↑ recall, ↑ latency (→ exhaustive at `nprobe = nlist`) |
| `m` | PQ | ↓ compression, ↑ fidelity/recall, ↑ RAM for codes |
| `rerank_factor` | all quantized paths | ↑ recall (monotonic), ↑ latency & full-vector fetches |

## Choosing a configuration

- **Lowest latency, data fits RAM:** HNSW, no quantization.
- **Large data, small RAM budget (the wedge):** DiskANN with PQ (`m` sized to the
  RAM budget) + exact re-rank.
- **Fast builds, predictable RAM, exact results:** IVFFlat, tune `nprobe`.
- **Tightest RAM, approximate acceptable:** IVF + PQ, or BQ pre-filter + re-rank.
