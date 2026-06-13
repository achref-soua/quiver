# ADR-0008: Quantization strategy

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Memory frugality requires compressing vectors while preserving enough fidelity for high recall. Different workloads want different compression/recall tradeoffs, and the disk index needs RAM-resident codes to navigate. See [`../index/design.md`](../index/design.md).

## Decision

Support three quantizers, **configurable per collection**, all sharing the same **approximate-then-exact-re-rank** flow:

- **Scalar (SQ):** f32 → int8 per-dimension min/max. ~4×. Fast, simple; good light-compression default.
- **Product (PQ):** `m` subspaces × 256 centroids (1 byte/code); asymmetric distance via precomputed lookup tables. The workhorse for RAM-resident codes (DiskANN navigation); compression set by `m`.
- **Binary (BQ):** 1 bit/dim (sign); Hamming via SIMD popcount as a fast pre-filter, then exact re-rank. ~32×; strong for high-dim normalized embeddings.

**Re-rank:** the approximate stage returns `k × rerank_factor` candidates; exact full-precision distances on those produce the final top-k. `rerank_factor` is the recall ↔ latency/memory knob. **Codebook training** uses k-means on a sample of the collection, persisted as an index artifact (encrypted at rest like all index files).

Defaults: small in-RAM HNSW collections default to **no quantization** (exact); the disk path defaults to **PQ** sized to the RAM budget; BQ offered as an explicit fast-filter mode.

## Consequences

- **+** One mechanism spans 4×–32× compression; re-rank recovers recall lost to compression; users tune memory vs recall explicitly.
- **−** Quantizers need training data and add code paths; PQ lookup-table distance must be SIMD-friendly (it is). Re-rank requires fetching full-precision vectors (cheap in RAM; one SSD read batch on the disk path).

## Alternatives considered

- **No quantization** — rejected: abandons memory frugality at scale (kept as the small-collection default).
- **OPQ / anisotropic PQ** (rotation/score-aware) — better recall; deferred as a Phase-2+ refinement once base PQ is benchmarked.
- **Learned/neural quantization** — out of scope (model dependence, training cost) for v1.
