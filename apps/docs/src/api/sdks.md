# Python, TypeScript & Go SDKs

Both SDKs are thin, idiomatic clients over the [REST API](rest-grpc.md). They are
unpublished today — install from the repository — and a publish to PyPI/npm is a
launch-time task.

## Python

Install from PyPI as [`quiver-client`](https://pypi.org/project/quiver-client/)
(`pip install quiver-client`; or `pip install ./sdks/python` from a checkout):

```python
from quiver import Client, Point

with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("items", dim=3, metric="cosine")
    q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
    hits = q.search("items", [0.1, 0.2, 0.3], k=5)
```

Beyond `search`, the client exposes `hybrid_search` (dense ⊕ sparse/BM25 via
`vector` / `sparse` / `query_text`, fused with RRF) and — when the server has a
[provider configured](../features/embedding.md) — `upsert_text` / `search_text`
(`search_text(..., rerank=True)` for retrieve→rerank in one call):

```python
q.upsert_text("kb", [{"id": "1", "text": "Quiver is a vector database"}])
hits = q.search_text("kb", "what is quiver?", k=5, rerank=True)
q.hybrid_search("kb", vector=embed(query), query_text=query, k=10)   # dense ⊕ BM25
```

**LangChain**, **LlamaIndex**, and **Haystack** adapters ship as extras
(`pip install "quiver-client[langchain]"` / `[llamaindex]` / `[haystack]`), so any
Quiver index — including the memory-frugal disk path — backs a retriever or
`DocumentStore`, with metadata filters mapped onto Quiver's exact pre-filter. Pass
`hybrid=True` to any of them for `dense ⊕ BM25` retrieval.

A synchronous `Client` and an async `AsyncClient` share one contract (with
`upsert_iter` / `scroll` / `delete_by_filter` and `upsert_text` / `search_text`
helpers), and `quiver.rerank` is a model-agnostic client-side helper for the
retrieve → rerank step of a RAG pipeline.

## TypeScript

Install from npm as [`quiver-client`](https://www.npmjs.com/package/quiver-client)
(`npm install quiver-client`; or `pnpm add ./sdks/typescript` from a checkout),
dependency-free over the global `fetch`:

```ts
import { Client } from "quiver-client";

const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });
await q.createCollection("items", 3, { metric: "cosine", index: "disk_vamana", pqSubspaces: 1 });
await q.upsert("items", [{ id: "a", vector: [0.1, 0.2, 0.3], payload: { tag: "x" } }]);
const hits = await q.search("items", [0.1, 0.2, 0.3], { k: 5 });
```

The TypeScript client is fully `Promise`-based and mirrors the same surface as
the Python async client: `hybridSearch` (dense ⊕ sparse/BM25); with a
[server-side provider](../features/embedding.md), `upsertText` / `searchText`
(`{ rerank: true }` to reorder in one call); and the bulk/maintenance helpers
`upsertIter` (batches a sync **or async** iterable), `scroll` (an async generator
over a collection, for export / re-embedding), and `deleteByFilter` (paged
erasure, for GDPR / re-indexing).

```ts
for await (const point of q.scroll("items", { batch: 500 })) {
  // export or re-embed each point
}
await q.upsertIter("items", asyncSource, { batch: 500, onProgress: (n) => console.log(n) });
await q.deleteByFilter("items", { eq: { field: "tag", value: "stale" } });
```

## Go

Install from [`sdks/go`](https://github.com/achref-soua/quiver/tree/main/sdks/go)
(`github.com/achref-soua/quiver/sdks/go`), **standard-library only**:

```go
import quiver "github.com/achref-soua/quiver/sdks/go"

c := quiver.New("http://127.0.0.1:8080", quiver.WithAPIKey("…"))
c.CreateCollection(ctx, "items", 3, &quiver.CreateCollectionOptions{Metric: "cosine"})
c.Upsert(ctx, "items", []quiver.Point{{ID: "a", Vector: []float32{0.1, 0.2, 0.3}}})
hits, _ := c.HybridSearch(ctx, "items", &quiver.HybridOptions{QueryText: "hello"})
```

The Go client mirrors the same surface — `Search`, `HybridSearch`, `UpsertText` /
`SearchText`, `Fetch`, and `Snapshot`, plus the bulk/maintenance helpers
`UpsertBatch` (batched upload), `Scroll` (page through a collection via a
callback), and `DeleteByFilter` (paged erasure). Every call takes a
`context.Context`; non-2xx responses return a typed `*quiver.APIError`.

## Snapshots

All three clients expose `snapshot(destination)` — a consistent online backup of
the whole database (admin-scoped). See [Snapshots & backup](../features/snapshot.md).

## Client-side encryption helpers

The SDKs carry the client-side ciphers as **optional subpath modules**, so the core
client stays dependency-free; install the audited crypto peer dependency only to use
them. Each has a Rust reference and a cross-language known-answer test.

| Helper | Purpose | Python | TypeScript |
|---|---|---|---|
| `PayloadCipher` | seal payload fields ([ADR-0012](https://github.com/achref-soua/quiver/blob/main/docs/adr/0012-client-side-encryption.md)) | `quiver.encryption` | `quiver-client/encryption` |
| `VectorCipher` | [opaque vectors](../security/client-side-vectors.md) (IND-CPA) | `quiver.vector` | `quiver-client/vector` |
| `DcpeCipher` | [DCPE](../security/dcpe.md) encrypted search (experimental) | `quiver.dcpe` | `quiver-client/dcpe` |

DCPE example (encrypt vectors before upsert, queries before search, with the same
cipher):

```python
from quiver import Client
from quiver.dcpe import DcpeCipher          # pip install quiver-client[dcpe]

cipher = DcpeCipher.from_hex("…64 hex chars…", approximation_factor=0.02)
with Client("https://…", api_key="…") as q:
    q.create_collection("vault", dim=8, metric="l2", vector_encryption="dcpe")
    sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8])
    q.upsert("vault", [{"id": "a", "vector": sealed.ciphertext}])
    hits = q.search("vault", cipher.encrypt_query(my_query), k=10)
```

```ts
import { DcpeCipher } from "quiver-client/dcpe"; // pnpm add @stablelib/{chacha,hkdf,hmac,sha256}

const cipher = DcpeCipher.fromHex("…64 hex chars…", 0.02);
const sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
// upsert sealed.ciphertext; search with cipher.encryptQuery(myQuery).
```
