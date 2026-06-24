# ADR-0061 — v0.22.0 benchmark dimensions (recall@k, saturated concurrency, memory wedge, filtered selectivity)

**Status:** Accepted
**Date:** 2026-06-23
**Deciders:** Achref Soua

---

## Context

ADR-0037 established the scientific multi-DB benchmark suite, and ADR-0041 proposed a set of
*deep* dimensions to close the coverage gaps the state-of-Quiver assessment recorded — concurrent
throughput, denser recall↔QPS curves, recall@{1,10,100}, filtered search, and a quantization
tradeoff curve. The v0.20.0 re-run (ADR-0055) refreshed the published SIFT1M + GIST1M standings on
the bulk-build path, but it ran single-thread, unfiltered, recall@10-only, in-memory HNSW for every
system. The deep dimensions stayed unbuilt.

Two things make them worth building now:

1. **v0.21.0 shipped concurrent reads** (ADR-0057: `RwLock` + `&self` snapshot reads). Its whole
   point is that readers no longer serialize — but the benchmark has no **saturated multi-thread
   QPS** column to show it. The concurrency dimension is the missing evidence for the headline
   feature.
2. **Memory frugality is Quiver's wedge**, and the comparison tables only ever showed in-memory
   HNSW. The recall↔RAM tradeoff of IVF+PQ and the disk-resident Vamana graph (PQ codes in RAM,
   vectors on SSD) is exactly the differentiator, and it was never measured side by side.

The honesty constraints are unchanged. This is a **resource-shared WSL2 dev box** (specs in each
run's `manifest.json`). Per the risk register: comparisons on the *identical* box under identical
conditions are a fair, publishable result (R6); two figures a VM distorts (R5) — the **official
absolute-RSS table** and the **Deep10M** disk-path headline — stay `[reference-hardware-pending]`,
never fabricated. The full-field saturated QPS across every competitor (nine adapters, several in
Docker, run concurrently) risks OOM on the shared box, so it too stays reference-hardware-pending;
the **Quiver-only** concurrency showcase is what runs here.

## Decision

Add the runnable deep dimensions to the harness as **Quiver-focused, unit-tested instrumentation**,
and publish what is honest to run now into `docs/benchmarks/results/comparison-v0.22.0/`. No
existing axis changes; these are additive.

### 1. recall@{1,10,100}

`BenchResult` gains `recall_at_1` and `recall_at_100` beside `recall_at_10`. The timed query loop
still runs at the report `k` (so QPS, latency, and recall@10 stay directly comparable to prior
runs); `recall@1` is derived for free from the same top-`k` retrieval, and `recall@100` is measured
in **one extra untimed pass** at `k=100` that never touches the throughput numbers. recall@1 is
precision@1 (the top hit), recall@100 is the wide-neighbour recall a k=10 query never retrieves.

### 2. Saturated multi-thread QPS (the concurrent-reads showcase)

The `--concurrency N` driver (`query_concurrent`, already in the harness) runs every query from an
`N`-thread pool and records `qps_nt` alongside single-thread `qps_1t`. The **qdrant adapter is
fixed** to hand each worker thread its own client — qdrant-client wraps a single non-thread-safe
HTTP session, so the prior shared client would have serialized or corrupted the saturated pass. The
published v0.22.0 showcase drives Quiver at `--concurrency 8` on SIFT1M; the full competitor field
stays reference-hardware-pending.

### 3. Quantization memory wedge (`quant_sweep.py`)

A Quiver-only sweep (`quant_sweep.py`) builds the *same* dataset under index/quantization configs,
**each in its own fresh server process**, and records recall@{1,10,100}, build time, and QPS:

- `hnsw` — exact vectors in RAM: the recall ceiling.
- `disk_vamana` + PQ — PQ codes in RAM, full vectors on SSD: holds recall@10 close to HNSW while PQ
  trades the *deep* tail (recall@100 falls off — measured, not asserted).
- `ivf` + PQ — in `default_configs` but **omitted from the published run**: its default parameters
  were mistuned on this box (slow build, poor recall), so a fair IVF point is reference-hardware-pending
  rather than published misleadingly. PQ uses `dim // 8` subspaces when that divides the dimension.

What the SIFT1M run showed — and the honest limit it forced: **post-build RSS does not capture the
serving-RAM wedge.** The disk-Vamana build pages every vector into RAM to construct the graph, and
the allocator keeps those pages, so RSS right after a build is the build's high-water mark, not the
cold-reload serving footprint where only PQ codes stay resident (fresh-server-per-config fixes
*cross-config* contamination but not this *within-config* build peak). Rather than publish an RSS
that shows the frugal index using the same RAM as HNSW, **RSS is omitted from the wedge table** and
the absolute serving-RAM figure stays `[reference-hardware-pending]` (it needs a build → restart →
cold-reload → measure-while-serving step the harness does not yet do). The recall/build/throughput
tradeoff *is* published — it is the runnable, honest part.

### 4. Filtered-selectivity sweep (`filter_sweep.py`)

A Quiver-only sweep gives each point a `bucket = id % 100` payload, so the filter `bucket < s`
matches exactly `s`% of the collection. For every selectivity it recomputes the **filtered** exact
ground truth by brute force over the matching subset (the only honest recall reference under a
filter) and reports recall@10, QPS, and latency percentiles as the filter tightens. This exercises
the selectivity planner (pre-filter-to-exact vs post-filter-ANN) and proves correctness under
filtering rather than just measuring throughput.

### Reporting

`report.py` renders the recall@{1,10,100} columns in the operating-point table and two new Quiver
sections — the memory wedge and the filtered-selectivity sweep — folded into each dataset. The
sweep CSVs (`quant_sweep.csv`, `filter_sweep.csv`) are excluded from the competitor matrix so they
never pollute the `quiver` ef/nprobe rows.

## Consequences

- The concurrent-reads work finally has a saturated-QPS column to stand on, and the memory wedge is
  measurable side by side instead of described in prose.
- The new instrumentation is pure/unit-tested (recall depths, selectivity mask, filtered brute-force
  truth, qdrant per-thread client, wedge config generation, report sections); the runs themselves
  are reproducible via `.scratch/run-bench-v022.sh <dataset> <out> <concurrency>`.
- What stays `[reference-hardware-pending]`, unchanged and never fabricated: the official
  absolute-RSS table, the full-field saturated QPS, and Deep10M. The shared-box numbers are labelled
  *dev-box · indicative*.

## Alternatives considered

- **Run the full nine-adapter field at saturated concurrency now.** Rejected: several competitors
  run in Docker; driving them all concurrently on the shared box is the OOM path the risk register
  warns against. The Quiver-only showcase is the honest subset; the field stays pending.
- **Bump the timed loop to k=100 to get recall@100 for free.** Rejected: it would change the QPS and
  latency numbers (retrieving 100 is slower than 10) and break comparability with prior runs. One
  untimed deep pass keeps the throughput figures clean.
- **Estimate the memory wedge from index sizes.** Rejected — measured RSS or nothing (PRIME RULE).
