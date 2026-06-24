# Quiver benchmark harness

An `ann-benchmarks`-style harness that drives a running Quiver server through the
Python SDK and reports **recall@k**, **latency** (p50/p95/p99), and **single-thread
QPS** while sweeping `ef_search`. Methodology and the reporting template:
[`docs/benchmarks/methodology.md`](../docs/benchmarks/methodology.md).

> **Honesty first.** Official figures come **only** from the documented
> reference hardware in the methodology — this repo's dev box is resource-shared
> and is **not** a source of published numbers. We never fabricate results, and
> if Quiver loses on a metric we report it. The README's benchmark table is
> **reference-hardware-pending** until those runs are recorded.

## Quick smoke run (no dataset download)

Start a server, then run the synthetic smoke set (a small random dataset with
exact ground truth — it validates the harness, not performance):

```bash
QUIVER_INSECURE=true cargo run -p quiverdb-cli -- serve &      # dev only
uv run --project bench python -m quiver_bench.run --synthetic
```

## SIFT1M

Download SIFT1M (≈ 500 MB) into `bench/datasets/sift1m/` (git-ignored). The
standard distribution provides `sift_base.fvecs`, `sift_query.fvecs`, and
`sift_groundtruth.ivecs`:

```bash
mkdir -p bench/datasets && cd bench/datasets
curl -LO ftp://ftp.irisa.fr/local/texmex/corpus/sift.tar.gz   # pin + verify SHA-256
tar xf sift.tar.gz && mv sift sift1m
```

Then, against a server with a `bench` API key:

```bash
uv run --project bench python -m quiver_bench.run \
  --dataset bench/datasets/sift1m --api-key "$QUIVER_API_KEY" \
  --k 10 --ef 32,64,128,256 --out docs/benchmarks/results/sift1m.csv
```

Recall@10 is scored against the dataset's exact ground truth. Sweeping `ef`
traces the recall–QPS curve. RSS (the memory headline) is captured separately on
the reference host per the methodology.

## Larger datasets and deeper dimensions (ADR-0041)

The multi-DB comparison runner accepts larger datasets and a concurrent
(saturated-QPS) pass:

```bash
# GIST1M (1M x 960, L2) — downloaded + cached on first use (~2.6 GB)
uv run --project bench python -m quiver_bench.comparison \
  --dataset gist1m --competitors all --concurrency 16 \
  --out docs/benchmarks/results/comparison-v0.18.0

# Deep1M (96-d, L2) runs only if you place deep_base.fvecs / deep_query.fvecs
# (+ deep_groundtruth.ivecs) under bench/datasets/deep/ — it is never fabricated.
```

`--concurrency N` adds a saturated multi-thread QPS measurement (`qps_nt`)
alongside single-thread QPS at each operating point. Comparative numbers on the
identical box are publishable; absolute RSS and the 10M disk path stay
reference-hardware-pending (see the methodology).

## v0.22.0 Quiver-only sweeps (ADR-0061)

Two Quiver-focused sweeps publish dimensions the competitor matrix doesn't cover.
Both run against a server and write into the same per-dataset result dir:

```bash
# Memory wedge — same dataset under hnsw / disk_vamana+PQ, each in a FRESH
# server process (--start-server). recall@{1,10,100} + build + QPS tradeoff.
uv run --project bench python -m quiver_bench.quant_sweep \
  --dataset sift1m --out docs/benchmarks/results/comparison-v0.22.0 \
  --indexes hnsw,disk_vamana --start-server

# Filtered-selectivity sweep — recall (vs filtered exact truth) + QPS as a
# payload pre-filter keeps s% of the collection.
uv run --project bench python -m quiver_bench.filter_sweep \
  --dataset sift1m --out docs/benchmarks/results/comparison-v0.22.0 \
  --quiver-url http://127.0.0.1:7333 --quiver-key "$QUIVER_API_KEY"
```

`recall@100` comes from one extra **untimed** pass so it never perturbs QPS. The
wedge publishes the recall/build/throughput tradeoff; the absolute serving-RAM
figure stays reference-hardware-pending because post-build RSS is the build's
allocator high-water mark, not the cold-reload serving footprint (ADR-0061).

## Development

```bash
uv sync && uv run pytest      # metric + ground-truth unit tests (no server)
```
