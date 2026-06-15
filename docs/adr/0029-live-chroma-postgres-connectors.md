# ADR-0029: Live Chroma and Postgres migration connectors

- **Status:** Proposed
- **Date:** 2026-06-15
- **Deciders:** Achref Soua

## Context

ADR-0027 added a **live** migration connector for Qdrant — point Quiver at a
running source and it pulls the points directly, normalizing each through the
same per-source mapper the offline importers use. It **deferred** live Chroma
and live Postgres, leaving both on the offline export → `quiver admin import`
path (ADR-0024):

- **Chroma** was deferred because its HTTP API had churned (v1 vs v2, tenant /
  database addressing, name→id resolution) and an unvalidated connector against
  a moving API would over-promise.
- **Postgres** was declined to avoid pulling an async runtime into an otherwise
  synchronous import path.

This ADR extends ADR-0027 and reverses both deferrals, completing one-command
live migration from all three supported sources. Two facts have made that the
right call now:

1. **Chroma's v2 HTTP API is the stable, pinnable shape.** Records come from
   `POST /api/v2/tenants/{tenant}/databases/{database}/collections/{id}/get`
   with an `include`/`limit`/`offset` body and return **parallel
   `{ids, embeddings, metadatas, documents}` arrays** — the exact shape the
   offline Chroma `collection.get(...)` mapper already consumes. The only
   wrinkle is that the `/get` path is keyed by the collection **UUID**, not its
   name.
2. **`tokio` is already a workspace dependency** (ADR-0002 — the REST/gRPC
   server runs on it). The embeddable engine (`quiver-embed` / `quiver-core`)
   is and stays tokio-free, but the workspace already builds and audits tokio,
   so a live Postgres connector adds the **rust-postgres driver family** to the
   import crate — not a new async runtime to the engine.

## Decision

**1. Ship a live Chroma connector on `ureq`** — the same blocking,
rustls-over-`ring`, tokio-free client ADR-0027 chose, so **no new dependency**
is added for Chroma. The connector:

- Resolves the collection **name → UUID** by listing collections
  (`GET /api/v2/tenants/{tenant}/databases/{database}/collections`) and matching
  on `name`. This avoids depending on whether a given Chroma build accepts a
  name in the `/get` path — the historical churn point — and works uniformly
  across v2 servers.
- Paginates `POST …/collections/{id}/get` with
  `{"include":["embeddings","metadatas","documents"],"limit":N,"offset":M}`,
  advancing `offset` until a short page ends the scroll.
- Feeds each `{ids, embeddings, metadatas, documents}` page through the
  **existing** Chroma normalization, so live and offline Chroma share one mapper
  and one `import_into` write path.
- Targets Chroma's **v2** API; `tenant` / `database` default to
  `default_tenant` / `default_database` and are overridable; an optional
  `x-chroma-token` header carries an API key.

**2. Ship a live Postgres/pgvector connector on the `postgres` crate** — the
maintained, **blocking** rust-postgres client (a synchronous wrapper over
`tokio-postgres`), which fits `quiver-import`'s synchronous shape. The
connector:

- Connects with a libpq-style URL (`postgresql://user:pass@host:port/dbname`),
  over TLS via the **existing rustls/`ring`** stack (matching the transport and
  Qdrant/Chroma TLS), falling back to a plaintext connection for
  `sslmode=disable` / trusted-network use.
- Reads each row as `row_to_json(...)` — **the same JSON shape the offline
  pgvector path already parses** (a JSON object whose id and vector columns are
  named and whose other columns become payload) — and normalizes it through the
  **existing** pgvector mapper.
- Reuses the shared `import_into` write path, so an encrypted import is
  byte-identical to one from a file.

**3. Both connectors reuse the per-source mappers**, exactly as ADR-0027
established for Qdrant: one normalization, one write path, exercised by both the
live and offline routes.

**4. Keep the dependency tree honest.** Any new license the rust-postgres family
introduces is allow-listed in `deny.toml`; `cargo deny` and `cargo audit` gate
every PR.

## Consequences

- **+** One-command live migration from all three sources (Qdrant, Chroma,
  Postgres) — no manual export step for any of them.
- **+** Live and offline paths share normalization, so the mappers stay
  consistent and are exercised twice.
- **+** Chroma adds **zero** new dependencies (it reuses the `ureq` seam).
- **−** Chroma's API has churned historically; this connector targets v2 and is
  validated **hermetically** — a check against a *running* Chroma is an operator
  step, like the Qdrant precedent. The name→UUID-by-listing choice is the hedge
  against the churn.
- **−** Postgres adds the rust-postgres driver family
  (`postgres` / `tokio-postgres` / `postgres-protocol` / `postgres-types`) plus
  a rustls TLS adapter to `quiver-import`. The engine (`quiver-embed` /
  `quiver-core`) stays tokio-free; the import crate and the already-tokio CLI
  binary gain the driver. Bounded by `deny`/`audit`.
- **−** The Postgres wire protocol and its SCRAM auth cannot be faked in-process,
  so the live Postgres I/O path has no hermetic test; its row→point mapping is
  unit-tested and an env-gated integration test covers a real instance.

## Verification

- **Chroma — hermetic.** An in-process HTTP server (`std::net::TcpListener` on a
  loopback port, the ADR-0027 pattern) serves a canned v2 collection-list plus
  paginated `/get` responses; the test asserts name→id resolution, pagination,
  normalization, and import. No external Chroma required.
- **Postgres — mapping unit-tested + gated integration.** The row→`ImportPoint`
  normalization is covered through the shared pgvector mapper; an `#[ignore]`d
  integration test imports from a real Postgres at `QUIVER_PG_TEST_URL` when an
  operator supplies one. Real-instance validation is an operator step.
- `cargo deny` and `cargo audit` keep the enlarged dependency tree honest.

## Alternatives considered

- **Keep Chroma and Postgres offline (ADR-0027 status quo)** — rejected: the
  manual export step is real friction, and both blockers have eased (Chroma's v2
  API is pinnable; tokio is already in the workspace tree).
- **Resolve the Chroma collection by putting its name in the `/get` path** —
  rejected: whether a name is accepted there has varied across Chroma builds
  (the churn that deferred it); listing and matching by name is version-robust.
- **Hand-roll the Postgres wire protocol over `std::net` to stay driver-free** —
  rejected: re-implementing the startup flow, SCRAM-SHA-256 auth, and row
  decoding is security-sensitive and far more code than a one-shot migration
  convenience warrants; the maintained `postgres` crate is better audited than a
  bespoke client.
- **`sqlx` for Postgres** — rejected: heavier (a larger async/macro surface and
  compile-time query checking Quiver does not need) than the minimal blocking
  `postgres` client.
- **`reqwest` for Chroma** — rejected for the same reason as ADR-0027: `ureq`
  keeps the HTTP path blocking and reuses the rustls/`ring` stack with no async
  runtime.
