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
```

The process reads requests on stdin and writes responses on stdout, so it is
launched by an MCP-capable client (e.g. an agent runtime) as a subprocess.

## Tools

| Tool | Arguments | Purpose |
|---|---|---|
| `list_collections` | — | List collections |
| `create_collection` | `name`, `dim`, `metric` (`l2`\|`cosine`\|`dot`), `index` (`hnsw`\|`vamana`\|`disk_vamana`\|`ivf`), `pq_subspaces?`, `filterable?` (`[{path, field_type: keyword\|numeric}]`) | Create a collection (pick the index, incl. the memory-frugal `disk_vamana`; declare `filterable` fields for hybrid pre-filtered search) |
| `upsert` | `collection`, `id`, `vector`, `payload?` | Insert/replace a point |
| `search` | `collection`, `vector`, `k?`, `filter?` | k-NN with an optional payload filter |
| `get` | `collection`, `id` | Fetch one point |
| `delete` | `collection`, `id` | Delete one point |

`filter` is a Quiver [payload filter](api/wire-protocol.md) tree, e.g.
`{"eq": {"field": "color", "value": "blue"}}`. The full JSON-Schema for each
tool is returned by the standard `tools/list` request.

## Protocol notes

- Protocol revision `2024-11-05`; capabilities advertise `tools`.
- **Tool execution failures** are returned as a normal result with
  `isError: true` and a human-readable message in the content, so the agent can
  read and recover from them. Malformed JSON-RPC (unknown method, missing tool
  name) returns a JSON-RPC error object instead.
- Embeddings are produced by the caller — Quiver stays model-agnostic.
