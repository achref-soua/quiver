# ADR-0079: Wiring the GPU kernel into the build and search paths

- **Status:** Accepted (design); implementation staged and **hardware-gated**
- **Date:** 2026-07-25
- **Deciders:** Achref Soua
- **Refines:** [ADR-0052](0052-gpu-acceleration.md) (GPU acceleration — the kernel
  and the feature gate)

## Context

[ADR-0052](0052-gpu-acceleration.md) shipped the GPU **batch-distance kernel** in
`v0.28.0`: `quiver_index::gpu::batch_l2_sq` computes one query against `n`
contiguous rows on a CUDA device (cudarc + NVRTC, loaded at runtime) and falls
back to the CPU SIMD kernel when the `cuda` feature is off or no device is
present. It was validated against the CPU kernel on real hardware.

It is wired into **nothing**. Grepping the workspace for `batch_l2_sq` finds only
its own module and tests: every distance in the engine still goes through
`quiver_simd` one row at a time. ADR-0052 said as much ("wiring it into the
planner's exact-scan path and the IVF/k-means build kernels remains") but did not
decide *where* the seam sits, or — the harder question — **what numeric contract
the engine promises once a second backend can produce a distance**. That
undecided contract, not the kernel, is what blocks the wiring.

This ADR settles both, so the implementation is mechanical follow-through on
hardware rather than a design exercise performed under time pressure with a GPU
rented by the hour.

### Where the arithmetic actually is

Distance work in Quiver concentrates in four places (`dim`-length squared-L2 or
the negated similarity — same shape):

| Site | Shape | Work |
| --- | --- | --- |
| k-means **seeding** (`kmeans.rs`, k-means++) | 1 centroid × `train_n` points, `nlist` times | `nlist · train_n · dim` |
| k-means **Lloyd assignment** (`kmeans.rs`) | `train_n` points × `nlist` centroids, per iteration | `iters · train_n · nlist · dim` |
| IVF **build assign** (`ivf.rs` `build` / `build_streaming`) | every point × `nlist` centroids | `n · nlist · dim` |
| Exact **scan / re-rank** (IVF `Storage::Flat` posting scan, `disk.rs` shortlist re-rank) | 1 query × candidate rows | per query |

Training is capped at `TRAIN_SAMPLE = 262_144` rows, so k-means is bounded work;
the **assign** pass is not — it is `O(n)` and, at `n = 100M`, `nlist = 4096`,
`dim = 128`, it is on the order of `5 × 10¹³` multiply-adds. That single pass is
the largest arithmetic block in a build, and it is the canonical
embarrassingly-parallel "many points × many centroids" matrix. It is where a GPU
earns its keep; everything else is a bonus.

All four sites reduce to one primitive — **one vector against a contiguous batch,
argmin or top-k over the result** — which is exactly the primitive already
implemented.

## Decision

### 1. The seam stays `quiver_index::gpu`, generalized

No new backend trait, no `Compute` enum threaded through the index types. The
module already *is* the seam: a free function that dispatches to the device when
one is present and to the SIMD kernel otherwise. It grows two capabilities and
nothing else:

- **Metric coverage.** `batch_ordering_distance(metric, query, batch, dim)`
  returning the smaller-is-closer ordering key of `score::ordering_distance` for
  all three metrics (L2, Dot, Cosine), so callers do not each re-derive the
  orientation. Device kernels exist for the metrics that have them; the rest fall
  back per-metric to the CPU kernel. `batch_l2_sq` stays as-is.
- **A fused assign.** `batch_assign(points, centroids, dim) -> Vec<u32>` — the
  argmin over centroids for each point, computed **on the device**. This is not a
  convenience: the intermediate `points × centroids` distance matrix is
  `1M × 4096 × 4 B = 16 GB` at a modest scale and simply cannot be returned to the
  host. The kernel must tile over points and reduce to one `u32` per point.

*(ponytail: no `Compute` backend enum and no per-index device handles. One module,
two functions, runtime auto-detect — the same shape that already works.)*

### 2. The numeric contract: **the GPU narrows, the CPU scores**

A GPU float sum does not associate the way the SIMD kernel's does, so
cross-backend bit-identity is not achievable and will not be promised. Instead:

- **Every distance handed to a caller is computed by the CPU SIMD kernel.** On a
  search path the device produces a *shortlist* (`k' = 4k`, or the probed
  candidate set); the final ordering key and the reported metric are recomputed on
  the CPU for those `k'` rows only. So the values a client sees, the recall
  tables, and every existing assertion are **bit-identical to a CPU-only run**;
  the rescoring cost is `O(k')`, negligible against the `O(n)` scan it follows.
  The only reachable difference is a shortlist boundary tie within float epsilon —
  semantically meaningless, and it cannot change a *reported* number.
- **Build-side output may differ across backends, and that is stated, not hidden.**
  A GPU-assisted k-means can land on different centroids than a CPU run of the
  same seed (a tie in the argmin resolves differently). Determinism is therefore
  **per-backend**: a fixed seed on a fixed backend reproduces exactly, as today.
  The persisted artifacts stay equally valid — codebooks are a heuristic, not a
  ground truth — and `build_is_deterministic` remains true on the CPU backend that
  CI runs. Forcing cross-backend identity would mean re-deriving every argmin on
  the CPU, which is the entire cost being avoided.
- **Nothing persisted changes.** Segment formats, index snapshots, the descriptor,
  the WAL, and the `kill -9` crash gate are untouched by construction: the GPU
  computes floats in RAM and writes no byte to disk. No format version moves.

### 3. Dispatch policy

- **CPU is the default and the fallback, always compiled.** A default build has no
  `cudarc`, no CUDA, and byte-identical behaviour to today.
- **Threshold.** Below `GPU_MIN_BATCH` rows the CPU kernel wins outright — PCIe
  transfer plus launch latency exceeds the arithmetic. Start at **4096 rows** and
  **calibrate on the target card**: this is a hardware constant, not a universal
  one, and it stays a named constant precisely so it can be tuned against a real
  device rather than guessed at forever.
- **Escape hatch.** `QUIVER_GPU=0` forces the CPU path in a `cuda`-enabled build.
  Needed for A/B measurement and for reproducing a CPU-backend build on a GPU box;
  one check inside the existing `OnceLock` context.
- **Observability.** Which backend a build used is logged once, not per batch.

### 4. Non-goals (explicitly not built)

- **No dataset residency in VRAM.** Vectors stream in bounded tiles. Capping a
  dataset at VRAM size contradicts the memory-frugality wedge — the disk path
  stays the scale story, the GPU is a throughput accelerator over it
  (ADR-0052, unchanged).
- **No GPU in PQ or binary scan.** Those are byte-code table lookups and popcounts
  (memory-bound, tiny per candidate); the LUT/SIMD path is already the right
  kernel. The GPU touches float distances only.
- **No GPU graph construction.** HNSW/Vamana build is pointer-chasing and
  inherently serial per insertion; only its distance batches could move, and that
  is a later, separate question.
- **No Metal, no wgpu.** ADR-0052's deferral stands.
- **No GPU in CI.** GPU tests stay `cuda`-gated and self-skipping (a no-op where no
  device exists), exactly like the existing one. **Every GPU performance number is
  owner-run on real hardware, or it does not exist** — the same discipline as the
  100M reference-hardware run and the multi-node Raft number.

### 5. Staging

| Increment | Content | Gate |
| --- | --- | --- |
| **G1** | The seam: metric-aware `batch_ordering_distance`, the `batch_assign` signature with its CPU implementation, the threshold constant, `QUIVER_GPU=0`. Pure CPU, fully testable everywhere. | self-doable |
| **G2** | Build wiring: k-means++ seeding pass, Lloyd assignment, IVF `build`/`build_streaming` assign (the streaming path accumulates a bounded tile of points before dispatching). | needs a device to be worth landing |
| **G3** | Search wiring: IVF `Storage::Flat` posting scan and the DiskVamana shortlist re-rank, under the narrow-then-rescore rule. | needs a device |
| **G4** | Hardware validation: CPU-vs-GPU equality on the shortlist contract, build-time and batch-QPS measurements on a real card, recorded in the benchmark table. | owner / real hardware |

G2 and G3 are deliberately **not** landed blind. Wiring a kernel that cannot be
executed, measured, or profiled produces code whose only proof is that it compiles
— and whose threshold constant, tile size, and fallback behaviour are guesses. The
design is the deliverable until a card is available.

## Consequences

- **+** The decision that was actually blocking the wiring — the numeric contract —
  is settled, and it is settled conservatively: no reported number, no test, and no
  persisted byte changes when a GPU appears.
- **+** The largest arithmetic block in a large build (the `O(n · nlist · dim)`
  assign pass) has a defined, tiled, VRAM-bounded path onto the device.
- **+** The default build, the dependency graph, `cargo deny`, and the single-binary
  story are unchanged; the accelerator stays behind an off-by-default feature.
- **−** Two backends can produce two different (both valid) codebooks for the same
  seed. Documented as per-backend determinism rather than papered over.
- **−** A real maintenance surface that CI cannot exercise. Mitigated by keeping the
  seam one module wide and the GPU strictly non-authoritative for reported values.
- **−** The win itself remains unmeasured until hardware exists. No speedup is
  claimed here, and the work counts above are arithmetic, not benchmarks.

## Alternatives considered

- **Return the full `points × centroids` distance matrix and argmin on the host.**
  Rejected on arithmetic: 16 GB of intermediate at 1M × 4096, and the device→host
  copy would dominate the kernel it is meant to accelerate. The argmin must fuse
  into the kernel.
- **GEMM formulation (`‖a‖² + ‖b‖² − 2a·b` via cuBLAS), the FAISS approach.**
  Materially faster than a hand-written per-row kernel, and the natural G4+ lever.
  Rejected for now because cuBLAS is a link-time CUDA dependency, which destroys
  the current property that the `cuda` feature *builds* without a CUDA toolchain
  installed (NVRTC compiles the kernel at runtime). Revisit once there is a
  measured need and a device to measure it on.
- **A `Compute { Cpu, Cuda, Metal }` backend enum threaded through the index
  types**, as ADR-0052 sketched. Rejected as over-built for one optional backend:
  the dispatch already lives in one module, and an enum with one non-default
  variant is a config knob nobody has asked to set. Revisit if a second device
  backend ever lands — that is the point at which the abstraction earns itself.
- **Promise bit-identical results across backends.** Rejected as undeliverable
  (float associativity) and, worse, as the kind of promise that is quietly broken
  later. The narrow-then-rescore rule delivers the property that actually matters —
  identical *reported* values — without claiming the one that cannot hold.
- **Land G2/G3 now, unvalidated.** Rejected: untestable, unmeasurable code with
  guessed constants, on a path that is off by default and therefore invisible to
  every user until it is wrong.
