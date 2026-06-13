# SIFT1M — recall@10

Standard **SIFT1M** (1,000,000 base × 128-d, 10,000 queries, L2), recall@10 against
the dataset's exact ground truth. Index: in-memory **HNSW**, `M = 16`, `efC = 200`
(`HnswConfig::default()`), sweeping `ef_search`.

| `ef_search` | recall@10 | QPS (1T) |
|---|---|---|
| 16  | 0.7939 | 9051 |
| 32  | 0.8976 | 5771 |
| 64  | 0.9598 | 3383 |
| 128 | 0.9869 | 1802 |
| 256 | 0.9957 |  969 |

Index build: 712 s for 1,000,000 vectors.

## What is and isn't official here

- **recall@10 is valid.** Recall is a property of the index and the data — it does
  not depend on the host, the build profile, or system load — so these figures
  stand regardless of where they were measured.
- **QPS is indicative only.** It was measured single-threaded, against the
  in-memory index directly (not the full server path), on a resource-shared WSL2
  dev box. Per [`../methodology.md`](../methodology.md), official throughput,
  **memory (RSS — the headline)**, and the head-to-head vs Qdrant/LanceDB require
  identical dedicated reference hardware and are **pending**. We never fabricate
  results.

## Reproduce

```bash
# fetch SIFT1M into bench/datasets/sift1m/ (git-ignored), then:
cargo run --release --example sift_recall -- \
  bench/datasets/sift1m/sift_base.fvecs \
  bench/datasets/sift1m/sift_query.fvecs \
  bench/datasets/sift1m/sift_groundtruth.ivecs
```

The end-to-end `ann-benchmarks`-style harness (server + SDK) lives in
[`../../../bench`](../../../bench); it additionally reports p50/p95/p99 latency and
is the path used for the reference-hardware throughput/memory runs.

> Measured on: WSL2 (Linux 6.6, x86-64), `rustc` per `rust-toolchain.toml`, release
> profile, single thread, shared box — documented here for honesty; not the source
> of official throughput/memory figures.
