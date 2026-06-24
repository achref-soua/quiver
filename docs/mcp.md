# MCP Server

Quiver ships a [Model Context Protocol](https://modelcontextprotocol.io) server
(ADR-0018) so an AI agent can drive a Quiver database directly as a set of tools.
It speaks JSON-RPC 2.0 over newline-delimited **stdio** and operates an
**in-process** database — there is no network hop and the data is encrypted at
rest with the same secure-by-default posture as the network server.

## Run

```bash
# Encrypted at rest (recommended): provide a 64-hex-character key.
QUIVER_ENCRYPTION_KEY=<64-hex> quiver mcp --data-dir ./data

# Development only — no encryption-at-rest:
quiver mcp --data-dir ./data --insecure

# Enable the text tools (upsert_text / search_text): point at a config with
# [embedding.<collection>] tables (the same file `quiver serve` uses).
QUIVER_ENCRYPTION_KEY=<64-hex> quiver mcp --data-dir ./data --config quiver.toml
```

The process reads requests on stdin and writes responses on stdout, so it is
launched by an MCP-capable client (e.g. an agent runtime) as a subprocess.

## Tools

| Tool | Arguments | Purpose |
|---|---|---|
| `list_collections` | — | List collections |
| `collection_info` | `collection` | Inspect one collection: dim, metric, index, filterable fields, multivector flag, vector-encryption mode, and live point count |
| `create_collection` | `name`, `dim`, `metric` (`l2`\|`cosine`\|`dot`), `index` (`hnsw`\|`vamana`\|`disk_vamana`\|`ivf`), `pq_subspaces?`, `filterable?` (`[{path, field_type: keyword\|numeric}]`), `multivector?`, `vector_encryption?` (`none`\|`dcpe`\|`client_side`) | Create a collection (pick the index, incl. the memory-frugal `disk_vamana`; declare `filterable` fields for hybrid pre-filtered search; set `multivector` for late-interaction / ColBERT; set `vector_encryption` for client-side vector encryption — `dcpe` (experimental, server ranks, L2-only, ADR-0031) or `client_side` (semantically secure opaque AEAD, server does not rank, ADR-0032)) |
| `upsert` | `collection`, `id`, `vector`, `payload?` | Insert/replace a point |
| `search` | `collection`, `vector`, `k?`, `filter?` | k-NN with an optional payload filter |
| `fetch` | `collection`, `filter?`, `limit?` | List points without ranking — the retrieval path for `client_side`-encrypted collections (ADR-0032) |
| `get` | `collection`, `id` | Fetch one point |
| `delete` | `collection`, `id` | Delete one point |
| `upsert_document` | `collection`, `id`, `vectors` (token set), `payload?` | Insert/replace a multi-vector (ColBERT) document |
| `search_multi_vector` | `collection`, `query` (token set), `k?`, `filter?` | MaxSim late-interaction search with an optional payload filter |
| `delete_document` | `collection`, `id` | Delete a multi-vector document |
| `delete_collection` | `collection` | Drop a whole collection and its points (reports whether it existed) |
| `database_stats` | — | Whole-database overview: collection count, total points, per-collection summary, and snapshot status (`manifest_version`, `disk_bytes`) |
| `snapshot` | `destination` | Take a consistent online backup of the whole database into a server-local directory (ADR-0050) |
| `upsert_text` | `collection`, `id`, `text`, `payload?` | Embed `text` server-side and upsert it as a point, co-populating the BM25 full-text field (requires a provider — see below) |
| `search_text` | `collection`, `text`, `k?`, `filter?`, `rerank?`, `rrf_k0?` | Embed the query server-side and run a hybrid dense+BM25 search, optionally reranking (requires a provider — see below) |

`filter` is a Quiver [payload filter](api/wire-protocol.md) tree, e.g.
`{"eq": {"field": "color", "value": "blue"}}`. The full JSON-Schema for each
tool is returned by the standard `tools/list` request.

## Text tools (server-side embedding)

`upsert_text` / `search_text` let an agent store and query documents by **text**,
with Quiver embedding them server-side (ADR-0047/0058) — the agent never runs an
embedding model itself. They require an embedding provider for the collection,
configured exactly as for `quiver serve`: an `[embedding.<collection>]` table
(and an optional `[rerank.<collection>]` for `search_text(rerank=true)`) in the
config passed via `quiver mcp --config <path>` (default `quiver.toml`). See
[Server-side embedding](../features/embedding.md) for the provider table format
and secret handling (API keys are referenced by env-var name, never stored).

Both tools are always advertised by `tools/list`; with no provider configured
they return an `isError` result explaining how to enable them, so an agent can
discover the capability.

## Protocol notes

- Protocol revision `2024-11-05`; capabilities advertise `tools`.
- **Tool execution failures** are returned as a normal result with
  `isError: true` and a human-readable message in the content, so the agent can
  read and recover from them. Malformed JSON-RPC (unknown method, missing tool
  name) returns a JSON-RPC error object instead.
- Embeddings are produced by the caller for `upsert` / `search` — Quiver stays
  model-agnostic — or, with a configured provider, server-side via the
  `upsert_text` / `search_text` tools.
