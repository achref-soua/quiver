# Benchmark Methodology

The project's credibility rests on this document. Every memory/performance claim in the README is reproducible from here. **Adjectives are banned in the README — numbers replace them**, with a link back to these steps. We never fabricate results, and if Quiver loses on a metric we report it.

The recall ↔ latency ↔ memory tradeoffs and the tunable knobs for each quantizer and index are catalogued in [`quantization-tradeoffs.md`](./quantization-tradeoffs.md).

## What we measure

The four-way tradeoff, with **memory as the headline**:

- **Recall@k** at `k = 1, 10, 100` against exact ground truth (primary `k=10`). recall@1 is
  precision of the top hit; recall@100 is the wide-neighbour recall measured in one extra untimed
  pass so it never perturbs the throughput numbers.
- **QPS** — single-thread (`qps_1t`) and **saturated multi-thread** (`qps_nt`, the `--concurrency`
  driver: every query run from an `N`-thread pool — the showcase for concurrent reads, [ADR-0057](../adr/0057-concurrent-reads-rwlock.md)).
- **Memory footprint** — process **RSS at steady state** after index load and warmup (measured identically for every system).
- **Build time** and **on-disk index size**.
- **Quantization memory wedge** (Quiver) — the *same* dataset under `hnsw` / `disk_vamana`+PQ (each
  in a fresh server), recall@{1,10,100} + build + QPS, so the recall/throughput tradeoff is measured
  side by side. The **absolute serving-RAM** figure stays reference-hardware-pending: post-build RSS
  is the build's allocator high-water mark, not the cold-reload serving footprint (ADR-0061).
- **Filtered-selectivity sweep** (Quiver) — recall and QPS as a payload pre-filter keeps `s`% of the
  collection, with recall measured against the *filtered* exact ground truth.
- **Read-during-write contention** (Quiver) — concurrent read QPS *with* vs *without* a concurrent
  writer; the retained ratio is the penalty the current `RwLock` imposes (a write's exclusive lock
  blocks reads) and the measured case for lock-free MVCC reads ([ADR-0064](../adr/0064-mvcc-reads-implementation.md)).
  See [`results/read-during-write.md`](./results/read-during-write.md).

The headline figure is **recall@10 vs RAM**; the classic figure is the **recall vs QPS** Pareto curve, traced by sweeping `efSearch` / `nprobe` / re-rank depth. The dimensions added in v0.22.0 are catalogued in [ADR-0061](../adr/0061-benchmark-dimensions-v0.22.0.md).

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

1. Build the index (record build time + RSS + disk size). **Build time is *time-until-queryable*** — for Quiver this means ingest via the bulk endpoint (`POST …/points:bulk`) plus the deferred index build, which the harness forces with one query inside the timer so the number is comparable to competitors whose build includes index construction ([ADR-0055](../adr/0055-benchmark-v0.20.0-bulk-build.md)).
2. Warm up (discard), then run the query set single-threaded, and — when `--concurrency N > 1` — a
   saturated `N`-thread pass for `qps_nt`.
3. Sweep the quality knob to trace the recall–QPS curve; record recall@{1,10,100} and p50/p95/p99
   latency at each operating point.
4. Emit raw **CSV** + the exact config used. The Quiver-only `quant_sweep` and `filter_sweep`
   modules emit their own CSVs (`quant_sweep.csv`, `filter_sweep.csv`), folded into the report as the
   memory-wedge and filtered-selectivity sections.

## Fair comparison vs Qdrant & LanceDB

- **Identical hardware**, same datasets, same recall operating point.
- Competitors configured per **their own** recommended settings (documented verbatim); we do not handicap them.
- **RSS measured the same way** for all systems (steady-state resident set), so the memory comparison is apples-to-apples.
- We publish the competitor versions, configs, the harness, and raw CSVs so anyone can reproduce or challenge the numbers.

## Reproducibility & honesty

- Pinned dataset versions + checksums; fixed RNG seeds; pinned Quiver + competitor versions.
- **Reference hardware is documented** with each result set: CPU model + core count, RAM, SSD model, OS/kernel, Rust version. Official numbers come from this documented reference hardware — the day-to-day dev box is resource-shared, so it is **not** the source of published numbers, and CI runs only small smoke datasets (correctness/regression gates), never the headline benchmarks. The step-by-step procedure to produce the published numbers (Quiver + Qdrant + LanceDB) is the [reference-hardware runbook](./reference-hardware-runbook.md).
- **Regression gates:** a fixed small dataset guards recall@10 and p95 in CI; a drop beyond a threshold fails the build.
- Results live in `docs/benchmarks/results/` (CSV + a short write-up) and are summarized as a table in the README with a link here. The current run is [`comparison-v0.20.0`](./results/comparison-v0.20.0/comparison-v0.20.0.md) (SIFT1M + GIST1M, eight adapters), regenerable with `just bench-compare` + `just bench-report`.

## Reporting template (per result set)

```
system:        quiver vX.Y.Z | qdrant vA.B | lancedb vC.D
dataset:       Deep10M (96d, 10M, L2)
hardware:      <cpu> / <cores> / <ram> / <ssd> / <os> / rustc <ver>
operating pt:  recall@10 = 0.95
metrics:       QPS(1T), QPS(NT), RSS(GB), build(s), index_on_disk(GB), p50/p95/p99(ms)
artifacts:     config.toml, raw.csv, command line
```
