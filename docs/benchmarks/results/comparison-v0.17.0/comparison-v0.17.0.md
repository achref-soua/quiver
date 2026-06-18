# Quiver v0.17.0 — Multi-DB Benchmark Comparison

_Generated: 2026-06-18 17:56 UTC_

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

| Competitor | Version | recall@10 | QPS (1T) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|
| faiss | 1.14.3 | 0.9684 | 2333 | 1228 | 118.4 | — | ef_search=64 | [reference-hardware-pending] |
| lancedb | 0.33.0 | 0.5565 | 146 | 2264 | 21.0 | 508.5 | nprobes=64 | [reference-hardware-pending] |

### Full ef/nprobe sweep

<details><summary>faiss</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.8113 | 7685 | 0.12 | 0.22 | 0.34 | 1184 |
| 32 | 0.9111 | 4492 | 0.21 | 0.37 | 0.55 | 1226 |
| 64 | 0.9684 | 2333 | 0.40 | 0.70 | 1.00 | 1228 |
| 128 | 0.9911 | 1172 | 0.75 | 1.73 | 3.02 | 1230 |
| 256 | 0.9977 | 779 | 1.25 | 1.88 | 2.51 | 1232 |

</details>

<details><summary>lancedb</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.5169 | 276 | 3.43 | 5.19 | 6.45 | 2708 |
| 8 | 0.5448 | 241 | 3.77 | 6.86 | 11.59 | 2749 |
| 16 | 0.5545 | 248 | 3.86 | 5.32 | 6.66 | 2261 |
| 32 | 0.5564 | 194 | 4.89 | 7.16 | 9.04 | 2264 |
| 64 | 0.5565 | 146 | 6.60 | 8.93 | 11.51 | 2264 |

</details>

### Wins / ties / losses (Quiver vs field)

_Quiver results not available — matrix requires a Quiver run._

## Dataset: SIFTSMALL (10k, 128-d, L2) — smoke validation

> **Purpose:** Validates every competitor adapter end-to-end on 10k vectors. QPS and RSS on 10k vectors are not representative of production scale.

### Operating point: recall@10 ≥ 0.95 (or best achieved)

| Competitor | Version | recall@10 | QPS (1T) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|
| chroma | 1.5.9 | 0.9980 | 904 | 381 | 1.2 | — | ef_search=16 | smoke only |
| faiss | 1.14.3 | 0.9660 | 38609 | 65 | 0.2 | — | ef_search=16 | smoke only |
| lancedb | 0.33.0 | 0.7590 | 278 | 314 | 4.4 | 5.2 | nprobes=32 | smoke only |
| milvus_lite | 3.0.0 | 0.9670 | 183 | 471 | 0.8 | 0.0 | ef_search=16 | smoke only |
| pgvector | 0.7/pg16 | 0.9650 | 1156 | 66 | 1.9 | 11.2 | nprobe=8 | smoke only |
| qdrant | 1.13.4 | 1.0000 | 482 | 218 | 0.9 | — | ef_search=16 | smoke only |
| quiver | v0.17.0-dev | 0.9680 | 1233 | 61 | 65.4 | — | ef_search=16 | smoke only |
| weaviate | 1.27.0 | 0.9980 | 567 | 164 | 24.1 | — | ef_search=16 | smoke only |

### Full ef/nprobe sweep

<details><summary>chroma</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9980 | 904 | 0.98 | 1.98 | 2.63 | 381 |
| 32 | 0.9980 | 962 | 0.94 | 1.57 | 2.47 | 381 |
| 64 | 0.9980 | 932 | 0.95 | 1.79 | 2.59 | 381 |
| 128 | 0.9980 | 860 | 1.02 | 2.05 | 3.12 | 381 |
| 256 | 0.9980 | 943 | 0.95 | 1.68 | 2.27 | 381 |

</details>

<details><summary>faiss</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9660 | 38609 | 0.02 | 0.04 | 0.09 | 65 |
| 32 | 0.9920 | 35427 | 0.03 | 0.04 | 0.07 | 66 |
| 64 | 0.9990 | 22377 | 0.04 | 0.06 | 0.09 | 66 |
| 128 | 1.0000 | 13427 | 0.07 | 0.10 | 0.12 | 66 |
| 256 | 1.0000 | 6432 | 0.14 | 0.21 | 0.47 | 66 |

</details>

<details><summary>lancedb</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.7060 | 354 | 2.67 | 4.21 | 4.92 | 313 |
| 8 | 0.7500 | 337 | 2.83 | 4.31 | 4.70 | 314 |
| 16 | 0.7580 | 332 | 2.94 | 3.78 | 4.43 | 314 |
| 32 | 0.7590 | 278 | 3.29 | 4.79 | 6.13 | 314 |
| 64 | 0.7590 | 234 | 4.08 | 5.40 | 6.09 | 314 |

</details>

<details><summary>milvus_lite</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9670 | 183 | 5.31 | 6.65 | 8.25 | 471 |
| 32 | 0.9670 | 184 | 5.35 | 6.06 | 6.54 | 437 |
| 64 | 0.9670 | 182 | 5.38 | 6.32 | 7.55 | 437 |
| 128 | 0.9670 | 183 | 5.37 | 6.24 | 7.48 | 414 |
| 256 | 0.9670 | 179 | 5.42 | 6.76 | 7.94 | 414 |

</details>

<details><summary>pgvector</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.8750 | 1261 | 0.74 | 1.11 | 1.75 | 66 |
| 8 | 0.9650 | 1156 | 0.83 | 1.18 | 1.35 | 66 |
| 16 | 0.9970 | 868 | 1.08 | 1.59 | 1.85 | 66 |
| 32 | 1.0000 | 615 | 1.57 | 2.30 | 2.64 | 66 |
| 64 | 1.0000 | 329 | 2.90 | 4.21 | 5.51 | 66 |

</details>

<details><summary>qdrant</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 1.0000 | 482 | 1.96 | 3.07 | 3.65 | 218 |
| 32 | 1.0000 | 472 | 1.95 | 3.44 | 3.78 | 213 |
| 64 | 1.0000 | 331 | 2.98 | 4.59 | 5.57 | 201 |
| 128 | 1.0000 | 482 | 1.94 | 3.21 | 4.38 | 187 |
| 256 | 1.0000 | 493 | 1.97 | 2.69 | 3.04 | 136 |

</details>

<details><summary>quiver</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9680 | 1233 | 0.74 | 1.00 | 1.31 | 61 |
| 32 | 0.9930 | 1225 | 0.78 | 1.15 | 1.28 | 60 |
| 64 | 0.9970 | 1156 | 0.84 | 1.11 | 1.33 | 60 |
| 128 | 1.0000 | 974 | 0.98 | 1.38 | 1.74 | 60 |
| 256 | 1.0000 | 769 | 1.23 | 1.71 | 2.46 | 60 |

</details>

<details><summary>weaviate</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9980 | 567 | 1.69 | 2.41 | 2.92 | 164 |
| 32 | 0.9980 | 587 | 1.61 | 2.46 | 2.85 | 165 |
| 64 | 0.9980 | 576 | 1.68 | 2.32 | 2.77 | 165 |
| 128 | 0.9980 | 564 | 1.69 | 2.45 | 2.68 | 165 |
| 256 | 0.9980 | 553 | 1.68 | 2.73 | 3.03 | 174 |

</details>

### Wins / ties / losses (Quiver vs field)

| Metric | vs competitor | Quiver | Competitor | Verdict |
|---|---|---|---|---|
| recall@10 | chroma | 0.9680 | 0.9980 | ❌ loss |
| recall@10 | faiss | 0.9680 | 0.9660 | ≈ tie |
| recall@10 | lancedb | 0.9680 | 0.7590 | ✅ win |
| recall@10 | milvus_lite | 0.9680 | 0.9670 | ≈ tie |
| recall@10 | pgvector | 0.9680 | 0.9650 | ≈ tie |
| recall@10 | qdrant | 0.9680 | 1.0000 | ❌ loss |
| recall@10 | weaviate | 0.9680 | 0.9980 | ❌ loss |
| QPS (1T) | chroma | 1233 | 904 | ✅ win |
| QPS (1T) | faiss | 1233 | 38609 | ❌ loss |
| QPS (1T) | lancedb | 1233 | 278 | ✅ win |
| QPS (1T) | milvus_lite | 1233 | 183 | ✅ win |
| QPS (1T) | pgvector | 1233 | 1156 | ✅ win |
| QPS (1T) | qdrant | 1233 | 482 | ✅ win |
| QPS (1T) | weaviate | 1233 | 567 | ✅ win |
| RSS (MB) | chroma | 61 | 381 | ✅ win |
| RSS (MB) | faiss | 61 | 65 | ✅ win |
| RSS (MB) | lancedb | 61 | 314 | ✅ win |
| RSS (MB) | milvus_lite | 61 | 471 | ✅ win |
| RSS (MB) | pgvector | 61 | 66 | ✅ win |
| RSS (MB) | qdrant | 61 | 218 | ✅ win |
| RSS (MB) | weaviate | 61 | 164 | ✅ win |
| Build (s) | chroma | 65.4 | 1.2 | ❌ loss |
| Build (s) | faiss | 65.4 | 0.2 | ❌ loss |
| Build (s) | lancedb | 65.4 | 4.4 | ❌ loss |
| Build (s) | milvus_lite | 65.4 | 0.8 | ❌ loss |
| Build (s) | pgvector | 65.4 | 1.9 | ❌ loss |
| Build (s) | qdrant | 65.4 | 0.9 | ❌ loss |
| Build (s) | weaviate | 65.4 | 24.1 | ❌ loss |

---

## Reference-hardware-pending figures

The following results require reproduction on dedicated, otherwise-idle hardware (see [`docs/benchmarks/reference-hardware-runbook.md`](../reference-hardware-runbook.md), §9 for the full multi-DB procedure):

- **Quiver SIFT1M** — uploading 1M vectors through the REST API takes ~12 minutes on this shared dev box; the comparison harness therefore skipped the Quiver SIFT1M run. Real measured numbers (recall@10 vs QPS at `ef_search` 16–256, HNSW, L2) are in [`docs/benchmarks/results/sift1m.md`](./sift1m.md) (single-DB harness, same methodology). A full cross-competitor Quiver run at SIFT1M requires the reference hardware setup.
- **SIFT1M Docker competitors** (Qdrant, pgvector, Weaviate) — Docker API overhead at 1M-vector scale requires a dedicated machine and is not run here.
- **GloVe-100** (cosine metric, ~1.2M vectors).
- **Deep10M** (disk-path, 10M vectors — the memory-frugality headline).
