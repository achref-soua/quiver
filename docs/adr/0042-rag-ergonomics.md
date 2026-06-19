# ADR-0042 — RAG / agentic ergonomics and usage documentation

**Status:** Proposed
**Date:** 2026-06-19
**Deciders:** Achref Soua

---

## Context

The state-of-Quiver assessment (`docs/analysis/state-of-quiver-v0.17.md`) found Quiver already usable
for **dense RAG and agentic** work today — metadata pre-filtering, LangChain and LlamaIndex
vector-store adapters, an MCP server with full CRUD + multi-vector tools, and ColBERT late
interaction. The gaps are ergonomic and documentary, not capability:

1. The Python SDK is **synchronous only**. A RAG service handling concurrent requests, or an agent
   issuing many retrievals, wants an async client over the same contract.
2. No **Haystack** adapter (LangChain and LlamaIndex exist).
3. The MCP server exposes data operations but **no read-only introspection** (collection stats / index
   info / health) — an agent cannot ask "what collections exist and how big are they?" beyond a bare
   list.
4. There is **no end-to-end RAG/agentic usage documentation** — only API reference. A developer must
   infer the chunk → embed → upsert → filter → search → rerank loop and the index/quantization tuning
   from scattered pages.

These are exactly the things that decide whether Quiver is *pleasant* to drop into an LLM project.

## Decision

Ship the ergonomics and the documentation that make Quiver a first-class RAG/agentic backend, with no
change to the wire contract or the engine.

### SDK ergonomics (`sdks/python`, mirrored in TS where natural)

- **`AsyncClient`** — an `httpx.AsyncClient`-backed mirror of `Client` (same methods, `async`/`await`,
  async context manager). It reuses the existing pure helpers (`_collection`, `_point_dict`,
  `_document_dict`, `_client_side_score`, `_raise_for_status`) so the two clients cannot drift.
- **Bulk-delete-by-filter** — `delete_by_filter(collection, filter)` (server already supports
  fetch+delete; the helper fetches ids by filter and deletes them, paged).
- **Scan iterator** — `scroll(collection, filter=None, batch=N)` yielding points page by page for
  export / re-embedding, over the existing `fetch` limit.
- **Batched upsert with progress** — `upsert_iter(collection, points, batch=N, on_progress=cb)` that
  chunks a large iterable into server-friendly batches (respecting the ADR-0040 `max_batch_size`).

### Haystack adapter

- `quiver.haystack.QuiverDocumentStore` implementing Haystack's `DocumentStore` (write/▪filter/▪delete/
  embedding-retrieval), alongside the existing LangChain/LlamaIndex adapters, under an optional extra
  `quiver-client[haystack]`. Unit-tested with a mocked client, like `tests/test_langchain.py`.

### MCP introspection tools (`crates/quiver-mcp`)

- Add read-only tools an agent needs to reason about state: `collection_info` (dim, metric, index,
  count, filterable fields, vector-encryption) and `stats`/`health`. No new mutating surface; all go
  through the same authorized op layer.

### Usage documentation (the headline deliverable)

New guides under `apps/docs/src/guides/` (and surfaced from the README + docs index):

- **RAG quickstart** — end-to-end: chunk → embed (any model) → `upsert` → filtered `search` →
  (optional) rerank → feed an LLM. A runnable `examples/rag/` Python script using a local/HTTP
  embedding function and Quiver, with no proprietary key required to read.
- **Chunking & metadata patterns** — splitting, overlap, and how to design `filterable` payload fields
  for effective hybrid (pre-filtered) retrieval.
- **Agentic patterns** — driving Quiver from an MCP-speaking agent: create collection, upsert, filter,
  search, introspect, clean up — with the tool list and an example loop.
- **Tuning for RAG** — choosing the index (hnsw / disk_vamana / ivf / colbert) and quantizer
  (scalar/PQ/binary) for a recall ↔ latency ↔ RAM budget; when to use the disk path; the cost-limit
  knobs (ADR-0040).

## Consequences

- Quiver becomes ergonomic for concurrent RAG services (async) and for the Haystack ecosystem, and an
  agent can introspect state over MCP — closing the medium-severity gaps in the assessment.
- The documentation answers "how do I use Quiver in a RAG / agentic / LLM project" directly, with
  runnable code, rather than leaving it to be inferred.
- No wire/engine change: the async client and Haystack adapter are pure clients of the existing REST
  contract; the MCP tools are read-only over the existing op layer.
- Hybrid/sparse retrieval (the larger capability gap) is **out of scope here** — it is the subject of
  ADR-0043 (Phase 4). This ADR makes the *dense* RAG path excellent and fully documented first.

## Alternatives considered

- **Only write docs, no async SDK.** Rejected: the docs would have to recommend the sync client for
  concurrent services, which is a real ergonomic wart; the async client is small (mirrors the sync one)
  and removes the caveat.
- **A server-side embedding step.** Rejected (and a non-goal): Quiver is deliberately model-agnostic
  (the caller embeds). The docs show how to plug any embedder; the engine stays free of model
  dependencies.
