# ADR-0018: SDK & integration strategy

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Quiver must be easy to adopt from many stacks and by AI agents, without coupling the core to any embedding model or vendor. We need a coherent story for the public API surface, client libraries, agent access, and framework integrations.

## Decision

- **One schema, many clients.** The gRPC service defined in `.proto` (in `quiver-proto`) is the **source of truth**; the REST API and its **OpenAPI 3.1** document are generated/kept in lockstep. The wire contract is versioned with SemVer.
- **SDKs:** **Python** (managed with `uv`) and **TypeScript** (pnpm). Transport stubs are generated from proto/OpenAPI; thin hand-written wrappers provide idiomatic ergonomics (typed clients, retries, pagination helpers).
- **Agent access:** an **MCP server** (`quiver-mcp`) exposes tools — create/list collections, upsert, query, manage keys — so external agents can drive Quiver directly.
- **Framework adapters:** LangChain and LlamaIndex vector-store adapters shipped in/with the Python SDK.
- **Embeddings stay the caller's job.** Quiver stores and searches vectors the caller provides. An optional `Embedder` hook allows pluggable embedding for convenience, defaulting **off** — no model is vendored into the core.

## Consequences

- **+** Efficient binary path (gRPC, streaming) *and* broad interop (REST/OpenAPI); agent-native via MCP; meets users where they already are (LangChain/LlamaIndex).
- **+** Model-agnostic: no model weights or vendor lock in the database.
- **−** The proto/OpenAPI must remain authoritative and versioned; SDK releases track API changes and are guarded by **contract tests against the spec**. Two SDKs are ongoing maintenance surface.
- **Open question (→ ADR-0016):** SDK licensing (likely Apache-2.0/MIT rather than AGPL) to avoid deterring client embedding.

## Alternatives considered

- **REST-only** — rejected: no efficient binary/streaming path.
- **A single bespoke binary protocol with no gRPC/REST** — rejected: poor ecosystem interop.
- **Vendoring an embedding model** — rejected: bloat and model lock-in; contradicts the model-agnostic stance.
