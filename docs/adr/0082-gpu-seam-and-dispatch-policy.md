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

### 3. `GPU_MIN_BATCH` ships at 4096 in G1 and is **re-measured in G4**

ADR-0079 said "start at 4096 and calibrate on the target card". G1 is pure CPU and
cannot calibrate anything, so it carries the placeholder — and the constant's doc
comment says outright that it is a hardware number pending measurement. **G4 replaces
it with a swept, measured crossover, or the sweep is published showing why 4096
stands.** It is never left as a guess wearing the word "calibrated".

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
- **−** `Dot` and `Cosine` batches never reach the device. Accepted for now — the
  build-side assign pass, which is the largest arithmetic block, is L2 by
  construction (k-means is L2).

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
