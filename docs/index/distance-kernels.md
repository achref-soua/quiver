# Distance Kernels (SIMD)

`quiver-simd` is a pure-compute leaf crate: the distance functions on the hottest path in the system. Decision recorded in [ADR-0009](../adr/0009-simd-kernels.md). It has no I/O, so it is trivially unit-tested and micro-benchmarked in isolation.

## Surface

- **Metrics:** cosine, squared-L2, dot / inner-product, Hamming.
- **dtypes:** `f32`, `f16`, `bf16`, `int8`, `binary`.
- **API:** for each `(metric, dtype)` a function `fn(&[T], &[T]) -> f32` (or `u32` for Hamming), plus batched `1×N` variants that compute one query against many database vectors (the inner loop of search), returning into a caller-provided buffer with **no allocation in the hot loop**.

## Implementation strategy: stable intrinsics + runtime dispatch + scalar fallback

We use **stable Rust** with `core::arch` intrinsics, *not* nightly `std::simd` (`portable_simd`), so the project keeps a stable, reproducible single-binary build. For each kernel:

1. A **scalar reference** implementation — always correct, the differential-test oracle, and the Miri-tested path.
2. **Target-specific intrinsics** behind `#[target_feature(enable = "...")]`:
   - x86_64: **AVX2** (f32/f16/bf16 via FMA), **AVX-512F** where present, **AVX-512 VNNI** for `int8` dot (`vpdpbusd`), **AVX-512 VPOPCNTDQ** (or AVX2 + `popcnt`) for Hamming.
   - aarch64: **NEON** (`fmla`), `sdot` for int8, `cnt` for popcount.
3. **Runtime feature detection once** (`is_x86_feature_detected!` / `std::arch::is_aarch64_feature_detected!`) selects the best implementation; the choice is cached (a function pointer / `OnceLock`) so detection is not on the per-call path. A scalar fallback always exists for unknown CPUs.

Hand-written intrinsics beat autovectorization here because the compiler rarely emits optimal FMA-reduction + horizontal-sum + unrolled loops across all dtypes. We may evaluate the vetted `pulp` crate for dispatch ergonomics, but the kernels themselves are ours (the "build the hot path from scratch" core of ADR-0001).

## Numerical care

- **Cosine** = `dot(a,b) / (‖a‖·‖b‖)`; norms are precomputed and stored with vectors (and refreshed on update), so query-time cosine is one dot plus two cached norms.
- **f16/bf16** widen to `f32` accumulators; **int8** uses `i32` accumulators (VNNI/`sdot` accumulate into i32) to avoid overflow; results documented with their fp tolerance.
- **Hamming** = `popcount(a XOR b)` over the packed bit words.
- Determinism: a fixed reduction order per implementation so results are reproducible run-to-run for a given CPU path.

## Safety

Every intrinsic kernel is an `unsafe fn` gated by `#[target_feature]`; each call site carries a `// SAFETY:` note asserting the feature was detected before dispatch. Intrinsics are exercised by the differential tests (below). Because Miri cannot execute target intrinsics, **Miri runs the scalar path**, while the SIMD paths are guarded by differential equivalence — together covering correctness and UB.

## Testing & benchmarking

- **Differential tests:** for every `(metric, dtype, dim)` across a range of dims (including non-multiples of the SIMD width, to exercise the scalar tail), assert `simd == scalar` within a documented fp tolerance, on randomized and edge-case inputs (zeros, denormals, max int8).
- **Property tests:** metric axioms where they hold (e.g. L2 symmetry, identity-of-indiscernibles up to fp), Hamming bounds.
- **`criterion` micro-benchmarks** per `(metric, dtype, dim)` report throughput (GFLOP/s, vectors/s) and the speedup over scalar; these are regression-gated so a hot-path slowdown fails review.
