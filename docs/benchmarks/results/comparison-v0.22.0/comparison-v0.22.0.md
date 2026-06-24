# Quiver v0.22.0 — Multi-DB Benchmark Comparison

_Generated: 2026-06-24 07:14 UTC_

> **Methodology:** [docs/benchmarks/methodology.md](../methodology.md) · [ADR-0037](../../adr/0037-scientific-multi-db-benchmark-suite.md)

> **Honesty note:** Every number below is real and measured. Where Quiver wins, numbers are shown; where it loses or ties, that is stated plainly. `[reference-hardware-pending]` marks figures that require reproduction on dedicated, otherwise-idle hardware to carry weight as official headlines.

## Hardware manifest

| | |
|---|---|
| OS | Linux 6.6.87.2-microsoft-standard-WSL2 |
| Processor | x86_64 |
| Logical CPUs | 20 |
| RAM total | 15 GB |
| Rust | rustc 1.96.0 (ac68faa20 2026-05-25) |
| Docker | Docker version 29.4.3, build 055a478 |
| Python | 3.12.13 |

> This benchmark ran on a WSL2 dev box (resource-shared). QPS and RSS numbers are labelled accordingly. See docs/benchmarks/reference-hardware-runbook.md for the procedure to produce official headline numbers on dedicated hardware.

## Dataset: SIFT1M `[reference-hardware-pending]`

### Operating point: recall@10 ≥ 0.95 (or best achieved)

| Competitor | Version | recall@1 | recall@10 | recall@100 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| quiver | v0.22.0-dev | 0.9660 | 0.9581 | 0.9182 | 855 | 928 | 2022 | 730.6 | — | ef_search=64 | dev-box · indicative |

### Full ef/nprobe sweep

<details><summary>quiver</summary>

| ef/nprobe | recall@10 | QPS (1T) | QPS (NT) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|---|
| 16 | 0.7928 | 1131 | 949 | 0.82 | 1.32 | 2.03 | 2022 |
| 32 | 0.8954 | 1001 | 968 | 0.91 | 1.67 | 2.28 | 2022 |
| 64 | 0.9581 | 855 | 928 | 1.07 | 1.90 | 2.49 | 2022 |
| 128 | 0.9864 | 673 | 938 | 1.36 | 2.42 | 3.06 | 2023 |
| 256 | 0.9952 | 506 | 892 | 1.86 | 3.05 | 3.81 | 2023 |

</details>

### Memory wedge — quantization tradeoff (Quiver)

> Same dataset, best operating point per index/quantization config, **each built in its own fresh server process**. The recall/build/throughput tradeoff is what is published here: the disk-resident Vamana graph (PQ codes in RAM, full vectors on SSD) holds recall@10 close to exact in-memory HNSW while PQ trades the *deep* tail — note recall@100 falls off. The **absolute serving-RAM wedge is `[reference-hardware-pending]`**: post-build RSS on this box reflects the build's allocator high-water mark (the disk-Vamana build pages in every vector to construct the graph, and the allocator keeps those pages), not the cold-reload serving footprint where only PQ codes stay resident — so RSS is deliberately omitted rather than shown misleadingly. IVF+PQ is also omitted: its default parameters were mistuned on this run (slow build, poor recall), so a fair IVF point is reference-hardware-pending too.

| Config | recall@1 | recall@10 | recall@100 | Build (s) | QPS (1T) | ef |
|---|---|---|---|---|---|---|
| hnsw | 0.9835 | 0.9864 | 0.9439 | 618.8 | 883 | 128 |
| disk_vamana+pq16 | 0.9735 | 0.9662 | 0.7093 | 1025.5 | 598 | 128 |

### Filtered-selectivity sweep (Quiver)

> Recall and throughput as a payload pre-filter (`bucket < s`) keeps `s`% of the collection. Recall is measured against the *filtered* exact ground truth (brute force over the matching subset), so it reflects correctness under filtering, not the unfiltered neighbours. The selectivity planner crosses over between regimes: very selective filters pre-filter to an exact scan (recall ≈ 1.0, but the scan to find the subset is the latency cost), while looser filters post-filter an ANN result — which has a recall valley at mid-selectivity before recovering as more candidates survive the filter.

| Selectivity | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) |
|---|---|---|---|---|---|
| 1% | 0.9999 | 7 | 142.28 | 156.09 | 167.09 |
| 5% | 0.6179 | 7 | 134.60 | 147.00 | 156.41 |
| 25% | 0.9697 | 5 | 189.17 | 205.40 | 231.44 |
| 50% | 0.9809 | 4 | 253.60 | 273.68 | 319.82 |
| 100% | 0.9863 | 878 | 1.12 | 1.38 | 1.66 |

### Wins / ties / losses (Quiver vs field)

| Metric | vs competitor | Quiver | Competitor | Verdict |
|---|---|---|---|---|

---

## How to read these numbers (honesty)

This is a **Quiver-only** result set (the v0.22.0 dimensions, ADR-0061) on a **resource-shared WSL2 dev box** (specs in the manifest above). The full multi-DB standings live in the `comparison-v0.20.0` set; here every number is Quiver against its own exact ground truth, labelled *dev-box · indicative*.

- **QPS (NT)** is the saturated multi-thread throughput from the concurrent driver (`--concurrency`) — the showcase for the v0.21.0 concurrent-reads work. Read it honestly: a single-process Python client (GIL + HTTP round-trip) is itself a concurrency ceiling, so for *light* queries (low `ef`, sub-2 ms) the client saturates first and NT sits at or below 1T; the server-side win shows on *heavier* queries (higher `ef`, higher recall), where NT pulls ahead of 1T.
- **Memory wedge.** The recall/build/throughput tradeoff across index/quantization configs is real and published; the **absolute serving-RAM** figure is omitted, not estimated — post-build RSS on this box is the build's allocator high-water mark, not the cold-reload serving footprint, so it stays `[reference-hardware-pending]`.
- **Build time** is *time-until-queryable* via the bulk-ingest path (`POST …/points:bulk`, ADR-0045): one WAL fsync per request and a single deferred index pass, with the first query forcing the rebuild inside the timer.

What stays pending on dedicated, otherwise-idle reference hardware (runbook [`§9`](../reference-hardware-runbook.md)): the **full-field saturated QPS** across every competitor, the **official absolute-RSS table** (and the serving-RAM wedge), and **Deep10M**. IVF+PQ is omitted from the wedge — its default parameters were mistuned on this run — so a fair IVF point is reference-hardware-pending too. Never fabricated.
