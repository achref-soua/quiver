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

Hybrid search is reachable from every surface (ADR-0045):

- **REST:** `POST /v1/collections/{name}/query/hybrid` with
  `{ "vector": [...], "sparse_indices": [...], "sparse_values": [...], "k": 10,
  "filter": {...}, "rrf_k0": 60 }`.
- **gRPC:** the `HybridSearch` RPC (`HybridSearchRequest` with a dense `vector`, a
  `sparse` `SparseVector`, `filter`, `k`, `ef_search`, `rrf_k0`).
- **MCP:** the `hybrid_search` tool (`vector`, `sparse_indices`/`sparse_values`,
  `query_text`, `k`, `filter`, `rrf_k0`).
- **SDKs:** `hybrid_search` (Python) and `hybridSearch` (TypeScript).

## Full-text (BM25) — search by words (ADR-0046)

You don't have to build sparse vectors yourself. Give a point a `__quiver_text__`
string and Quiver tokenizes it (Unicode split, lowercase, stop-words, Snowball
stemming) into a term-frequency vector at ingest; query with `query_text` and Quiver
scores it with **Okapi BM25** over the same inverted index — fused with a dense
vector through the same RRF path for `dense ⊕ BM25` hybrid:

```python
client.upsert("docs", [Point(id="1", vector=embed(text), payload={"__quiver_text__": text})])
client.hybrid_search("docs", vector=embed(query), query_text=query, k=10)  # dense ⊕ BM25
```

An explicit `__quiver_sparse__` vector (e.g. SPLADE) still takes precedence over
`__quiver_text__`. BM25 uses the index's corpus statistics (document frequency,
average length), so it stays correct under incremental upsert/delete.

## Performance: the derived inverted index

The sparse side is served by an in-memory **inverted index** (`dim → {doc → weight}`,
ADR-0045): a query scores only the documents that share one of its nonzero terms,
rather than scanning every row. The index is **derived** — built from the store
when a collection's index is (re)built and maintained incrementally on
upsert/delete — so there is no on-disk format change and the `kill -9` crash gate is
untouched. A collection with no sparse vectors carries no index, and a not-yet-built
or client-side collection falls back to a correct full store scan.

## Limits and scope

- The sparse query's term count is bounded by `QUIVER_MAX_SPARSE_TERMS` (default
  4096), alongside the other [query cost limits](../security/threat-model.md).
- Tokenization uses the **Snowball (Porter2) English stemmer** (ADR-0048), so
  morphological variants conflate (`connection`/`connected`/`connecting` →
  `connect`); ingest and query share it, keeping the conflation consistent. Term ids
  are a 32-bit hash, so distinct tokens can in principle collide — negligible for
  realistic vocabularies.

See the [RAG guide](../guides/rag.md) for where hybrid retrieval fits in a
pipeline.
