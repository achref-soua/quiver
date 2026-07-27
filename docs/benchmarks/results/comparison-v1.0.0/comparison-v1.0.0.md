# Quiver v1.0.0 — Multi-DB Benchmark Comparison

_Generated: 2026-07-27 02:16 UTC_

> **Methodology:** [docs/benchmarks/methodology.md](../methodology.md) · [ADR-0037](../../adr/0037-scientific-multi-db-benchmark-suite.md)

> **Honesty note:** Every number below is real and measured. Where Quiver wins, numbers are shown; where it loses or ties, that is stated plainly. The runs are on this project's documented reference hardware — a laptop under WSL2, specified in full below. Comparative standings are meaningful because every system is measured on that same box, on the same data, in the same run. Absolute figures are laptop figures and must not be read as datacentre numbers. `[reference-hardware-pending]` still marks the few figures that need a larger machine to mean anything — the 10M disk path in particular.

## Hardware manifest

| | |
|---|---|
| OS | Linux 6.6.87.2-microsoft-standard-WSL2 |
| CPU | 12th Gen Intel(R) Core(TM) i7-12700H |
| Cores | 10 physical / 20 logical |
| RAM | 15.5 GB total, 10.8 GB available |
| Swap | 4.0 GB |
| Disk free | 756 GB |
| Load at start | 1.16, 2.12, 1.99 |
| Quiver | quiver 0.38.0 (/home/achref/projects/github-portfolio/quiver/target/release/quiver) |
| Rust | rustc 1.96.0 (ac68faa20 2026-05-25) |
| Docker | Docker version 29.4.3, build 055a478 |
| Python | 3.12.13 |

> Reference hardware for this project is the laptop described by the fields above, running under WSL2. That is a legitimate, reproducible data point and it is labelled as one: it is not a datacentre result, and absolute numbers should not be read as one. Comparative standings are meaningful because every system is measured on this same box with the same data. `load_average` is recorded so a run taken on a busy machine can be recognised as such rather than quietly published. See docs/benchmarks/methodology.md.

## Dataset: SIFT1M

### Operating point: recall@10 ≥ 0.95 (or best achieved)

| Competitor | Version | recall@1 | recall@10 | recall@100 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| faiss | 1.14.3 | 0.9755 | 0.9682 | 0.8680 | 2998 | 8198 | 2181 | 142.1 | — | ef_search=64 | reference HW · laptop/WSL2 |
| lancedb | 0.33.0 | 0.4591 | 0.5569 | 0.6337 | 157 | 308 | 1675 | 41.5 | 508.5 | nprobes=64 | reference HW · laptop/WSL2 |
| qdrant | 1.13.4 | 0.9785 | 0.9768 | 0.9369 | 267 | 329 | 263 | 182.0 | — | ef_search=32 | reference HW · laptop/WSL2 |
| quiver | quiver 0.38.0 | 0.9660 | 0.9581 | 0.9182 | 625 | 644 | 1454 | 807.3 | — | ef_search=64 | reference HW · laptop/WSL2 |

### Full ef/nprobe sweep

<details><summary>faiss</summary>

| ef/nprobe | recall@10 | QPS (1T) | QPS (NT) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|---|
| 16 | 0.8110 | 8158 | 9460 | 0.12 | 0.18 | 0.27 | 2158 |
| 32 | 0.9117 | 5099 | 8793 | 0.19 | 0.27 | 0.41 | 2160 |
| 64 | 0.9682 | 2998 | 8198 | 0.33 | 0.45 | 0.68 | 2181 |
| 128 | 0.9910 | 1596 | 6282 | 0.61 | 0.85 | 1.26 | 2172 |
| 256 | 0.9976 | 874 | 4606 | 1.15 | 1.53 | 2.07 | 2182 |

</details>

<details><summary>lancedb</summary>

| ef/nprobe | recall@10 | QPS (1T) | QPS (NT) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|---|
| 4 | 0.5160 | 237 | 336 | 4.01 | 6.01 | 7.32 | 2105 |
| 8 | 0.5454 | 236 | 328 | 4.05 | 5.77 | 7.28 | 1656 |
| 16 | 0.5548 | 220 | 314 | 4.34 | 6.13 | 7.70 | 1676 |
| 32 | 0.5567 | 185 | 344 | 4.95 | 9.34 | 13.18 | 1660 |
| 64 | 0.5569 | 157 | 308 | 6.13 | 7.99 | 9.45 | 1675 |

</details>

<details><summary>qdrant</summary>

| ef/nprobe | recall@10 | QPS (1T) | QPS (NT) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|---|
| 16 | 0.9328 | 258 | 336 | 3.08 | 9.17 | 19.98 | 472 |
| 32 | 0.9768 | 267 | 329 | 3.30 | 6.58 | 7.86 | 263 |
| 64 | 0.9936 | 250 | 305 | 3.63 | 6.66 | 8.09 | 263 |
| 128 | 0.9982 | 231 | 287 | 4.11 | 6.16 | 7.92 | 264 |
| 256 | 0.9992 | 203 | 262 | 4.79 | 6.48 | 7.60 | 264 |

</details>

<details><summary>quiver</summary>

| ef/nprobe | recall@10 | QPS (1T) | QPS (NT) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|---|
| 16 | 0.7928 | 675 | 649 | 1.35 | 2.42 | 3.21 | 1451 |
| 32 | 0.8954 | 726 | 592 | 1.31 | 1.89 | 2.46 | 1454 |
| 64 | 0.9581 | 625 | 644 | 1.52 | 2.28 | 2.96 | 1454 |
| 128 | 0.9864 | 553 | 626 | 1.73 | 2.46 | 3.17 | 1454 |
| 256 | 0.9952 | 419 | 600 | 2.30 | 3.25 | 4.11 | 1454 |

</details>

### Wins / ties / losses (Quiver vs field)

| Metric | vs competitor | Quiver | Competitor | Verdict |
|---|---|---|---|---|
| recall@10 | faiss | 0.9581 | 0.9682 | ≈ tie |
| recall@10 | lancedb | 0.9581 | 0.5569 | ✅ win |
| recall@10 | qdrant | 0.9581 | 0.9768 | ≈ tie |
| QPS (1T) | faiss | 625 | 2998 | ❌ loss |
| QPS (1T) | lancedb | 625 | 157 | ✅ win |
| QPS (1T) | qdrant | 625 | 267 | ✅ win |
| RSS (MB) | faiss | 1454 | 2181 | ✅ win |
| RSS (MB) | lancedb | 1454 | 1675 | ✅ win |
| RSS (MB) | qdrant | 1454 | 263 | ❌ loss |
| Build (s) | faiss | 807.3 | 142.1 | ❌ loss |
| Build (s) | lancedb | 807.3 | 41.5 | ❌ loss |
| Build (s) | qdrant | 807.3 | 182.0 | ❌ loss |

---

## How to read these numbers (honesty)

This run is on a **resource-shared WSL2 dev box** (specs in the manifest above). Per the risk register: comparisons run on the *identical* box under identical conditions are a fair, real result (R6) — so the **recall, QPS, and latency standings above stand**. Two things a VM distorts (R5) are **not** to be read as official headlines:

- **Absolute RSS.** Only the *isolated* systems are comparable: Quiver, Qdrant, Weaviate, and Milvus **server** report the DB process/container RSS. FAISS, LanceDB, and Chroma run in-process, so their RSS includes the Python harness **and the resident dataset** (~512 MB for SIFT1M, ~3.7 GB for GIST1M) — inflated, not directly comparable. These are **in-memory HNSW** comparisons for every system; Quiver's memory-frugality wedge is its **disk-resident DiskVamana path** (holds only PQ codes in RAM), measured separately in [`docs/benchmarks/results/disk-path.md`](./disk-path.md) — not these tables.
- **Build time.** As of v0.20.0 Quiver's build uses the **bulk-ingest** path (`POST …/points:bulk`, ADR-0045): one WAL fsync per request and a single deferred index pass, with the first query forcing the rebuild so the reported number is the honest *time-until-queryable* (the same thing every competitor's build column measures). This replaces the v0.18.0 REST-upload path (1M points in 500-point POSTs, each doing incremental index maintenance) — compare the two `comparison-*` result sets for the improvement. In-process libraries (FAISS) still build fastest because they skip the network and serialization entirely.

The **SIFT1M and GIST1M comparative standings above are dev-box but real** (R6 — identical box, identical conditions). The **QPS (NT)** column is the saturated multi-thread throughput from the concurrent driver (`--concurrency`); it is populated where a run drove more than one client thread and is the showcase for the v0.21.0 concurrent-reads work. Read it honestly: a single-process Python client (GIL + HTTP round-trip) is itself a concurrency ceiling, so for *light* queries (low `ef`, sub-2 ms) the client saturates first and NT can sit at or below 1T; the server-side concurrent-reads win shows on *heavier* queries (higher `ef`, higher recall), where NT pulls ahead of 1T. What stays pending on dedicated, otherwise-idle reference hardware (runbook [`§9`](../reference-hardware-runbook.md)): the **official absolute-RSS table**, the **full-field saturated QPS** across every competitor, and **Deep10M** (the disk-path memory headline). Milvus is benchmarked as the **server** (Docker), not the in-process Lite build, which is not performance-representative.
