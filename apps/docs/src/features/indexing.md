# Indexing & memory frugality

Memory frugality is Quiver's wedge: serve large datasets from a laptop's RAM budget
at a fixed recall. The lever is the **disk-resident graph index** plus
**quantization**. The full design — with cited papers — is in the
[architecture deep dive](../architecture/deep-dive.md); this page is the practical
overview.

## Choosing an index

Set the index per collection at creation time.

| Index | Where it lives | Best for |
|---|---|---|
| `hnsw` | RAM | the default; fast, high-recall in-memory search |
| `ivf` | RAM | clustered datasets; pairs with quantization |
| `vamana` | RAM | the DiskANN graph, in memory |
| `disk_vamana` | disk (encrypted) + PQ codes in RAM | **memory frugality** — large datasets, small RAM |
| `colbert` | RAM (derived) | [multi-vector](multi-vector.md) ColBERTv2/PLAID token pools |

## Quantization

Compress stored vectors to cut RAM at a small recall cost:

- **scalar** — per-dimension 8-bit; simple, modest savings.
- **product (PQ)** — subspace codebooks; the largest savings, tunable via
  `pq_subspaces`.
- **binary** — 1-bit with a Hamming pre-filter and an exact re-rank.

The per-collection **recall ↔ latency ↔ memory** knobs and their measured
trade-offs are tabulated in
[`docs/benchmarks/quantization-tradeoffs.md`](https://github.com/achref-soua/quiver/blob/main/docs/benchmarks/quantization-tradeoffs.md).

## The disk-resident path

`disk_vamana` keeps the graph and full-precision vectors in the encrypted on-disk
index and holds only compact PQ codes resident. On SIFTSMALL it serves recall@10 up
to **1.000** with a **32× smaller RAM-resident footprint** than full-precision
vectors — a reduction that is exact arithmetic and scales (e.g. a 10M × 768-d
collection: ~1 GB resident vs ~31 GB). The head-to-head **RSS vs Qdrant/LanceDB**
is reference-hardware-pending and never fabricated; method and numbers live in
[`docs/benchmarks/results/disk-path.md`](https://github.com/achref-soua/quiver/blob/main/docs/benchmarks/results/disk-path.md).

## Recall on SIFT1M

In-memory HNSW (`M=16`, `efC=200`), recall@10 vs exact ground truth — a property of
the index and data, so host-independent:

| `ef_search` | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| **recall@10** | 0.794 | 0.898 | 0.960 | 0.987 | 0.996 |

Reproduce with `cargo run --release --example sift_recall`.

## Incremental updates

Every index family applies inserts, updates, and deletes **incrementally**, so
streaming workloads avoid an `O(N)` rebuild per write:

- **IVF** — SpFresh-style LIRE rebalancing (cell split/merge).
- **HNSW** — `O(1)` soft-delete with an amortized rebuild.
- **Vamana / disk graph** — FreshDiskANN StreamingMerge (a read-only base graph
  plus an in-memory delta graph and an `O(1)` deletion set, consolidated past a
  churn threshold).

All indexes stay derived and the disk artifact keeps its write-once contract, so
the `kill -9` crash gate is untouched. See the
[ADRs](../architecture/adrs.md) for the per-family designs.
