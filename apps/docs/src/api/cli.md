# CLI reference

The single `quiver` binary is the entrypoint for every component — the server, the
terminal cockpit, the MCP server, admin tasks, benchmarks, the self-updater, and a
zero-config demo. This page documents every command and flag; it mirrors
`quiver <command> --help`.

```text
Usage: quiver <COMMAND>

Commands:
  serve   Run the server (gRPC + REST)
  tui     Launch the terminal cockpit
  mcp     Run the MCP server for AI agents (JSON-RPC over stdio)
  admin   Administrative commands (imports, collections, keys)
  bench   Run benchmarks
  update  Check for a newer release and optionally install it
  demo    Zero-config demo: seeds vectors, starts the server, opens the cockpit
```

Global options: `-h, --help`, `-V, --version`.

Most configuration is supplied by **environment variables** / `quiver.toml`, not
flags — see the [configuration reference](../configuration.md). Flags shown below
override or supplement those.

## `quiver serve`

Run the server (gRPC + REST). Takes no flags — it is configured entirely from
`quiver.toml` and `QUIVER_*` environment variables (bind addresses, API keys,
encryption, TLS, cluster, rate limits, …). See the
[configuration reference](../configuration.md).

```bash
QUIVER_API_KEYS=… QUIVER_ENCRYPTION_KEY=… quiver serve
```

## `quiver tui`

Launch the terminal cockpit against a running server.

| Flag | Env | Default | Description |
| --- | --- | --- | --- |
| `--url <URL>` | `QUIVER_TUI_URL` | `http://127.0.0.1:6333` | REST base URL of the server to inspect. |
| `--api-key <API_KEY>` | `QUIVER_API_KEY` | — | API key presented as a bearer token, if the server requires one. |

## `quiver mcp`

Run the MCP server for AI agents (JSON-RPC over stdio). Opens the embedded
database directly (no network server).

| Flag | Env | Default | Description |
| --- | --- | --- | --- |
| `--data-dir <DATA_DIR>` | `QUIVER_DATA_DIR` | `./data` | Data directory for the embedded database. |
| `--encryption-key <KEY>` | `QUIVER_ENCRYPTION_KEY` | — | 64-hex-character key for encryption-at-rest. |
| `--insecure` | `QUIVER_INSECURE` | `false` | Run without encryption-at-rest (development only). |
| `--config <CONFIG>` | `QUIVER_CONFIG` | `quiver.toml` | Config file supplying `[embedding.*]`/`[rerank.*]` provider tables for the `upsert_text`/`search_text` tools. A missing file is fine — those tools then report no provider configured. |

See the [MCP server reference](./mcp.md) for the tool catalog.

## `quiver admin`

Administrative commands.

### `quiver admin import`

Import an export from another vector database into a collection
([ADR-0024](../architecture/adrs.md); see the [migration guide](../features/migration.md)).
Offline (`--input`) or live (`--qdrant-url` / `--chroma-url` / `--postgres-url`).

```text
Usage: quiver admin import [OPTIONS] --source <SOURCE> --collection <COLLECTION>
```

| Flag | Env | Default | Description |
| --- | --- | --- | --- |
| `--source <SOURCE>` | — | *(required)* | Source tool: `qdrant`, `chroma`, or `pgvector`. |
| `--collection <COLLECTION>` | — | *(required)* | Target collection (created if absent, appended to otherwise). |
| `--input <INPUT>` | — | — | Export file (offline): JSON Lines for qdrant/pgvector; a single `collection.get(...)` JSON object for chroma. |
| `--qdrant-url <URL>` | — | — | Live import: base URL of a running Qdrant, instead of `--input`. |
| `--chroma-url <URL>` | — | — | Live import: base URL of a running Chroma (v2 API). |
| `--chroma-tenant <T>` | — | `default_tenant` | Chroma tenant for `--chroma-url`. |
| `--chroma-database <D>` | — | `default_database` | Chroma database for `--chroma-url`. |
| `--postgres-url <URL>` | — | — | Live import: Postgres URL (`postgresql://…`) to pull pgvector rows. |
| `--table <TABLE>` | — | `--collection` | Source table for `--postgres-url`. |
| `--api-key <KEY>` | `QDRANT_API_KEY` | — | API key for a live import: Qdrant `api-key` or Chroma `x-chroma-token`. |
| `--metric <METRIC>` | — | `cosine` | Distance metric for a newly created collection (`l2`, `cosine`, `dot`). |
| `--dim <DIM>` | — | *(inferred)* | Vector dimensionality (inferred from the export when omitted). |
| `--filterable <PATH:TYPE>` | — | — | Filterable payload field as `path:type` (`keyword`\|`numeric`); repeatable. |
| `--id-field <ID_FIELD>` | — | `id` | Id column name (pgvector). |
| `--vector-field <FIELD>` | — | qdrant `vector`, pgvector `embedding` | Vector column name. |
| `--vector-name <NAME>` | — | — | Named vector to import (qdrant named vectors). |
| `--data-dir <DATA_DIR>` | `QUIVER_DATA_DIR` | `./data` | Data directory for the embedded database. |
| `--encryption-key <KEY>` | `QUIVER_ENCRYPTION_KEY` | — | 64-hex master key for encryption-at-rest. |
| `--insecure` | `QUIVER_INSECURE` | `false` | Import into an unencrypted database (development only). |

> A live import that sends a credential over a plaintext `http://` URL (or a
> Postgres URL with `sslmode=disable`) prints a `warning:` first — see the
> migration guide's *Security of live import*.

## `quiver bench`

Run the built-in benchmark harness. See the benchmark methodology in the README.

## `quiver update`

Check for a newer release and optionally install it (downloads, verifies the
SHA-256 checksum, and atomically replaces the binary).

| Flag | Description |
| --- | --- |
| `--check` | Only check whether an update is available; do not download or install. |

## `quiver demo`

Zero-config demo: seeds two collections (a text-searchable `articles` set and a
1 000-vector `demo` set for the constellation view), starts the server on `:7333`
with encryption-at-rest, and opens the cockpit — no config, no network. Override
the data directory with `QUIVER_DEMO_DIR`.
