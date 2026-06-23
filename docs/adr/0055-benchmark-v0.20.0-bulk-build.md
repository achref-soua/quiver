# ADR-0055 — v0.20.0 multi-DB benchmark re-run with the bulk-ingest build path

**Status:** Accepted
**Date:** 2026-06-23
**Deciders:** Achref Soua

---

## Context

ADR-0037 established the scientific multi-DB benchmark suite and ADR-0041 extended it with
deep dimensions; the published comparison was [`comparison-v0.18.0`](../benchmarks/results/comparison-v0.18.0/comparison-v0.18.0.md)
(SIFT1M + GIST1M, eight adapters). Two things have changed since:

1. **The engine moved to v0.20.0** — the v0.18.0 numbers were measured against a `v0.18.0-dev`
   server. The standings deserve a re-measurement on the released engine.
2. **The build-time column was the one column where Quiver looked bad for the wrong reason.**
   ADR-0045 shipped a bulk-ingest endpoint (`POST …/points:bulk`) that commits a large batch with
   one WAL fsync and a single deferred index pass; the v0.18.0 benchmark still used the old
   REST-upload path (1M points in 500-point POSTs, each doing incremental index maintenance), so it
   reported ~14 min for SIFT1M and ~51 min for GIST1M — a transport artifact, not engine speed. The
   v0.18.0 report footnoted this; v0.20.0 should *measure* the bulk path instead of footnoting it.

The honesty constraints are unchanged: a **resource-shared WSL2 box** (i7-12700H, 20 logical
cores, 15.5 GiB RAM). Per R6, comparisons on identical hardware are a fair, publishable result;
per R5, absolute headline RSS, saturated multi-thread QPS, and the 10M disk path are distorted by a
VM and stay `reference-hardware-pending`.

## Decision

Re-run the existing SIFT1M + GIST1M comparison against the **v0.20.0** server into a new
`docs/benchmarks/results/comparison-v0.20.0/` result set, with one methodology refinement and no
change to the published axes (recall@10 × QPS(1T) × p50/p95/p99 × build × steady-state RSS ×
on-disk size × `ef`/`nprobe` sweep × unfiltered).

### Build-column refinement (the only methodology change)

The Quiver adapter now ingests through `POST …/points:bulk`, batching only to stay under the
32 MiB request-body cap (batch size derived from the vector dimension), and **forces the deferred
index rebuild inside the build timer with one query**. This makes `build_s` the honest
*time-until-queryable* — the same quantity every competitor's build column already measures (they
all include index construction). Without the forced rebuild the bulk path would hide index-build
cost in the first query's latency, which would be dishonest. The batch-sizing helper
(`bulk_batch_size`) is unit-tested.

### What stays the same (and why)

- **Single-thread QPS only (concurrency = 1).** The saturated multi-thread pass needs per-adapter
  thread-safe clients (the `qdrant-client` cross-thread file-descriptor issue) and roughly N× the
  query load; on a shared VM it is exactly the metric R5 says not to publish as a headline. QPS(NT)
  stays `reference-hardware-pending`, as in v0.18.0.
- **recall@10, unfiltered, quantization off.** recall@{1,100}, a filtered-selectivity sweep, and a
  PQ/quantization comparison column each require multi-adapter harness work that competitors expose
  too differently to compare fairly in one round; ADR-0041 already deferred filtered/churn, and the
  recall@10 ↔ QPS Pareto + RAM remains the methodology's headline. These stay explicit follow-ups,
  not half-done columns.
- **GIST1M runs process-isolated** (one competitor per process) to avoid the in-process adapters'
  960-d build peak OOM-ing the box; a run that cannot complete (e.g. LanceDB's in-process IVF-PQ at
  960-d) is reported as a DNF, never estimated.

### Honesty rules (binding, unchanged from ADR-0041)

- Every result carries the auto-captured `manifest.json` machine spec. Comparative head-to-head
  numbers on the identical box are published as real; only absolute RSS, QPS(NT), and the 10M disk
  path are labelled `reference-hardware-pending`.
- Publish wins **and** losses; pin competitor versions and configs; never fabricate or extrapolate.

## Consequences

- A v0.20.0 picture of Quiver vs the field on the released engine, with a **real** build column
  measured through the bulk path — directly comparable to v0.18.0 to show the improvement.
- The Quiver adapter's build path changes; the report's version map and build honesty note are
  updated; the README and the "Quiver, Explained" evidence chapter carry the v0.20.0 numbers.
- No CI change: heavy runs stay manual; the smoke dataset remains the regression gate (ADR-0037).

## Alternatives considered

- **Keep footnoting the REST-upload build time.** Rejected: the bulk path exists and is the
  recommended ingest route; measuring it is more honest than explaining why the old number is
  unfair.
- **Add the saturated multi-thread and filtered columns now.** Rejected for this round for the
  reasons above (shared-VM distortion + fair-comparison cost); kept as named follow-ups.
- **Overwrite `comparison-v0.18.0`.** Rejected: keeping both result sets side by side is what lets a
  reader see the build-time improvement and the engine delta.
