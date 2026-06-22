# ADR-0052: GPU-accelerated build & search (design only)

- **Status:** Proposed (design only — not implemented; gated on explicit owner go-ahead)
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

Index build (graph construction, PQ codebook training, k-means for IVF) and
brute-force / large-batch search are dominated by distance computation, which is
embarrassingly parallel and a natural fit for GPUs. Competitors (FAISS,
Milvus/CAGRA) offer GPU paths that cut billion-scale build time from hours to
minutes and push batch QPS far above CPU. Quiver today is CPU-only with SIMD
kernels (ADR-0009).

This ADR records the intended design without committing to build it. GPU support
is in tension with the "runs anywhere, memory-frugal, self-hostable" wedge:
GPUs are not present on most self-hosted targets, and a hard CUDA dependency
would fracture the single-binary story.

## Decision (intended design)

Add GPU acceleration **behind the existing index trait**, strictly optional and
feature-gated, never on the default build:

- **Seam.** The distance kernel (ADR-0009) and the index build/search traits are
  the only surfaces a backend must implement. A `compute` backend enum
  (`Cpu` default, `Cuda`, `Metal`) is selected at runtime from config; the CPU
  path is always compiled and is the fallback when no device is present.
- **What moves to the GPU first** (highest value, lowest coupling): (1) batch
  brute-force distance for exact search and for candidate scoring; (2) k-means /
  PQ codebook training; (3) graph-build distance batches. The on-disk format and
  the served graph stay **device-independent** — the GPU accelerates *building*
  and *scoring*, it does not change what is persisted, so the crash gate and
  ADR-0004/0005 are untouched.
- **Memory model.** Vectors stream to device memory in batches sized to the
  card; results stream back. The engine never assumes the whole dataset fits in
  VRAM (that would break the memory-frugality promise) — the disk path
  (ADR-0019) remains the scale story; the GPU is a throughput accelerator over
  it, not a replacement.
- **Build & deploy.** `--features cuda` / `--features metal`; a separate Docker
  image tag carries the CUDA runtime. `cargo deny` must stay clean — a GPU crate
  (e.g. `cudarc`) is added only under the feature, so the default dependency
  graph and the default binary are unchanged.

## Consequences

- **+** Order-of-magnitude faster build and batch search where a GPU exists;
  closes the FAISS-GPU / CAGRA gap for users who have the hardware.
- **+** Zero impact on the default CPU build, the on-disk format, or the crash
  gate — it is an optional accelerator behind a trait.
- **−** A real maintenance surface: kernels per backend, device-memory
  management, CI that most runners can't exercise (GPU tests are honestly
  marked not-in-CI, like other hardware-dependent paths).
- **−** Easy to over-invest relative to the wedge; most self-hosters have no
  GPU. Value is concentrated in large-scale build time, so scope to that first.

## Alternatives considered

- **A separate GPU index type** (parallel to HNSW/IVF) rather than a backend
  behind the trait — rejected: duplicates the index logic and the format; the
  trait seam keeps one index, two compute backends.
- **Whole-dataset-in-VRAM design (FAISS-GPU style)** — rejected as the model: it
  contradicts memory-frugality and caps dataset size at VRAM; batch-streaming
  over the disk path preserves the wedge.
- **OpenCL / wgpu for portability** instead of CUDA+Metal — deferred: broader
  portability, but materially more work and weaker kernels today; revisit if a
  portable backend matures.
- **Do nothing** — the *current* decision; SIMD on CPU (ADR-0009) covers the
  common self-hosted case. Build only when a concrete large-scale-build need and
  the hardware to test it exist.
