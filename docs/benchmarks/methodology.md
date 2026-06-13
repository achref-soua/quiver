# Benchmark Methodology

The project's credibility rests on this document. Every memory/performance claim in the README is reproducible from here. **Adjectives are banned in the README — numbers replace them**, with a link back to these steps. We never fabricate results, and if Quiver loses on a metric we report it.

The recall ↔ latency ↔ memory tradeoffs and the tunable knobs for each quantizer and index are catalogued in [`quantization-tradeoffs.md`](./quantization-tradeoffs.md).

## What we measure

The four-way tradeoff, with **memory as the headline**:

- **Recall@k** (primary `k=10`) against exact ground truth.
- **QPS** — single-thread and saturated multi-thread.
- **Memory footprint** — process **RSS at steady state** after index load and warmup (measured identically for every system).
- **Build time** and **on-disk index size**.

The headline figure is **recall@10 vs RAM**; the classic figure is the **recall vs QPS** Pareto curve, traced by sweeping `efSearch` / `nprobe` / re-rank depth.

## Datasets

| Dataset | Dim | N | Metric | Use |
|---|---|---|---|---|
| **SIFT1M** | 128 | 1 M | L2 | Phase 1 baseline, in-memory HNSW |
| **GloVe-100** | 100 | ~1.2 M | cosine | cosine path |
| **Deep10M** (subset of Deep1B) | 96 | 10 M | L2 | Phase 2 disk path (memory headline) |
| **SIFT100M / big-ann subset** | 128 | 100 M | L2 | stretch disk-path scale |

Ground truth is the provided exact neighbors, or brute-forced with the SIMD kernels and checksum-pinned. Datasets are fetched by `bench/` (git-ignored under `bench/datasets/`) with pinned URLs + SHA-256.

## Harness

`ann-benchmarks`-style, in `bench/`:

1. Build the index (record build time + RSS + disk size).
2. Warm up (discard), then run the query set single- and multi-threaded.
3. Sweep the quality knob to trace the recall–QPS curve; record p50/p95/p99 latency at each operating point.
4. Emit raw **CSV** + the exact config used.

## Fair comparison vs Qdrant & LanceDB

- **Identical hardware**, same datasets, same recall operating point.
- Competitors configured per **their own** recommended settings (documented verbatim); we do not handicap them.
- **RSS measured the same way** for all systems (steady-state resident set), so the memory comparison is apples-to-apples.
- We publish the competitor versions, configs, the harness, and raw CSVs so anyone can reproduce or challenge the numbers.

## Reproducibility & honesty

- Pinned dataset versions + checksums; fixed RNG seeds; pinned Quiver + competitor versions.
- **Reference hardware is documented** with each result set: CPU model + core count, RAM, SSD model, OS/kernel, Rust version. Official numbers come from this documented reference hardware — the day-to-day dev box is resource-shared, so it is **not** the source of published numbers, and CI runs only small smoke datasets (correctness/regression gates), never the headline benchmarks.
- **Regression gates:** a fixed small dataset guards recall@10 and p95 in CI; a drop beyond a threshold fails the build.
- Results live in `docs/benchmarks/results/` (CSV + a short write-up) and are summarized as a table in the README with a link here.

## Reporting template (per result set)

```
system:        quiver vX.Y.Z | qdrant vA.B | lancedb vC.D
dataset:       Deep10M (96d, 10M, L2)
hardware:      <cpu> / <cores> / <ram> / <ssd> / <os> / rustc <ver>
operating pt:  recall@10 = 0.95
metrics:       QPS(1T), QPS(NT), RSS(GB), build(s), index_on_disk(GB), p50/p95/p99(ms)
artifacts:     config.toml, raw.csv, command line
```
