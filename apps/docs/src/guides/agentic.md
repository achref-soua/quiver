# Agentic patterns

Quiver ships an [MCP server](../api/mcp.md) (`quiver mcp`) that speaks JSON-RPC 2.0
over stdio, so any MCP-capable LLM agent (an IDE assistant or a custom agent) can
use Quiver as a tool: build a knowledge base, retrieve from it, and curate it, all
without bespoke glue. This guide covers the agent-facing surface and a typical loop.

## Connect

Point your MCP client at the binary. Encryption-at-rest is on by default, so the
agent's memory is sealed on disk:

```jsonc
// e.g. an MCP client config
{
  "mcpServers": {
    "quiver": {
      "command": "quiver",
      "args": ["mcp", "--data-dir", "/var/lib/quiver"],
      "env": { "QUIVER_ENCRYPTION_KEY": "<64-hex>" }
    }
  }
}
```

## Tools the agent gets

| Tool | What it does |
|---|---|
| `list_collections` | Enumerate collections |
| `collection_info` | Inspect one collection's shape — dim, metric, index, filterable fields, multivector, encryption, count |
| `database_stats` | A whole-database overview in one call — collection count, total points, a per-collection summary, and snapshot status (`manifest_version`, `disk_bytes`) |
| `create_collection` | Create one (`dim`, `metric`, `index`, `filterable`, `multivector`, `vector_encryption`) |
| `delete_collection` | Drop an entire collection and all its points (reports whether it existed) |
| `snapshot` | Take a consistent online backup of the whole database into a server-local directory (ADR-0050) |
| `upsert` | Insert/replace a point (`id`, `vector`, `payload`) |
| `search` | k-NN with an optional payload `filter` |
| `fetch` | List points by filter without ranking |
| `get` | Fetch one point by id |
| `delete` | Delete a point by id |
| `upsert_document` / `search_multi_vector` / `delete_document` | Multi-vector (ColBERT) late-interaction documents |

All calls go through the same authorized op layer and cost limits (ADR-0040) as
REST/gRPC, so an agent cannot exceed the server's guardrails.

## A typical agent loop

A research assistant maintaining its own long-term memory:

1. `list_collections` → does a `research` collection exist? If not,
   `create_collection("research", dim=…, metric="cosine", filterable=[{path:"topic",field_type:"keyword"}])`.
2. As it reads sources, the agent embeds passages (with its own model) and
   `upsert`s them with `{topic, url, added}` payloads.
3. To answer a question, it embeds the query and `search`es with a `filter`
   (e.g. `topic = "vector-db"`), then grounds its answer in the returned text.
4. It `delete`s stale entries or `fetch`es a topic to review what it knows.

Because Quiver is model-agnostic, the **agent owns the embedding step** — pass
the float vectors it produces. The server stores, filters, and ranks.

## Tips

- **Declare `filterable` fields up front** so the agent can scope retrieval
  (per-user, per-topic, per-recency) — the pre-filter is exact.
- **Scope the agent's API key** (RBAC, [security overview](../security/overview.md))
  to just its collection prefix, so a tool call can't touch other data.
- **Use `client_side` vector encryption** ([client-side vectors](../security/client-side-vectors.md))
  if the agent's host should not be able to read the stored vectors — the agent
  fetches and ranks locally.
- For **paragraph-grained** memory, store documents as token sets and use
  `search_multi_vector` ([multi-vector](../features/multi-vector.md)).
