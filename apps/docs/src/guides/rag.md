# RAG with Quiver

Quiver is a drop-in retrieval backend for Retrieval-Augmented Generation. It is
**model-agnostic** — you bring the embeddings (OpenAI, Cohere, a local
sentence-transformer, anything), Quiver stores them, filters on metadata, and
returns nearest neighbours fast. This guide walks the full loop: **chunk → embed
→ upsert → filtered search → rerank → answer**.

A runnable, dependency-light version of everything here is in
[`examples/rag/quickstart.py`](https://github.com/achref-soua/quiver/blob/main/examples/rag/quickstart.py)
(it uses a deterministic hash embedder so it runs with no API key; swap in a real
model for production).

## 1. Create a collection with filterable metadata

Pick the metric your embedding model was trained for (`cosine` for most
sentence encoders, `l2` or `dot` otherwise), and declare the payload fields you
will filter on — the metadata pre-filter is **exact**, so retrieval can be
scoped to a tenant, a document set, a date range, etc.

```python
from quiver import Client, FilterableField, Point

q = Client("http://127.0.0.1:6333", api_key="…")
q.create_collection(
    "kb",
    dim=384,                      # must match your embedder
    metric="cosine",
    filterable=[
        FilterableField("source", "keyword"),
        FilterableField("year", "numeric"),
    ],
)
```

## 2. Chunk and embed

Split long documents into overlapping windows (so a relevant passage is never
cut across a boundary), embed each chunk, and keep the original text in the
payload so you can feed it to the LLM later.

```python
def chunk(text, size=800, overlap=120):
    out, start = [], 0
    while start < len(text):
        out.append(text[start:start + size]); start += size - overlap
    return out

points = []
for doc in documents:
    for j, piece in enumerate(chunk(doc.text)):
        points.append(Point(
            id=f"{doc.id}-{j}",
            vector=embed(piece),                       # your model
            payload={"text": piece, "source": doc.source, "year": doc.year},
        ))
```

## 3. Upsert (batched, with progress)

`upsert_iter` chunks a large corpus into server-friendly batches (within the
configured `max_batch_size`) and reports progress — ideal for loading millions
of chunks.

```python
q.upsert_iter("kb", points, batch=500, on_progress=lambda n: print(f"upserted {n}"))
```

For a high-throughput ingestion service, use the **async client** so embedding
and upload overlap:

```python
from quiver import AsyncClient

async with AsyncClient(api_key="…") as q:
    await q.upsert_iter("kb", points, batch=500)
```

## 4. Retrieve (with a metadata filter)

Embed the question and search, scoping with a filter when you can — pre-filtering
both improves answer quality and reduces the candidate set:

```python
hits = q.search(
    "kb",
    embed(question),
    k=8,
    filter={"and": [
        {"eq": {"field": "source", "value": "handbook"}},
        {"gte": {"field": "year", "value": 2024}},
    ]},
)
context = "\n\n".join(h.payload["text"] for h in hits)
```

## 5. Rerank (optional) and answer

`search` already returns exact-reranked nearest neighbours. For higher precision,
over-fetch and re-score the top-k with a cross-encoder before trimming to the few
chunks you feed the LLM. The `quiver.rerank` helper handles the extract → score →
sort → truncate step (you bring the scorer):

```python
# pip install sentence-transformers
from sentence_transformers import CrossEncoder
from quiver import rerank

ce = CrossEncoder("cross-encoder/ms-marco-MiniLM-L-6-v2")
hits = q.search("kb", embed(question), k=50)          # over-fetch
top = rerank(question, hits, lambda query, texts: ce.predict([(query, t) for t in texts]),
             key="text", top_k=4)                      # best-first RerankResults
context = "\n\n".join(r.match.payload["text"] for r in top)
```

Then hand the assembled context plus the question to your LLM as grounding. For
paragraph/token-level retrieval (ColBERT-style late interaction), see
[multi-vector](../features/multi-vector.md).

## Hybrid retrieval

For queries with rare terms, exact matches, or out-of-domain phrasing, fuse the
dense embedding with a **sparse** signal (SPLADE/BGE-M3 or lexical weights) via
`hybrid_search` — see **[Hybrid search](../features/hybrid-search.md)**. Store the
sparse vector under `__quiver_sparse__` and pass both to the query; Quiver fuses
the two rankings with Reciprocal Rank Fusion.

## Where to go next

- **[Tuning for RAG](tuning.md)** — choosing the index and quantizer for your
  recall ↔ latency ↔ RAM budget (including the memory-frugal disk path).
- **[Agentic patterns](agentic.md)** — let an LLM agent drive Quiver over MCP.
- **LangChain / LlamaIndex / Haystack** — Quiver ships vector-store adapters; see
  [SDKs](../api/sdks.md).
