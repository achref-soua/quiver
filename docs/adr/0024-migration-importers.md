# ADR-0024: Migration importers

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

Adoption hinges on a cheap exit from an incumbent. Quiver should let a user move
an existing collection out of Qdrant, Chroma, or pgvector with one command,
preserving ids, vectors, payloads, and the filterable fields hybrid search needs
(ADR-0022). The question is the import surface: connect live to each source, or
read a portable export.

## Decision

`quiver admin import` reads a **file the user exports from the source tool** and
bulk-loads it into a Quiver collection. No live connection is opened.

- **Per-source adapters** normalize each tool's portable export into a common
  `ImportPoint { id, vector, payload }`:
  - **Qdrant** — JSON Lines of scrolled points (`{id, vector, payload}`; `vector`
    may be an array or a named-vector object).
  - **pgvector** — JSON Lines of rows (e.g. from `row_to_json`); the id and vector
    columns are named (defaults `id` / `embedding`, the vector an array or a
    `"[..]"` text literal), and every other column becomes payload.
  - **Chroma** — the single JSON object from `collection.get(include=[...])`
    (`ids` / `embeddings` / `metadatas` / `documents`, zipped; the document is
    stored under a `document` payload key).
- The importer targets the **embeddable `Database`** directly — create the
  collection with the chosen dim / metric / **filterable** fields, then upsert and
  checkpoint — so imported data gets the same crash-safety, encryption-at-rest,
  and indexing as any other write, and is immediately serveable.
- It lives in its own dependency-light `quiver-import` library crate (just
  `quiver-embed` + `serde_json`), so the adapters are unit-testable without a CLI
  or any source service; the CLI is a thin shell over it.

## Consequences

- **+** No new heavy or network dependencies (no HTTP client, no Postgres driver)
  for `cargo deny` / `audit` to vet; adapters are pure and tested from fixture
  strings.
- **+** Export → import is the standard, reproducible migration path; the output
  is an ordinary Quiver data directory — encryptable and serveable.
- **+** Reusing the engine means filterable fields, encryption, and the crash
  gate apply with no special-casing.
- **−** The user must produce the export first (a documented one-liner per tool)
  rather than pointing Quiver at a running instance. Live connectors are a future
  enhancement behind the same `ImportPoint` seam.
- **−** Each source's export shape drifts across versions; the adapters target the
  current, common shapes and fail loudly on a mismatch.

## Alternatives considered

- **Live connectors** (scroll Qdrant's REST, query Chroma's API, `SELECT` from
  Postgres) — rejected for the first increment: drags an HTTP client and a
  Postgres driver into the tree, is hard to test without live services, and
  couples to each tool's server API. Deferred behind the `ImportPoint` seam.
- **A bespoke Quiver dump format only** — rejected: fine for Quiver→Quiver, but it
  does nothing for the actual migrate-off-an-incumbent use case.
- **Importing binary snapshots** (Qdrant snapshot tarballs, Chroma's
  SQLite/parquet) — rejected: brittle, undocumented internal formats; the tools'
  own export / `get` APIs already emit stable JSON.

## References

- ADR-0018 (SDK & integration strategy), ADR-0022 (secondary indexes →
  filterable fields), ADR-0020 / ADR-0021 (the storage the importer writes
  through).
