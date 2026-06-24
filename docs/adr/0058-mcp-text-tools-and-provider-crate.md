# ADR-0058: MCP text tools and the shared provider crate

- **Status:** Accepted
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

Server-side embedding & reranking (ADR-0047) lets a caller send *text* and have
Quiver embed it — the single biggest RAG-DX win — over REST, gRPC, and the
Python/TypeScript/Go SDKs (`upsert_text` / `search_text`). The MCP server
(`quiver mcp`, ADR-0018) was the one surface still missing it: an agent driving
Quiver through MCP had to embed text itself before calling `upsert` / `search`.

The blocker was structural, not conceptual. The embedding/rerank seam
(`EmbeddingProvider` / `RerankProvider` / `EmbedRegistry`, plus the
OpenAI-compatible and Cohere adapters and the deterministic `fake` provider)
lived **inside `quiver-server`**. `quiver-mcp` wraps `quiver_embed::Database`
in-process and must not depend on `quiver-server` — doing so would pull the whole
`axum` + `tonic` daemon into the MCP binary's dependency tree. So MCP had no way
to reach the seam, and the prior session deferred the tools for exactly this
reason.

## Decision

**Extract the provider seam into its own lean crate, `quiver-providers`, and add
`upsert_text` / `search_text` MCP tools that use it.**

### 1. `quiver-providers` crate

The entire seam moves verbatim from `quiver-server/src/embed_provider.rs` to
`quiver-providers/src/lib.rs` (via `git mv`, history preserved). Its only
dependencies are `serde`/`serde_json`, `thiserror`, `ureq` (the live HTTP calls),
and `figment` (config loading) — no `axum`, no `tonic`, no `tokio`. Both
`quiver-server` and `quiver-mcp` depend on it.

- `quiver-server` **re-exports** the types (`EmbedRegistry`, `EmbeddingConfig`,
  `RerankConfig`, `EmbeddingProvider`, `RerankProvider`, `ProviderKind`,
  `ProviderError`), so its public API is unchanged and `ureq` drops out of its
  manifest (the seam was its only user).
- A new `EmbedRegistry::from_toml_path` reads the `[embedding.*]` / `[rerank.*]`
  tables from a Quiver TOML config — the same tables `quiver serve` reads — so
  the MCP server gets the identical configuration surface without depending on
  `quiver-server`'s `Config`. A missing file yields an **empty** registry (the
  MCP server still starts; the text tools then report "no provider configured"
  only when actually invoked); a malformed file or an unbuildable provider (e.g.
  a missing required API key) is a hard error.

### 2. MCP text tools

`quiver-mcp` gains two tools, mirroring the server's behaviour exactly:

- **`upsert_text`** — embed `{collection, id, text, payload?}` with the
  collection's provider and upsert it as a dense point, co-populating the
  `__quiver_text__` full-text key (ADR-0046) so one call feeds both the dense
  index and BM25, without clobbering a caller-supplied text key.
- **`search_text`** — embed `{collection, text, k?, filter?, rerank?, rrf_k0?}`,
  run a hybrid dense+BM25 search, and (when `rerank` is set and a reranker is
  configured) over-fetch a wide candidate set and reorder it with the rerank
  provider before truncating to `k`.

Both are always **advertised** in `tools/list`; without a configured provider
they return a clear tool error rather than vanishing, so an agent can discover
the capability and learn how to enable it.

The configuration is plumbed through a new `quiver mcp --config <path>` flag
(env `QUIVER_CONFIG`, default `quiver.toml`). The protocol entrypoints gain
embed-aware variants — `run_with_config`, `serve_with_embed`,
`handle_message_with_embed` — and the existing `run` / `serve` / `handle_message`
become thin wrappers that pass an empty registry, so every existing caller and
test is untouched (the zero-churn pattern).

## Consequences

- The agentic surface now has full provider parity with REST/gRPC/the SDKs.
- The engine and `quiver-core`/`quiver-embed` remain model-agnostic and free of
  any network/embedding dependency; library-mode users pay nothing.
- One more crate to publish under the `quiverdb-*` namespace (ADR-0056) when the
  crates.io job is enabled.
- **Testing honesty (unchanged from ADR-0047):** the `fake` provider exercises
  the whole text-in/text-out path in-process — including the new MCP tools and
  the TOML loader — with no network. The live HTTP provider methods remain thin,
  unit-tested-helper-backed shells that are not exercised in CI (stated, not
  faked).

## Alternatives considered

- **Fold the seam into `quiver-embed`.** Rejected: it would add a network
  dependency (`ureq`) to the lean engine crate that embedded/library users link,
  defeating the model-agnostic boundary.
- **`quiver-mcp` depends on `quiver-server`.** Rejected: drags the entire HTTP +
  gRPC daemon into the MCP binary's tree.
- **Pass provider config as tool arguments.** Rejected: would put API keys (or
  their handling) on the wire per call; config-file + env-var-named secrets keep
  the ADR-0047 "no secrets on disk / not in the request" property.
