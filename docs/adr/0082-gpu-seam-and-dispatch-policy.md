# ADR-0082: The GPU seam, the dispatch policy, and the measure-gate

- **Status:** Accepted
- **Date:** 2026-07-27
- **Deciders:** Achref Soua
- **Implements:** [ADR-0079](0079-gpu-build-search-wiring.md) (where the GPU kernel
  wires in, and the numeric contract it must honour), which refines
  [ADR-0052](0052-gpu-acceleration.md) (the kernel and the feature gate)

## Context

[ADR-0079](0079-gpu-build-search-wiring.md) settled the hard question — the numeric
contract — and staged the work G1–G4, with G2–G4 explicitly **hardware-gated**: "the
design is the deliverable until a card is available."

A card is now available and reachable. On the reference machine, `nvidia-smi` reports
an **RTX 3070 Laptop (8192 MiB)** under NVIDIA-SMI 610.53 / CUDA UMD 13.3, `/dev/dxg`
is present, and `LD_LIBRARY_PATH=/usr/lib/wsl/lib cargo test -p quiverdb-index
--features cuda` runs the `cuda`-gated validation test **on the device** rather than
self-skipping. The gate ADR-0079 set is therefore open.

Two things it did not settle, and this ADR does:

1. **The shape of the seam in code** — the exact signatures, where the dispatch
   policy lives, and how the escape hatch is made testable without a device.
2. **What "done" means for G2 and G3**, given that `v1.1.0` is the project's
   **final** release. A feature that ships in a closing release gets no
   field-hardening: no users, no bug reports, no follow-up release to fix what they
   find. That changes the standard of evidence required to land it.

## Decision

### 1. The seam is two free functions, and the dispatch policy lives in one place

`quiver_index::gpu` grows exactly what ADR-0079 specified and nothing more:

```rust
pub fn batch_ordering_distance(metric: Metric, query: &[f32], batch: &[f32], dim: usize) -> Vec<f32>;
pub fn batch_assign(points: &[f32], centroids: &[f32], dim: usize) -> Vec<u32>;
```

each paired with an always-compiled `cpu_*` twin that is both the fallback and the
oracle its tests compare against. No `Compute` enum, no per-index device handle, no
backend trait — the module already *is* the seam.

`batch_ordering_distance` covers all three metrics so callers stop re-deriving the
smaller-is-closer orientation; only `L2` has a device kernel, and `Dot`/`Cosine` fall
back per-metric to the CPU, which is precisely what an unported caller already did.

`batch_assign` returns one `u32` per point. This is the fused-argmin requirement from
ADR-0079 stated as a signature: the `points × centroids` intermediate is 16 GB at
1M × 4096, so a design that could return it would be the wrong design. Ties take the
**lowest centroid index**, which is what the existing `nearest_centroid` does, so the
CPU backend's output is unchanged by construction.

### 2. Three dispatch rules, each with a test that fails if it breaks

- **CPU is the default and the fallback**, always compiled. A default build has no
  `cudarc` and behaviour byte-identical to `v1.0.0`.
- **Below `GPU_MIN_BATCH` rows the CPU wins.** Extracted as `worth_dispatching(n)` so
  the policy is stated once, at one call site, and asserted directly rather than
  inferred from a timing.
- **`QUIVER_GPU=0` forces the CPU path.** Implemented as `disabled_by_env(Option<&str>)`
  — *taking the value* rather than reading the environment — so the policy is unit-tested
  without mutating a process global from a parallel test runner. Only the exact string
  `0` disables; a stray `QUIVER_GPU=1` cannot accidentally turn the accelerator off.

Which backend the process settled on is logged **once**, at context init, never per
batch — a per-batch line is one log record per query.

*(ponytail: `worth_dispatching` and `disabled_by_env` are one-line functions. They
exist as functions because they are the dispatch **policy**, and a policy that lives
inline inside a `#[cfg(feature = "cuda")]` block is a policy no default-build test can
reach.)*

### 3. `GPU_MIN_BATCH` is **8192**, measured — not ADR-0079's guessed 4096

ADR-0079 said "start at 4096 and calibrate on the target card". The sweep
(`crates/quiver-index/tests/gpu_calibration.rs`, re-runnable) puts the crossover
between 6144 and 8192 points, so the constant is **8192**.

The sweep measures the device path with the threshold **bypassed**
(`device_batch_assign`), because a sweep that respects the threshold spends its
sub-threshold rows timing the CPU against itself and calling the ratio a speedup.

**Assign pass, `dim = 128`, `nlist = 1024`, reference card (RTX 3070 Laptop, WSL2),
median of 5:**

| points | CPU ms | GPU ms | speedup | ties resolved differently |
| ---: | ---: | ---: | ---: | ---: |
| 1,024 | 11.31 | 79.87 | 0.14× | 0 |
| 2,048 | 23.49 | 79.96 | 0.29× | 0 |
| 4,096 | 43.38 | 79.97 | 0.54× | 0 |
| 6,144 | 66.14 | 80.22 | 0.82× | 0 |
| **8,192** | **86.15** | **80.47** | **1.07×** | 0 |
| 12,288 | 134.36 | 80.25 | 1.67× | 0 |
| 16,384 | 176.99 | 81.60 | 2.17× | 0 |
| 65,536 | 716.00 | 205.20 | 3.49× | 1 |
| 262,144 | 2843.28 | 739.84 | 3.84× | 1 |
| 1,048,576 | 12155.11 | 2977.06 | **4.08×** | 3 |

Two things worth reading off that table rather than glossing:

- **Device time is flat at ~80 ms up to 16 K points.** That is fixed per-dispatch cost
  (launch and synchronisation through WSL2's paravirtualised `/dev/dxg`), not
  arithmetic — which is precisely why a threshold has to exist and why it is this
  high. On a bare-metal Linux host the crossover would very likely be lower; the
  constant is documented as re-measurable for exactly that reason.
- **The "ties resolved differently" column is the per-backend determinism ADR-0079
  predicted, quantified: 3 points in 1,048,576.** The harness does not merely count
  them — it asserts each one is a genuine tie, i.e. the device's centroid sits at
  float-equal distance to the CPU's. A materially worse assignment fails the run.

**End-to-end IVF build (`n = 200,000`, `dim = 128`, `nlist = 1024`), three runs per
backend, same process except for `QUIVER_GPU`:**

| backend | runs (s) | median | |
| --- | --- | ---: | --- |
| CPU (`QUIVER_GPU=0`) | 57.88, 58.26, 57.88 | **57.88 s** | |
| GPU | 24.36, 23.96, 24.07 | **24.07 s** | **2.40× faster** |

The kernel is 4× on the assign pass and the whole build is 2.4×, which is the honest
shape: the assign pass is the largest block in a build, not the only one. The probe
asserts the built index returns 10 results before reporting any timing — the
[1.0 A/B trap](../benchmarks/scale-characterization.md) was a five-run, tight-range,
utterly convincing measurement of an index that was never built.

### 3a. The measure-gate outcome: **G2 ships, G3 is retired**

The same harness sweeps the search-side shape — one query against `n` contiguous rows,
which is what an IVF flat posting scan and a DiskVamana shortlist re-rank do. It loses
at **every** size measured:

| rows | CPU ms | GPU ms | speedup |
| ---: | ---: | ---: | ---: |
| 1,024 | 0.01 | 0.30 | 0.05× |
| 4,096 | 0.06 | 0.44 | 0.13× |
| 16,384 | 0.25 | 1.00 | 0.25× |
| 65,536 | 2.11 | 6.93 | 0.30× |
| 262,144 | 7.75 | 23.42 | 0.33× |
| 1,048,576 | 31.12 | 86.76 | **0.36×** |

Even at a million rows the device is **2.8× slower**, and the curve is still climbing
toward — not past — parity. The reason is arithmetic intensity, and it was implicit in
ADR-0079's own table without being drawn out: the assign pass reads `n × dim` floats
and does `n × nlist × dim` work, reusing every byte transferred 1024 times. A scan
reads the same `n × dim` floats and does `n × dim` work — one pass, no reuse, ~0.25
flop per byte. That is a PCIe bandwidth problem wearing a compute problem's clothes,
and no amount of kernel tuning fixes it while the vectors live in host memory.

Therefore, under the measure-gate:

- **G2 (build wiring) ships.** Wired at the two sites whose shape wins: the k-means
  Lloyd assignment step and the IVF `build` / `build_streaming` assign pass. The
  streaming path buffers a bounded 64 Ki-row tile before dispatching, so it keeps the
  property it exists for — no `n × dim` arena.
- **G3 (search wiring) is retired, not deferred.** It is measured to make search
  slower on the reference card. Shipping it behind a threshold high enough to never
  trigger would be shipping dead code with a promise attached.
- **k-means++ seeding is also not wired**, though ADR-0079 listed it as a site: it is
  one centroid against `train_n` points — the *scan* shape, on the losing side of the
  same table.

The honest one-line summary: **the GPU helps where a byte is reused a thousand times,
and hurts where it is read once.** Quiver's builds do the former; its searches do the
latter.

### 4. The measure-gate: G2 and G3 land only on a measured win

`v1.1.0` is the final release. Therefore:

> **A GPU wiring increment ships only if it is measured, on the reference card, to be
> faster than the CPU path it replaces — and if it is not, it is retired in writing
> rather than shipped on faith.**

This is stricter than the usual bar, deliberately. The normal argument for landing a
plausible optimisation — "it will get exercised and refined next release" — is not
available here, because there is no next release. The compensating fact is that the
whole path sits behind the off-by-default `cuda` feature: a default build is
unaffected either way, so the *risk* of shipping is low and the *value* of shipping
something unmeasured is zero.

The release notes state plainly that the GPU path is new in the final release and
less battle-tested than the rest of the engine. An honest caveat beats silence.

### 5. Non-goals, restated so they are not quietly reopened

Everything ADR-0079 §4 excluded stays excluded: no VRAM dataset residency, no GPU in
PQ or binary scan, no GPU graph construction, no Metal/wgpu, and **no GPU in CI** —
every GPU number is owner-run on real hardware or it does not exist.

## Consequences

- **+** The seam is testable in full on a machine with no GPU: both `cpu_*` twins, the
  threshold, and the escape hatch have direct assertions, so CI on a GPU-less runner
  gates the policy rather than merely compiling it.
- **+** The fused-argmin constraint is encoded in a *signature*, not a comment, so a
  future implementation cannot accidentally take the 16 GB path.
- **+** The measure-gate makes "we did not ship G3" a legitimate, documented outcome
  rather than a silent omission.
- **−** `GPU_MIN_BATCH` is a single global constant, not per-metric or per-dimension.
  A real crossover surface depends on `dim` as well as `n`. Accepted: one constant is
  honest and tunable; a fitted model would be unmeasurable precision.
- **−** `Dot` and `Cosine` batches never reach the device. Accepted, and now moot:
  the only site that wins is the build-side assign pass, which is L2 by construction
  (k-means is L2). The metrics that lack a device kernel are exactly the ones that
  would only ever have used the losing scan path.
- **−** The GPU accelerates **builds only**. A user expecting "GPU acceleration" to
  mean faster queries will be disappointed, so the README and the field guide say
  build-side explicitly rather than saying "GPU support" and letting the reader
  assume. This is the single most important thing to state plainly.
- **−** The wiring ships in the project's final release and therefore gets no
  field-hardening. Stated in the release notes rather than left for a user to
  discover. Containment: it is off by default (`cuda` is not a default feature), the
  CPU path is unchanged and still the fallback, and no persisted byte differs.

## Alternatives considered

- **Read `QUIVER_GPU` directly inside the `cuda` module.** Rejected: untestable
  without either a device or an env-var mutation that races every other test in the
  binary. Passing the value costs one line and makes the rule a unit test.
- **Make `GPU_MIN_BATCH` a runtime config knob.** Rejected as a config for a value
  nobody has asked to set. It is a `pub const` — readable, documentable, and
  overridable by a fork — which is the right weight for a hardware constant.
- **Land G2/G3 unmeasured and let the `cuda` feature's off-by-default status carry
  the risk.** Rejected on principle: the feature being invisible to most users is an
  argument that shipping it is *low-risk*, not an argument that shipping it is
  *valuable*. Unmeasured code in a final release is unmeasured forever.
- **Defer the whole thing and retire ADR-0079 as designed-but-never-built.** Weighed
  seriously — the hardware objection is gone but the field-hardening objection is
  not. Rejected by the owner in favour of shipping under the measure-gate above.
