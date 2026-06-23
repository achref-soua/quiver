# Server-side embedding & reranking

Quiver's engine is **model-agnostic** — by default you bring the vectors. But the
single biggest friction in RAG is *"I have to embed the text myself."* So the
**server** offers an opt-in, provider-agnostic embedding (and reranking) step:
send text, and Quiver embeds it for you, stores it, and searches it — while the
engine stays model-free (the adapter lives only at the server edge, never in the
embeddable library). See ADR-0047.

> **Opt-in, off by default.** A collection with no configured provider behaves
> exactly as before (you supply vectors). Library mode is unaffected.

## Configure a provider (server config, secrets by reference)

Providers are configured **per collection** in `quiver.toml` — *not* in the
on-disk collection schema, so there is no format change and the data directory
never holds a secret. An API key is referenced by the **name** of an environment
variable and resolved at startup; the value is never persisted.

```toml
# quiver.toml
[embedding.docs]                    # collection "docs"
provider    = "openai"              # openai | ollama | http | cohere | fake
model       = "text-embedding-3-small"
dim         = 1536                  # MUST equal the collection's vector dim
api_key_env = "OPENAI_API_KEY"      # name of the env var holding the key

[rerank.docs]                       # optional, enables search_text(rerank=true)
provider    = "cohere"
model       = "rerank-v3.5"
api_key_env = "COHERE_API_KEY"
```

| `provider` | Endpoint | Notes |
|---|---|---|
| `openai` | `https://api.openai.com/v1/embeddings` | Bearer `api_key_env`. |
| `ollama` | your `endpoint` (e.g. `http://localhost:11434/v1/embeddings`) | OpenAI-compatible; usually no key. |
| `http` | your `endpoint` | Any OpenAI-compatible server (vLLM, LM Studio, llama.cpp, …). |
| `cohere` | `https://api.cohere.com/v2/embed` · `/v2/rerank` | Bearer `api_key_env` (required). |
| `fake` | — | Deterministic hash embedder for tests/CI; never a real model. |

The `openai` / `ollama` / `http` providers share one OpenAI-compatible adapter, so
"never hard-code a vendor" is satisfied by **configuration**, not a vendor matrix.
A missing required `api_key_env` is a hard error at startup, surfacing the
misconfiguration immediately. A provider call that fails returns **HTTP 502 /
gRPC `Unavailable`** with a secret-free message; it never corrupts state.

## `upsert_text` — store text, Quiver embeds it

One call embeds the text for dense search **and** indexes it under
`__quiver_text__` for BM25 ([full-text](hybrid-search.md)) — so a corpus ingested
this way is immediately searchable both semantically and lexically.

```bash
curl -X POST localhost:6333/v1/collections/docs/points:text \
  -H 'authorization: Bearer …' -H 'content-type: application/json' \
  -d '{"points":[{"id":"1","text":"Quiver is a memory-frugal vector database","payload":{"src":"readme"}}]}'
```

```python
q.upsert_text("docs", [{"id": "1", "text": "Quiver is a vector database", "payload": {"src": "readme"}}])
```

## `search_text` — query by text, optionally rerank in one call

The server embeds the query, runs dense (⊕ BM25 when the collection has text)
retrieval, and — with `rerank=true` and a `[rerank.<collection>]` provider —
over-fetches a candidate pool and reorders it to the top `k`, all in one round
trip.

```bash
curl -X POST localhost:6333/v1/collections/docs/query/text \
  -H 'authorization: Bearer …' -H 'content-type: application/json' \
  -d '{"text":"what is quiver?","k":5,"rerank":true}'
```

```python
hits = q.search_text("docs", "what is quiver?", k=5, rerank=True)
```

```typescript
const hits = await client.searchText("docs", "what is quiver?", { k: 5, rerank: true });
```

Reachable on **every surface**: REST (`POST …/points:text`, `POST …/query/text`),
gRPC (`UpsertText` / `SearchText`), the Python (sync + async) and TypeScript SDKs
(`upsert_text` / `search_text`, `upsertText` / `searchText`), and the
[MCP server](../api/mcp.md) (`upsert_text` / `search_text` tools — run
`quiver mcp --config quiver.toml` so the provider tables are loaded). The
sparse-term cost limit (`QUIVER_MAX_SPARSE_TERMS`) bounds the tokenized query.

## When to use which

- **You already embed** (you run a model, want full control, or use a bespoke
  encoder): keep `upsert` / `search` / `hybrid_search` — no provider needed.
- **You want zero client-side embedding** (prototyping, a simple service, a thin
  agent): configure a provider and use `upsert_text` / `search_text`.
- **Either way**, [hybrid `dense ⊕ BM25`](hybrid-search.md) and the metadata
  pre-filter work the same — `upsert_text` co-populates the BM25 side for you.
