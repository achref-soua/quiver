# Tuning Quiver for RAG

Every RAG workload sits somewhere on a **recall ↔ latency ↔ RAM ↔ cost** surface.
Quiver lets you pick that point per collection — index family, quantizer, and a
few query knobs. This guide is the practical map.

## Pick the index

| Index | RAM resident | Best for |
|---|---|---|
| `hnsw` (default) | graph **+ full vectors** | small/hot collections; highest recall, lowest latency |
| `ivf` (+PQ) | centroids (+ codes) | predictable RAM, fast build, a frugal fallback |
| `vamana` | PQ codes + node cache | medium collections, one machine |
| `disk_vamana` | **PQ codes only** | large collections (10M–100M+) on modest RAM — the memory-frugality wedge |
| `colbert` | coarse centroids + residual codes | token-level (late-interaction) retrieval |

Rule of thumb: start with `hnsw`; if the working set no longer fits comfortably
in RAM, move to **`disk_vamana`** — it serves high recall while holding only the
PQ codes resident (the full vectors live on the encrypted on-disk index). On
SIFTSMALL the disk path holds recall@10 up to 1.000 at a ~32× smaller resident
footprint than full-precision vectors; the arithmetic scales (e.g. a 10M × 768-d
collection ≈ 1 GB resident vs ~31 GB). See [indexing](../features/indexing.md)
and the [disk-path numbers](https://github.com/achref-soua/quiver/blob/main/docs/benchmarks/results/disk-path.md).

```python
q.create_collection("kb", dim=768, metric="cosine", index="disk_vamana", pq_subspaces=48)
```

## Pick the quantizer

`pq_subspaces` (product quantization) trades a little recall for a large RAM/disk
saving; scalar (4×) and binary (32×, a fast Hamming pre-filter then exact re-rank)
are also available. More subspaces → higher fidelity → more memory. Tune against
*your* embeddings; the [quantization tradeoff table](https://github.com/achref-soua/quiver/blob/main/docs/benchmarks/quantization-tradeoffs.md)
shows the shape.

## Tune the query

`ef_search` is the recall/latency dial. On **SIFT1M** (in-memory HNSW) Quiver's
own curve — second only to FAISS on throughput at this recall bar
([full comparison](https://github.com/achref-soua/quiver/blob/main/docs/benchmarks/results/comparison-v0.18.0/comparison-v0.18.0.md)):

| `ef_search` | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| recall@10 | 0.794 | 0.898 | 0.960 | 0.987 | 0.996 |
| QPS (1T) | 1150 | 1032 | 870 | 673 | 508 |
| p95 (ms) | 1.1 | 1.2 | 1.5 | 1.9 | 2.7 |

For RAG, recall@10 ≈ 0.95–0.99 (here `ef_search` 64–256) is the usual sweet spot:
the LLM tolerates a near-miss in the candidate set, and you save latency. Raise
`k` to give a reranker more to work with, then trim to the few chunks you ground
on.

## Operational guardrails

The server enforces **query cost limits** (ADR-0040) — caps on `k`, `ef_search`,
fetch `limit`, vector dimension, payload size, and batch size — so one oversized
request can't exhaust the node. The defaults are generous; raise a specific
`QUIVER_MAX_*` (see `.env.example`) if a legitimate workload needs more, rather
than removing the guardrail. Batched ingestion via `upsert_iter` stays within
`max_batch_size` automatically.

## Quick checklist

- Embeddings normalized? Use `metric="cosine"` for most sentence encoders.
- Working set bigger than RAM? `index="disk_vamana"` with `pq_subspaces`.
- Need scoping? Declare `filterable` fields and pre-filter every query.
- Latency-bound? Lower `ef_search`; recall-bound? Raise it (and add a reranker).
- High concurrency? Use the **async client** and batch upserts.
