# Quiver v0.18.0 — Multi-DB Benchmark Comparison

_Generated: 2026-06-19 10:30 UTC_

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

| Competitor | Version | recall@10 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |
|---|---|---|---|---|---|---|---|---|---|
| chroma | 1.5.9 | 0.9770 | 732 | — | 3496 | 202.1 | — | ef_search=16 | dev-box · indicative |
| faiss | 1.14.3 | 0.9677 | 2900 | — | 1234 | 110.3 | — | ef_search=64 | dev-box · indicative |
| lancedb | 0.33.0 | 0.5573 | 159 | — | 2255 | 19.0 | 508.5 | nprobes=64 | dev-box · indicative |
| milvus_server | v2.5.4 (server) | 0.9577 | 166 | — | 1601 | 31.1 | — | ef_search=16 | dev-box · indicative |
| qdrant | 1.13.4 | 0.9751 | 310 | — | 259 | 117.8 | — | ef_search=32 | dev-box · indicative |
| quiver | v0.18.0-dev | 0.9598 | 870 | — | 1617 | 854.3 | — | ef_search=64 | dev-box · indicative |
| weaviate | 1.27.0 | 0.9826 | 494 | — | 2091 | 2404.6 | — | ef_search=16 | dev-box · indicative |

### Full ef/nprobe sweep

<details><summary>chroma</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9770 | 732 | 1.24 | 2.26 | 2.95 | 3496 |
| 32 | 0.9770 | 733 | 1.25 | 2.15 | 2.88 | 3529 |
| 64 | 0.9770 | 732 | 1.25 | 2.17 | 2.88 | 3534 |
| 128 | 0.9770 | 743 | 1.24 | 2.12 | 2.76 | 3534 |
| 256 | 0.9770 | 723 | 1.25 | 2.23 | 2.99 | 3535 |

</details>

<details><summary>faiss</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.8130 | 7593 | 0.12 | 0.23 | 0.37 | 1188 |
| 32 | 0.9112 | 4932 | 0.19 | 0.33 | 0.49 | 1231 |
| 64 | 0.9677 | 2900 | 0.33 | 0.51 | 0.77 | 1234 |
| 128 | 0.9906 | 1535 | 0.62 | 0.97 | 1.53 | 1235 |
| 256 | 0.9976 | 821 | 1.17 | 1.86 | 2.62 | 1236 |

</details>

<details><summary>lancedb</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 4 | 0.5170 | 306 | 3.06 | 4.71 | 6.11 | 2695 |
| 8 | 0.5454 | 290 | 3.26 | 4.82 | 6.13 | 2742 |
| 16 | 0.5555 | 261 | 3.61 | 5.29 | 6.68 | 2254 |
| 32 | 0.5571 | 214 | 4.45 | 6.17 | 7.64 | 2255 |
| 64 | 0.5573 | 159 | 6.07 | 7.82 | 9.08 | 2255 |

</details>

<details><summary>milvus_server</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9577 | 166 | 5.89 | 17.56 | 23.88 | 1601 |
| 32 | 0.9631 | 442 | 1.86 | 6.74 | 8.52 | 1976 |
| 64 | 0.9867 | 522 | 1.79 | 2.76 | 3.59 | 1254 |
| 128 | 0.9969 | 460 | 2.05 | 3.12 | 3.84 | 1249 |
| 256 | 0.9989 | 372 | 2.54 | 3.85 | 4.63 | 1249 |

</details>

<details><summary>qdrant</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9295 | 372 | 2.07 | 5.42 | 7.80 | 513 |
| 32 | 0.9751 | 310 | 2.41 | 5.97 | 7.04 | 259 |
| 64 | 0.9933 | 337 | 2.54 | 5.72 | 6.95 | 259 |
| 128 | 0.9981 | 272 | 3.07 | 6.46 | 7.55 | 259 |
| 256 | 0.9990 | 243 | 3.83 | 6.60 | 7.84 | 258 |

</details>

<details><summary>quiver</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.7939 | 1150 | 0.83 | 1.11 | 1.60 | 1617 |
| 32 | 0.8976 | 1032 | 0.93 | 1.24 | 1.63 | 1617 |
| 64 | 0.9598 | 870 | 1.11 | 1.46 | 1.89 | 1617 |
| 128 | 0.9869 | 673 | 1.45 | 1.88 | 2.35 | 1617 |
| 256 | 0.9957 | 508 | 1.92 | 2.69 | 3.65 | 1617 |

</details>

<details><summary>weaviate</summary>

| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |
|---|---|---|---|---|---|---|
| 16 | 0.9826 | 494 | 1.95 | 2.65 | 3.64 | 2091 |
| 32 | 0.9826 | 504 | 1.92 | 2.58 | 3.48 | 2066 |
| 64 | 0.9826 | 502 | 1.92 | 2.67 | 3.54 | 2159 |
| 128 | 0.9826 | 504 | 1.92 | 2.60 | 3.48 | 2160 |
| 256 | 0.9826 | 506 | 1.92 | 2.61 | 3.53 | 2161 |

</details>

### Wins / ties / losses (Quiver vs field)

| Metric | vs competitor | Quiver | Competitor | Verdict |
|---|---|---|---|---|
| recall@10 | chroma | 0.9598 | 0.9770 | ≈ tie |
| recall@10 | faiss | 0.9598 | 0.9677 | ≈ tie |
| recall@10 | lancedb | 0.9598 | 0.5573 | ✅ win |
| recall@10 | milvus_server | 0.9598 | 0.9577 | ≈ tie |
| recall@10 | qdrant | 0.9598 | 0.9751 | ≈ tie |
| recall@10 | weaviate | 0.9598 | 0.9826 | ❌ loss |
| QPS (1T) | chroma | 870 | 732 | ✅ win |
| QPS (1T) | faiss | 870 | 2900 | ❌ loss |
| QPS (1T) | lancedb | 870 | 159 | ✅ win |
| QPS (1T) | milvus_server | 870 | 166 | ✅ win |
| QPS (1T) | qdrant | 870 | 310 | ✅ win |
| QPS (1T) | weaviate | 870 | 494 | ✅ win |
| RSS (MB) | chroma | 1617 | 3496 | ✅ win |
| RSS (MB) | faiss | 1617 | 1234 | ❌ loss |
| RSS (MB) | lancedb | 1617 | 2255 | ✅ win |
| RSS (MB) | milvus_server | 1617 | 1601 | ≈ tie |
| RSS (MB) | qdrant | 1617 | 259 | ❌ loss |
| RSS (MB) | weaviate | 1617 | 2091 | ✅ win |
| Build (s) | chroma | 854.3 | 202.1 | ❌ loss |
| Build (s) | faiss | 854.3 | 110.3 | ❌ loss |
| Build (s) | lancedb | 854.3 | 19.0 | ❌ loss |
| Build (s) | milvus_server | 854.3 | 31.1 | ❌ loss |
| Build (s) | qdrant | 854.3 | 117.8 | ❌ loss |
| Build (s) | weaviate | 854.3 | 2404.6 | ✅ win |

---

## How to read these numbers (honesty)

This run is on a **resource-shared WSL2 dev box** (specs in the manifest above). Per the risk register: comparisons run on the *identical* box under identical conditions are a fair, real result (R6) — so the **recall, QPS, and latency standings above stand**. Two things a VM distorts (R5) are **not** to be read as official headlines:

- **Absolute RSS.** Only the *isolated* systems are comparable: Quiver, Qdrant, Weaviate, and Milvus **server** report the DB process/container RSS. FAISS, LanceDB, and Chroma run in-process, so their RSS includes the Python harness **and the resident 512 MB dataset** — inflated, not directly comparable. This SIFT1M table is an **in-memory HNSW** comparison for every system; Quiver's memory-frugality wedge is its **disk-resident DiskVamana path** (holds only PQ codes in RAM), measured separately in [`docs/benchmarks/results/disk-path.md`](./disk-path.md) — not this table.
- **Build time.** Quiver's build is the **REST-upload** path (1M points in batched POSTs); competitors using in-process or bulk insert are faster. A bulk-ingest endpoint is a known follow-up; it does not reflect engine speed.

Pending on dedicated, otherwise-idle reference hardware (runbook [`§9`](../reference-hardware-runbook.md)): **GIST1M** (960-d), **Deep10M** (the disk-path memory headline), and the official absolute-RSS table. Milvus is benchmarked as the **server** (Docker), not the in-process Lite build, which is not performance-representative.
