# ADR-0047 — Server-side embedding & reranking hooks (provider-agnostic)

**Status:** Accepted
**Date:** 2026-06-22
**Deciders:** Achref Soua

---

## Context

The single biggest friction in using a vector database for RAG is *"I have to embed
the text myself."* Today a caller must turn text into a vector before every
`upsert` and every `search`. ADR-0046 removed that friction for the **lexical**
(BM25) half — `__quiver_text__` and `query_text` give keyword search from raw text —
but the **dense** half still requires the client to run an embedding model and send
a float vector.

Quiver's design has, deliberately, kept the engine **model-agnostic**: `quiver-core`
and the `quiver-embed` engine crate know nothing about embedding models, and that is
a feature (no Python/ONNX/GPU runtime baked into a memory-frugal Rust engine; bring
any model). The cost is DX. Competitors (Qdrant FastEmbed, Weaviate vectorizers,
Chroma embedding functions) close this gap with an *optional, opt-in* server-side
embedding step.

A second, related gap: `rerank()` is a **client-side** helper today (ADR-0042). The
RAG-quality pattern users want is retrieve→rerank in **one call** — fetch a wide
candidate set, then reorder with a cross-encoder / LLM-judge — without a client round
trip.

Constraints that must hold (same posture as ADR-0043/0045/0046):

1. **Never bind a vendor, never hard-code one.** OpenAI, Cohere, Ollama, and any
   local HTTP endpoint must be equal citizens behind one seam.
2. **Keep the engine model-agnostic.** The adapter lives in the **server**, never in
   `quiver-core` or the `quiver-embed` engine. Library-mode users who want raw vectors
   pay nothing and import nothing.
3. **Opt-in, per collection, default off.** A collection without a configured
   provider behaves exactly as today.
4. **No secrets on disk.** A provider's *selection, model, and endpoint* are
   collection configuration; its **API key** is read from server environment at call
   time and is never persisted in the descriptor, the manifest, or the audit log.
5. **Crash gate untouched.** Embedding happens in the server *before* the engine sees
   a normal dense vector; reranking happens *after* the engine returns candidates.
   Neither changes the on-disk format, so the `kill -9` gate is unaffected.
6. **Testable to ~100% without a network.** The provider is a trait with a
   deterministic in-process fake; real HTTP providers are covered at the
   request-build / response-parse seam (honest that live calls are not in CI).

## Decision

Add an **opt-in, provider-agnostic embedding/rerank adapter layer in `quiver-server`**
(`embed_provider` module). It is reachable only when a collection is configured with a
provider, and only over the server surfaces — the embeddable engine is untouched.

### Provider seam

```rust
// quiver-server::embed_provider  (NOT in core / not in the engine crate)
pub trait EmbeddingProvider {
    /// Embed a batch of texts into dense vectors (one per input, fixed dim).
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError>;
}
pub trait RerankProvider {
    /// Score (query, document) pairs; higher is more relevant.
    fn rerank(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, ProviderError>;
}
```

Built-in implementations, each a thin HTTP adapter (reuse the `ureq` already vendored
for `quiver update`; no new heavy async client):

- **OpenAI** (`/v1/embeddings`), **Cohere** (`/v2/embed`, `/v2/rerank`),
  **Ollama** (`/api/embed`, local) — and a generic **`http`** provider (a
  user-supplied OpenAI-compatible URL) so any local/self-hosted model server works
  without a code change.
- A **`fake`** deterministic provider (hash → vector) used by tests and the
  acceptance script — never selectable in production config.

The provider is chosen by a small enum config; the trait is what the request path
holds, so adding a provider is one match arm.

### Per-collection configuration (server-side, no secrets persisted)

Embedding is an **operator/server-edge concern**, so its configuration lives in the
**server configuration** (the existing figment Toml/env layer), keyed by collection —
*not* in the on-disk collection descriptor. This keeps the entire embedding concern at
the server edge (the engine crate and the on-disk format are untouched, so there is no
postcard descriptor migration and the crash gate is trivially unaffected), and it keeps
secrets out of the data directory and out of any data backup:

```toml
[embedding.docs]                       # collection name = "docs"
provider    = "openai"                 # openai | cohere | ollama | http | fake
model       = "text-embedding-3-small"
endpoint    = ""                       # override base URL (required for http/ollama)
dim         = 1536                      # must equal the collection's vector dim
api_key_env = "OPENAI_API_KEY"         # NAME of the env var; the value is never stored

[rerank.docs]                          # optional, same shape, for search_text
provider    = "cohere"
model       = "rerank-v3.5"
api_key_env = "COHERE_API_KEY"
```

`api_key_env` names an environment variable; the server resolves it **at call time** —
the persisted config holds only the variable *name*, never the secret. A startup check
warns if a configured `api_key_env` is unset. `search_text`/`upsert_text` against a
collection with no configured provider return a clear `400 Bad Request` rather than
guessing. (Per-collection config attached to the descriptor and set at create time is a
possible later refinement — see Alternatives — but server config gets the DX win now
with zero engine/on-disk change.)

Under the hood the OpenAI-compatible providers (`openai`, `ollama`, `http`) share one
HTTP adapter parameterized by base URL + auth header — most local and self-hosted model
servers (Ollama's OpenAI-compatible endpoint, vLLM, LM Studio, llama.cpp server) speak
that shape, so "never hard-code a vendor" is satisfied by configuration, not by a
matrix of near-identical adapters. Cohere (a distinct request/response shape, and the
only first-class rerank API) is its own adapter.

### Text ingest and query (the DX win)

New convenience operations on every server surface, active only when the collection
has an embedding provider:

- **`upsert_text`** — `{ id, text, payload? }`: the server embeds `text` → dense
  vector, and *also* tokenizes it into `__quiver_text__` (ADR-0046), so one call
  populates **both** the dense and BM25 sides. The point then flows through the normal
  dense `upsert` — no engine change.
- **`search_text`** — `{ text, k, filter?, rerank? }`: the server embeds `text` → dense
  query and runs the normal dense search (or, when the collection also has sparse/BM25,
  a hybrid `dense ⊕ BM25` via the existing RRF planner). If `rerank` is requested, it
  over-fetches `rerank.candidates` (default `max(k, 50)`), calls the rerank provider on
  the candidate texts, and returns the top-`k` reordered. The existing client-side
  `rerank()` helper stays for clients that prefer to own it.

Reachable as REST (`POST …/points:embed`, `POST …/query/text`), gRPC (`UpsertText`,
`SearchText`), the MCP tools (`upsert_text`, `search_text`), and the Python/TS SDKs.

## Consequences

- Quiver gains "give me text, store/search it" for the **dense** path too —
  text-in/text-out RAG with zero client-side embedding — while the engine stays
  model-agnostic and library mode is unchanged.
- One seam, four+ providers, no vendor lock; adding a provider is a match arm. Any
  OpenAI-compatible local server works via `http` with no code change.
- Secrets never touch disk: the descriptor stores the env-var *name*; the value is
  resolved per call. Embedding/reranking are server-side concerns; the audit log
  records the operation, never the key or the text.
- No on-disk format change (embedding precedes a normal dense upsert; rerank
  post-processes results), so the crash gate and migrations are untouched.
- New network dependency *at request time only when configured*: an embedding call can
  fail or be slow; failures map to a clear 502/`Unavailable` and never corrupt state.
  Latency/cost is the operator's choice by enabling the hook.

## Alternatives considered

- **Embed in the engine (`quiver-core`/`quiver-embed`).** Rejected: binds a model
  runtime into a memory-frugal engine and breaks library-mode model-agnosticism. The
  adapter belongs at the server edge.
- **Bundle a built-in ONNX/Candle embedding model (FastEmbed-style).** Deferred: pulls
  a heavy ML runtime + model weights into the binary against the memory-frugal wedge,
  and still wouldn't cover the providers users already pay for. The provider seam gets
  the DX win now; a bundled-local-model provider can be added behind the same trait
  later if measured demand justifies the weight.
- **Persist API keys in the collection config.** Rejected: secrets on disk / in
  backups. Store the env-var name; resolve at call time.
- **Attach embedding config to the on-disk descriptor (set at create-collection time).**
  Deferred: it would travel with the collection and be settable over the API (the
  Qdrant/Weaviate model), but it pushes an embedding concern into `quiver-core`, adds a
  postcard descriptor-version migration, and risks persisting config near the data.
  Server-side config keeps the concern strictly at the edge with zero engine change; a
  descriptor-attached refinement can come later behind the same provider seam.
- **Server-side rerank only (drop the client helper).** Rejected: keep both — some
  clients want to own reranking; the server stage is the opt-in convenience.
- **A single global provider for the whole server.** Rejected: per-collection lets one
  instance serve multiple models/dims, which RAG users need.

## Implementation

Shipped incrementally (each its own PR, tests + docs in the same PR):

1. `quiver-server::embed_provider` — the two traits, the config enum, the `fake`
   provider, and the OpenAI/Cohere/Ollama/`http` HTTP adapters (request-build +
   response-parse unit-tested; live calls excluded from CI, stated honestly).
2. Server-config `[embedding.<collection>]` / `[rerank.<collection>]` tables
   (name-only secrets) + the startup env-presence warning.
3. `upsert_text` / `search_text` on REST + gRPC + MCP + Python/TS SDKs, with the
   `__quiver_text__` co-population and the optional rerank stage.
4. Docs: `apps/docs` embedding + rerank feature pages, `.env.example` provider keys,
   the acceptance script exercising the `fake` provider end to end.

## Verification

- The provider trait is exercised by the deterministic `fake` provider to full
  coverage of the text-ingest / text-query / rerank request paths.
- Each HTTP adapter is unit-tested at the request-build and response-parse boundary
  (URL, headers minus the secret, body shape; parsing a recorded provider response).
  Live network calls are **not** in CI — stated, not faked.
- No on-disk format change → the `kill -9` crash gate is untouched; no migration.
