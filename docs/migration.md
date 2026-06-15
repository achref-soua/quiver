# Migrating to Quiver

`quiver admin import` loads an export from another vector database — **Qdrant**,
**Chroma**, or **pgvector** — into a Quiver collection, preserving ids, vectors,
payloads, and (optionally) the filterable fields hybrid search needs. The design
is recorded in [ADR-0024](./adr/0024-migration-importers.md).

The importer can either read a **file you export from the source tool** or pull
directly from a **running source** — Qdrant, Chroma, or Postgres
([ADR-0027](./adr/0027-live-migration-connectors.md),
[ADR-0029](./adr/0029-live-chroma-postgres-connectors.md)). Either way it
bulk-loads into a local data directory through the engine — so the result is an
ordinary Quiver store: crash-safe, encrypted at rest (unless `--insecure`), and
immediately serveable with `quiver serve`.

## 1. Export from your current database

**Qdrant** — scroll the collection to JSON Lines (one point per line). Using the
Python client:

```python
from qdrant_client import QdrantClient
import json

client = QdrantClient(url="http://localhost:6333")
with open("qdrant.jsonl", "w") as f:
    offset = None
    while True:
        points, offset = client.scroll(
            "my_collection", with_vectors=True, with_payload=True,
            limit=1000, offset=offset,
        )
        for p in points:
            f.write(json.dumps({"id": p.id, "vector": p.vector, "payload": p.payload}) + "\n")
        if offset is None:
            break
```

**Chroma** — dump the collection's `get(...)` result as one JSON object:

```python
import chromadb, json
col = chromadb.PersistentClient("./chroma").get_collection("my_collection")
data = col.get(include=["embeddings", "metadatas", "documents"])
json.dump(data, open("chroma.json", "w"))
```

**pgvector** — emit one JSON row per line with `row_to_json` (the `embedding`
column comes out as a `"[1,2,3]"` text literal, which the importer parses):

```bash
psql "$DATABASE_URL" -At -c \
  "SELECT row_to_json(t) FROM (SELECT id, embedding, title, category FROM items) t" \
  > pgvector.jsonl
```

## 2. Import into Quiver

```bash
# Qdrant → an encrypted local store (dimension inferred from the export)
export QUIVER_ENCRYPTION_KEY=<64-hex-character master key>
quiver admin import --source qdrant --input qdrant.jsonl \
  --collection my_collection --data-dir ./data --metric cosine

# Chroma, declaring filterable payload fields for hybrid search
quiver admin import --source chroma --input chroma.json \
  --collection docs --data-dir ./data --metric cosine \
  --filterable category:keyword --filterable year:numeric

# pgvector, naming the id/vector columns, no encryption (dev only)
quiver admin import --source pgvector --input pgvector.jsonl \
  --collection items --data-dir ./data --metric l2 \
  --id-field id --vector-field embedding --insecure
```

For a **live** import — no export step — point at a running source instead of
`--input`. All three reuse the same normalization and write path as the offline
importer ([ADR-0027](./adr/0027-live-migration-connectors.md),
[ADR-0029](./adr/0029-live-chroma-postgres-connectors.md)):

```bash
# Qdrant — paginated points/scroll; --collection is the source collection name
quiver admin import --source qdrant --qdrant-url http://localhost:6333 \
  --collection my_collection --data-dir ./data --metric cosine
# add --api-key <key> (or set QDRANT_API_KEY) for a secured Qdrant

# Chroma — v2 HTTP API; resolves the collection name to its id, then paginates get
quiver admin import --source chroma --chroma-url http://localhost:8000 \
  --collection docs --data-dir ./data --metric cosine
# override --chroma-tenant / --chroma-database for a non-default deployment;
# add --api-key <token> for a secured Chroma (sent as x-chroma-token)

# Postgres/pgvector — reads row_to_json over the table; TLS per the URL's sslmode
quiver admin import --source pgvector \
  --postgres-url postgresql://user:pass@localhost/db \
  --table items --collection items --data-dir ./data --metric l2
# --table defaults to --collection; use sslmode=disable for a plaintext/local DB
```

> Live connectors are validated against a hermetic in-process server (Qdrant,
> Chroma) or the offline mapper plus an opt-in integration test (Postgres);
> validating against your *running* instance is the final step on your side.

Then serve it with the **same** key (the importer writes the same encrypted
format the server reads):

```bash
QUIVER_ENCRYPTION_KEY=<same key> quiver serve   # data_dir defaults to ./data
```

## Options

| Flag | Meaning | Default |
|---|---|---|
| `--source` | `qdrant`, `chroma`, or `pgvector` | required |
| `--input` | export file for an offline import (JSON Lines for qdrant/pgvector; one JSON object for chroma) | one of `--input` / a live `--*-url` |
| `--qdrant-url` | base URL of a running Qdrant for a **live** import (qdrant only) | one of `--input` / a live `--*-url` |
| `--chroma-url` | base URL of a running Chroma for a **live** import (chroma only) | — |
| `--chroma-tenant` / `--chroma-database` | Chroma tenant / database for `--chroma-url` | `default_tenant` / `default_database` |
| `--postgres-url` | connection URL of a running Postgres for a **live** import (pgvector only) | — |
| `--table` | source table for `--postgres-url` | `--collection` |
| `--api-key` | API key for a live import: Qdrant `api-key` / Chroma `x-chroma-token` (or `QDRANT_API_KEY`) | — |
| `--collection` | target collection (created if absent, appended to otherwise); also the source collection name for a live import | required |
| `--data-dir` | target data directory | `./data` |
| `--metric` | `l2`, `cosine`, or `dot` (for a newly created collection) | `cosine` |
| `--dim` | vector dimensionality | inferred from the export |
| `--filterable` | `path:type` (`keyword`\|`numeric`), repeatable | none |
| `--id-field` | id column name (pgvector) | `id` |
| `--vector-field` | vector column name | `vector` (qdrant) / `embedding` (pgvector) |
| `--vector-name` | which named vector to import (qdrant) | the sole one |
| `--encryption-key` | 64-hex master key (or `QUIVER_ENCRYPTION_KEY`) | — |
| `--insecure` | import without encryption-at-rest (dev only) | off |

## Notes

- **Ids** are stringified (Qdrant/Chroma integer or UUID ids become strings).
- **Payloads**: Qdrant `payload` is kept as-is; for pgvector every non-id,
  non-vector column becomes a payload field; for Chroma the `metadatas` object is
  the payload and each `documents` entry is stored under a `document` key.
- **Filterable fields** must be declared at import time to be usable by hybrid
  search later (they are extracted into the secondary index at flush —
  [ADR-0022](./adr/0022-secondary-indexes.md)).
- Importing the same export twice **appends** (re-upserting the same ids replaces
  them); the importer never drops an existing collection.
- **Live import** is available for **all three sources** — Qdrant (`--qdrant-url`,
  [ADR-0027](./adr/0027-live-migration-connectors.md)), Chroma (`--chroma-url`)
  and Postgres (`--postgres-url`,
  [ADR-0029](./adr/0029-live-chroma-postgres-connectors.md)) — each pulling
  directly from a running instance through the same normalization as the offline
  path. Live Chroma uses its v2 HTTP API (resolving the collection name to an id
  by listing collections); live Postgres reads `row_to_json` over the table and
  negotiates TLS per the URL's `sslmode`.
