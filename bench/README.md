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
QUIVER_INSECURE=true cargo run -p quiver-cli -- serve &      # dev only
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

## Development

```bash
uv sync && uv run pytest      # metric + ground-truth unit tests (no server)
```
