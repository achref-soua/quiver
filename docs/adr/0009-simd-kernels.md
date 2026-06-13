# ADR-0009: SIMD distance kernels

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Distance computation is the hottest path in the system; it must be fast across CPUs and dtypes, while the build must stay on **stable Rust** for a reproducible single static binary. Rust's portable SIMD (`std::simd` / `portable_simd`) is **nightly-only**, creating a tension between portability ergonomics and a stable toolchain. Full design: [`../index/distance-kernels.md`](../index/distance-kernels.md).

## Decision

Hand-write kernels using **stable `core::arch` intrinsics** with **runtime CPU-feature detection** and a **scalar fallback** — *not* nightly `std::simd`:

- Per kernel: a scalar reference (oracle + Miri path) plus target-specific intrinsics behind `#[target_feature]` — AVX2 / AVX-512(F/VNNI/VPOPCNTDQ) on x86_64, NEON (`fmla`/`sdot`/`cnt`) on aarch64.
- Detect features once (`is_x86_feature_detected!` / aarch64 equivalent) and cache the chosen function pointer; never detect on the per-call path.
- dtypes f32/f16/bf16/int8/binary with correct accumulator widths (f32 accumulation for half precision, i32 for int8).

## Consequences

- **+** Stable toolchain, reproducible builds, single binary; best-available SIMD per CPU with a guaranteed scalar fallback; full control of FMA/reduction/unrolling on the hot path.
- **−** More code than `portable_simd` would need, and `unsafe` for intrinsics — each block carries a `// SAFETY:` note (feature detected before dispatch) and is guarded by differential tests (`simd == scalar` within tolerance). Miri can't run intrinsics, so it covers the scalar path while differential equivalence covers SIMD.

## Alternatives considered

- **Nightly `std::simd`** — cleanest portability but requires a nightly toolchain; rejected to keep stable/reproducible builds.
- **Rely on autovectorization** — rejected: inconsistent codegen for FMA-reduction across dtypes; we still keep the scalar path as the reference.
- **A third-party SIMD crate** (`wide`/`simba`/`pulp`) — `pulp` may be evaluated for dispatch ergonomics, but the kernels stay first-party per the "build the hot path from scratch" principle (ADR-0001).
