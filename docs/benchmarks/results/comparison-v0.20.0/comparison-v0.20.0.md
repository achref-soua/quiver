# Quiver v0.20.0 — Multi-DB Benchmark Comparison

_Generated: 2026-06-23 10:23 UTC_

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

## Dataset: GIST1M `[reference-hardware-pending]`

### Operating point: recall@10 ≥ 0.95 (or best achieved)

| Competitor | Version | recall@10 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|---|
| chroma | 1.5.9 | 0.7895 | 577 | — | 8156 | 382.5 | — | ef_search=16 | dev-box · indicative |
| faiss | 1.14.3 | 0.9191 | 471 | — | 7526 | 443.6 | — | ef_search=256 | dev-box · indicative |
| milvus_server | v2.5.4 (server) | 0.9613 | 53 | — | 6821 | 119.7 | — | ef_search=64 | dev-box · indicative |
| pgvector | 0.7/pg16 | 0.9798 | 8 | — | 4393 | 1295.8 | 7912.3 | nprobe=64 | dev-box · indicative |
| qdrant | 1.13.4 | 0.9554 | 185 | — | 391 | 656.5 | — | ef_search=128 | dev-box · indicative |
| quiver | v0.20.0 | 0.9230 | 268 | — | 10117 | 2314.4 | — | ef_search=256 | dev-box · indicative |
| weaviate | 1.27.0 | 0.8277 | 325 | — | 8374 | 2638.3 | — | ef_search=16 | dev-box · indicative |

### Full ef/nprobe sweep

<details><summary>chroma</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.7895 | 577 | 1.74 | 2.03 | 2.30 | 8156 |
| 32 | 0.7895 | 580 | 1.74 | 2.03 | 2.30 | 8158 |
| 64 | 0.7895 | 563 | 1.79 | 2.08 | 2.40 | 8159 |
| 128 | 0.7895 | 558 | 1.81 | 2.10 | 2.35 | 8159 |
| 256 | 0.7895 | 557 | 1.82 | 2.10 | 2.32 | 8159 |

</details>

<details><summary>faiss</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.4985 | 4780 | 0.21 | 0.28 | 0.33 | 7518 |
| 32 | 0.6368 | 2941 | 0.34 | 0.45 | 0.54 | 7523 |
| 64 | 0.7582 | 1505 | 0.65 | 0.96 | 1.16 | 7526 |
| 128 | 0.8536 | 906 | 1.13 | 1.36 | 1.57 | 7526 |
| 256 | 0.9191 | 471 | 2.16 | 2.67 | 2.88 | 7526 |

</details>

<details><summary>milvus_server</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9289 | 53 | 16.38 | 29.88 | 33.15 | 6040 |
| 32 | 0.9468 | 50 | 17.14 | 31.66 | 34.62 | 6668 |
| 64 | 0.9613 | 53 | 16.45 | 29.41 | 32.52 | 6821 |
| 128 | 0.9697 | 52 | 17.40 | 31.86 | 35.39 | 7042 |
| 256 | 0.9739 | 52 | 19.08 | 22.48 | 29.16 | 7786 |

</details>

<details><summary>pgvector</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.5457 | 86 | 10.29 | 26.91 | 35.35 | 461 |
| 8 | 0.7035 | 53 | 16.66 | 39.97 | 51.58 | 4208 |
| 16 | 0.8379 | 29 | 30.96 | 71.85 | 81.80 | 4332 |
| 32 | 0.9329 | 15 | 62.66 | 122.23 | 129.74 | 4366 |
| 64 | 0.9798 | 8 | 121.01 | 194.18 | 205.30 | 4393 |

</details>

<details><summary>qdrant</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.7063 | 148 | 11.59 | 53.51 | 66.28 | 1031 |
| 32 | 0.8176 | 308 | 3.23 | 3.73 | 4.12 | 409 |
| 64 | 0.9048 | 247 | 4.07 | 4.67 | 5.06 | 393 |
| 128 | 0.9554 | 185 | 5.47 | 6.32 | 6.85 | 391 |
| 256 | 0.9790 | 142 | 7.16 | 8.28 | 8.98 | 392 |

</details>

<details><summary>quiver</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.4870 | 718 | 1.31 | 1.94 | 3.06 | 10116 |
| 32 | 0.6225 | 667 | 1.48 | 1.78 | 1.98 | 10117 |
| 64 | 0.7492 | 515 | 1.81 | 2.13 | 2.34 | 10117 |
| 128 | 0.8532 | 402 | 2.50 | 2.97 | 3.24 | 10117 |
| 256 | 0.9230 | 268 | 3.80 | 4.41 | 4.83 | 10117 |

</details>

<details><summary>weaviate</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.8277 | 325 | 3.00 | 4.20 | 4.70 | 8374 |
| 32 | 0.8277 | 402 | 2.50 | 3.05 | 3.56 | 8880 |
| 64 | 0.8277 | 418 | 2.42 | 2.79 | 3.09 | 8880 |
| 128 | 0.8277 | 404 | 2.50 | 2.89 | 3.16 | 8880 |
| 256 | 0.8277 | 405 | 2.49 | 2.89 | 3.22 | 8880 |

</details>

### Wins / ties / losses (Quiver vs field)

| Metric | vs competitor | Quiver | Competitor | Verdict |
|---|---|---|---|---|
| recall@10 | chroma | 0.9230 | 0.7895 | ✅ win |
| recall@10 | faiss | 0.9230 | 0.9191 | ≈ tie |
| recall@10 | milvus_server | 0.9230 | 0.9613 | ❌ loss |
| recall@10 | pgvector | 0.9230 | 0.9798 | ❌ loss |
| recall@10 | qdrant | 0.9230 | 0.9554 | ❌ loss |
| recall@10 | weaviate | 0.9230 | 0.8277 | ✅ win |
| QPS (1T) | chroma | 268 | 577 | ❌ loss |
| QPS (1T) | faiss | 268 | 471 | ❌ loss |
| QPS (1T) | milvus_server | 268 | 53 | ✅ win |
| QPS (1T) | pgvector | 268 | 8 | ✅ win |
| QPS (1T) | qdrant | 268 | 185 | ✅ win |
| QPS (1T) | weaviate | 268 | 325 | ❌ loss |
| RSS (MB) | chroma | 10117 | 8156 | ❌ loss |
| RSS (MB) | faiss | 10117 | 7526 | ❌ loss |
| RSS (MB) | milvus_server | 10117 | 6821 | ❌ loss |
| RSS (MB) | pgvector | 10117 | 4393 | ❌ loss |
| RSS (MB) | qdrant | 10117 | 391 | ❌ loss |
| RSS (MB) | weaviate | 10117 | 8374 | ❌ loss |
| Build (s) | chroma | 2314.4 | 382.5 | ❌ loss |
| Build (s) | faiss | 2314.4 | 443.6 | ❌ loss |
| Build (s) | milvus_server | 2314.4 | 119.7 | ❌ loss |
| Build (s) | pgvector | 2314.4 | 1295.8 | ❌ loss |
| Build (s) | qdrant | 2314.4 | 656.5 | ❌ loss |
| Build (s) | weaviate | 2314.4 | 2638.3 | ✅ win |

> **Did not complete on GIST1M:** lancedb — ran on another dataset but failed or ran out of memory here on this box (recorded honestly, never estimated).

## Dataset: SIFT1M `[reference-hardware-pending]`

### Operating point: recall@10 ≥ 0.95 (or best achieved)

| Competitor | Version | recall@10 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|---|
| chroma | 1.5.9 | 0.9772 | 995 | — | 3714 | 153.2 | — | ef_search=16 | dev-box · indicative |
| faiss | 1.14.3 | 0.9679 | 3842 | — | 1234 | 82.0 | — | ef_search=64 | dev-box · indicative |
| lancedb | 0.33.0 | 0.5568 | 219 | — | 2475 | 15.2 | 508.5 | nprobes=64 | dev-box · indicative |
| milvus_server | v2.5.4 (server) | 0.9648 | 265 | — | 1750 | 26.3 | — | ef_search=16 | dev-box · indicative |
| pgvector | 0.7/pg16 | 0.9798 | 118 | — | 1291 | 132.0 | 1083.2 | nprobe=32 | dev-box · indicative |
| qdrant | 1.13.4 | 0.9742 | 358 | — | 258 | 98.5 | — | ef_search=32 | dev-box · indicative |
| quiver | v0.20.0 | 0.9581 | 1222 | — | 2069 | 581.3 | — | ef_search=64 | dev-box · indicative |
| weaviate | 1.27.0 | 0.9829 | 647 | — | 2173 | 2309.0 | — | ef_search=16 | dev-box · indicative |

### Full ef/nprobe sweep

<details><summary>chroma</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9772 | 995 | 0.99 | 1.18 | 1.42 | 3714 |
| 32 | 0.9772 | 1008 | 0.98 | 1.13 | 1.30 | 3749 |
| 64 | 0.9772 | 1000 | 0.99 | 1.16 | 1.35 | 3750 |
| 128 | 0.9772 | 1009 | 0.98 | 1.12 | 1.29 | 3752 |
| 256 | 0.9772 | 994 | 0.99 | 1.17 | 1.41 | 3751 |

</details>

<details><summary>faiss</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.8120 | 11172 | 0.09 | 0.13 | 0.19 | 1188 |
| 32 | 0.9116 | 6877 | 0.14 | 0.20 | 0.27 | 1230 |
| 64 | 0.9679 | 3842 | 0.26 | 0.36 | 0.53 | 1234 |
| 128 | 0.9907 | 2106 | 0.47 | 0.66 | 0.88 | 1235 |
| 256 | 0.9975 | 1157 | 0.86 | 1.17 | 1.49 | 1237 |

</details>

<details><summary>lancedb</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.5174 | 429 | 2.22 | 3.04 | 3.76 | 2916 |
| 8 | 0.5460 | 395 | 2.41 | 3.33 | 4.18 | 2961 |
| 16 | 0.5549 | 362 | 2.63 | 3.58 | 4.40 | 2474 |
| 32 | 0.5567 | 293 | 3.28 | 4.28 | 5.05 | 2476 |
| 64 | 0.5568 | 219 | 4.47 | 5.28 | 6.02 | 2475 |

</details>

<details><summary>milvus_server</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9648 | 265 | 4.46 | 8.65 | 10.47 | 1750 |
| 32 | 0.9820 | 324 | 2.33 | 5.89 | 6.44 | 1570 |
| 64 | 0.9864 | 649 | 1.49 | 1.91 | 2.47 | 2075 |
| 128 | 0.9970 | 573 | 1.67 | 2.27 | 2.91 | 1280 |
| 256 | 0.9991 | 506 | 1.93 | 2.41 | 2.88 | 1276 |

</details>

<details><summary>pgvector</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.6981 | 637 | 1.47 | 2.42 | 2.89 | 580 |
| 8 | 0.8373 | 387 | 2.44 | 3.96 | 4.63 | 1278 |
| 16 | 0.9321 | 221 | 4.33 | 6.67 | 7.59 | 1286 |
| 32 | 0.9798 | 118 | 8.26 | 11.84 | 13.20 | 1291 |
| 64 | 0.9960 | 56 | 18.26 | 24.55 | 27.22 | 1295 |

</details>

<details><summary>qdrant</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9260 | 348 | 2.46 | 4.71 | 12.68 | 578 |
| 32 | 0.9742 | 358 | 2.12 | 4.51 | 5.21 | 258 |
| 64 | 0.9923 | 330 | 2.33 | 4.81 | 5.43 | 258 |
| 128 | 0.9979 | 322 | 2.57 | 5.41 | 6.03 | 258 |
| 256 | 0.9990 | 281 | 3.10 | 5.96 | 6.62 | 258 |

</details>

<details><summary>quiver</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.7928 | 1539 | 0.62 | 0.79 | 0.98 | 2069 |
| 32 | 0.8954 | 1424 | 0.68 | 0.84 | 1.04 | 2069 |
| 64 | 0.9581 | 1222 | 0.80 | 0.99 | 1.18 | 2069 |
| 128 | 0.9864 | 955 | 1.03 | 1.26 | 1.54 | 2069 |
| 256 | 0.9952 | 701 | 1.42 | 1.73 | 2.12 | 2069 |

</details>

<details><summary>weaviate</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9829 | 647 | 1.53 | 1.79 | 2.10 | 2173 |
| 32 | 0.9829 | 654 | 1.51 | 1.76 | 2.16 | 2225 |
| 64 | 0.9829 | 658 | 1.51 | 1.75 | 2.12 | 2218 |
| 128 | 0.9829 | 658 | 1.50 | 1.75 | 2.20 | 2218 |
| 256 | 0.9829 | 663 | 1.50 | 1.70 | 1.97 | 2218 |

</details>

### Wins / ties / losses (Quiver vs field)

| Metric | vs competitor | Quiver | Competitor | Verdict |
|---|---|---|---|---|
| recall@10 | chroma | 0.9581 | 0.9772 | ≈ tie |
| recall@10 | faiss | 0.9581 | 0.9679 | ≈ tie |
| recall@10 | lancedb | 0.9581 | 0.5568 | ✅ win |
| recall@10 | milvus_server | 0.9581 | 0.9648 | ≈ tie |
| recall@10 | pgvector | 0.9581 | 0.9798 | ❌ loss |
| recall@10 | qdrant | 0.9581 | 0.9742 | ≈ tie |
| recall@10 | weaviate | 0.9581 | 0.9829 | ❌ loss |
| QPS (1T) | chroma | 1222 | 995 | ✅ win |
| QPS (1T) | faiss | 1222 | 3842 | ❌ loss |
| QPS (1T) | lancedb | 1222 | 219 | ✅ win |
| QPS (1T) | milvus_server | 1222 | 265 | ✅ win |
| QPS (1T) | pgvector | 1222 | 118 | ✅ win |
| QPS (1T) | qdrant | 1222 | 358 | ✅ win |
| QPS (1T) | weaviate | 1222 | 647 | ✅ win |
| RSS (MB) | chroma | 2069 | 3714 | ✅ win |
| RSS (MB) | faiss | 2069 | 1234 | ❌ loss |
| RSS (MB) | lancedb | 2069 | 2475 | ✅ win |
| RSS (MB) | milvus_server | 2069 | 1750 | ❌ loss |
| RSS (MB) | pgvector | 2069 | 1291 | ❌ loss |
| RSS (MB) | qdrant | 2069 | 258 | ❌ loss |
| RSS (MB) | weaviate | 2069 | 2173 | ✅ win |
| Build (s) | chroma | 581.3 | 153.2 | ❌ loss |
| Build (s) | faiss | 581.3 | 82.0 | ❌ loss |
| Build (s) | lancedb | 581.3 | 15.2 | ❌ loss |
| Build (s) | milvus_server | 581.3 | 26.3 | ❌ loss |
| Build (s) | pgvector | 581.3 | 132.0 | ❌ loss |
| Build (s) | qdrant | 581.3 | 98.5 | ❌ loss |
| Build (s) | weaviate | 581.3 | 2309.0 | ✅ win |

---

## How to read these numbers (honesty)

This run is on a **resource-shared WSL2 dev box** (specs in the manifest above). Per the risk register: comparisons run on the *identical* box under identical conditions are a fair, real result (R6) — so the **recall, QPS, and latency standings above stand**. Two things a VM distorts (R5) are **not** to be read as official headlines:

- **Absolute RSS.** Only the *isolated* systems are comparable: Quiver, Qdrant, Weaviate, and Milvus **server** report the DB process/container RSS. FAISS, LanceDB, and Chroma run in-process, so their RSS includes the Python harness **and the resident dataset** (~512 MB for SIFT1M, ~3.7 GB for GIST1M) — inflated, not directly comparable. These are **in-memory HNSW** comparisons for every system; Quiver's memory-frugality wedge is its **disk-resident DiskVamana path** (holds only PQ codes in RAM), measured separately in [`docs/benchmarks/results/disk-path.md`](./disk-path.md) — not these tables.
- **Build time.** As of v0.20.0 Quiver's build uses the **bulk-ingest** path (`POST …/points:bulk`, ADR-0045): one WAL fsync per request and a single deferred index pass, with the first query forcing the rebuild so the reported number is the honest *time-until-queryable* (the same thing every competitor's build column measures). This replaces the v0.18.0 REST-upload path (1M points in 500-point POSTs, each doing incremental index maintenance) — compare the two `comparison-*` result sets for the improvement. In-process libraries (FAISS) still build fastest because they skip the network and serialization entirely.

The **SIFT1M and GIST1M comparative standings above are dev-box but real** (R6 — identical box, identical conditions). What stays pending on dedicated, otherwise-idle reference hardware (runbook [`§9`](../reference-hardware-runbook.md)): the **official absolute-RSS table**, **saturated multi-thread QPS** (QPS NT — these runs are single-thread), and **Deep10M** (the disk-path memory headline). Milvus is benchmarked as the **server** (Docker), not the in-process Lite build, which is not performance-representative.
