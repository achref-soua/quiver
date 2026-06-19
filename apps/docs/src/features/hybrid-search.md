# Hybrid (dense + sparse) search

Hybrid search combines a **dense** embedding (semantic similarity) with a
**sparse** vector (learned-sparse like SPLADE/BGE-M3, or lexical term weights) and
fuses the two rankings — the combination that beats dense-only retrieval on rare
terms, exact matches, and out-of-domain queries. Quiver fuses them with
**Reciprocal Rank Fusion (RRF)** (ADR-0043).

## How it works

- A point carries a sparse vector in its payload under the reserved key
  `__quiver_sparse__` — parallel `indices` (dimension ids) and `values` (weights).
  It rides the existing encrypted store, so there is **no on-disk format change**.
- `hybrid_search` runs the dense ANN ranking and a sparse dot-product ranking
  independently, re-checks the **same payload filter** on both (results stay
  exact), and fuses by RRF: a document at rank *r* in a list contributes
  `1 / (k0 + r + 1)`, summed across lists. RRF is rank-based, so the incomparable
  dense-distance and sparse-score scales need no normalisation.
- Either query may be omitted — pass only a dense vector for pure dense search, or
  only a sparse vector for pure lexical/sparse search, through the same call.

## Store a sparse vector

Put `__quiver_sparse__` in the point payload (the dense vector is upserted as
usual):

```python
from quiver import Client, Point

q = Client(api_key="…")
q.create_collection("kb", dim=384, metric="cosine")
q.upsert("kb", [Point(
    id="doc1",
    vector=dense_embedding,                       # your model
    payload={
        "text": "…",
        "__quiver_sparse__": {"indices": [4, 17, 2090], "values": [0.7, 1.2, 0.3]},
    },
)])
```

## Query

```python
from quiver import SparseVector

hits = q.hybrid_search(
    "kb",
    vector=dense_query,                            # omit for pure-sparse
    sparse=SparseVector(indices=[4, 2090], values=[0.9, 0.4]),  # omit for pure-dense
    k=10,
    filter={"eq": {"field": "lang", "value": "en"}},
    rrf_k0=60.0,
)
```

Over REST: `POST /v1/collections/{name}/query/hybrid` with
`{ "vector": [...], "sparse_indices": [...], "sparse_values": [...], "k": 10,
"filter": {...}, "rrf_k0": 60 }`.

## Limits and scope

- The sparse query's term count is bounded by `QUIVER_MAX_SPARSE_TERMS` (default
  4096), alongside the other [query cost limits](../security/threat-model.md).
- The sparse side currently scans the live store, which keeps results correct
  under incremental upsert/delete; a derived inverted index, gRPC/MCP/TypeScript
  parity, and a built-in BM25 tokenizer (which produces sparse vectors, reusing
  this machinery) are tracked follow-ups (ADR-0043).

See the [RAG guide](../guides/rag.md) for where hybrid retrieval fits in a
pipeline.
