# ADR-0027: Live migration connectors

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Achref Soua

## Context

ADR-0024 shipped **offline** migration importers: the operator exports a source
collection to a portable file (JSON Lines for Qdrant/pgvector, a `get(...)` dump
for Chroma) and `quiver admin import` parses it into [`ImportPoint`]s and writes
them into a Quiver collection. That needs no network dependency, but it makes the
operator run an export step by hand.

A **live** connector removes that step: point Quiver at a running source and it
pulls the points directly. The cost is real — it needs a network client, and
Quiver is deliberately a minimal-dependency, security-first engine, so every
crate on the import path is supply-chain surface. The sources also differ in how
cleanly a live connector can be built *and validated*:

- **Qdrant** exposes a stable, version-consistent, name-addressed HTTP API
  (`points/scroll`); a live connector needs only a blocking HTTP client and is
  straightforward to get right.
- **Chroma** exposes HTTP too, but the API has churned (v1 vs v2, tenant /
  database addressing, name→id resolution); a live connector is hard to ship with
  confidence without validating against a running instance.
- **Postgres/pgvector** needs a Postgres wire driver; the maintained Rust options
  (`postgres`, `sqlx`) pull `tokio` — a full async runtime — into an otherwise
  synchronous, single-writer engine.

## Decision

**1. Ship a live HTTP connector for Qdrant, on `ureq`.** `ureq` is a small,
blocking HTTP client on `rustls` over `ring` — the same TLS provider Quiver
already uses for transport — with **no async runtime**. It fits `quiver-import`,
which is deliberately synchronous and tokio-free (`reqwest` is present, but only
as an async, dev-only test dependency; using it here would pull `tokio` into a
crate with no other async need). The connector paginates Qdrant's `scroll`
endpoint and feeds each point through the **existing** per-source mapper, so live
and offline import share one normalization and one `import_into` write path.

**2. Chroma and Postgres stay on the offline path.** Both already migrate via
export → `quiver admin import` (ADR-0024). Live Chroma is deferred until its API
can be pinned down and validated against a running instance; live Postgres is
declined to avoid pulling an async runtime into the engine. Neither blocks
migration today — only the manual export step remains for them.

**3. Allow-list the CA-bundle license.** `ureq` verifies TLS with the Mozilla CA
root bundle (`webpki-roots`), licensed CDLA-Permissive-2.0 — a permissive data
license now added to `deny.toml`.

## Consequences

- **+** One-command live migration from Qdrant, no manual export.
- **+** Live and offline paths share the same normalization and write code, so
  the mappers are exercised by both and stay consistent.
- **+** The connector seam (`ImportPoint` + `ureq`) generalizes to Chroma later
  with no new dependency.
- **−** One new dependency (`ureq` + its rustls/HTTP tree) on the import path —
  bounded by the minimal blocking client, the reused rustls/ring stack, and the
  deny/audit gate.
- **−** Live Chroma and Postgres are deferred; their offline routes remain the
  supported path (a documented extra step for those two sources).

## Verification

The connector is tested **hermetically**, with no external services: an
in-process HTTP server (`std::net::TcpListener` on a loopback port) serves canned
Qdrant `scroll` responses — including a paginated, multi-batch case — and the test
asserts the connector paginates, normalizes, and imports the points correctly.
`cargo deny` and `cargo audit` keep the dependency honest. Final validation
against a *running* Qdrant is an operator step, like the reference-hardware
benchmark — the hermetic test proves the fetch/paginate/map plumbing, not a
specific server build.

## Alternatives considered

- **`reqwest` for HTTP** — rejected for `quiver-import`: though present as an
  async, dev-only test dependency, using it here would pull `tokio` + `hyper`
  into a crate that is otherwise synchronous and async-free; `ureq` keeps the
  import path synchronous and minimal.
- **A live Chroma connector now** — deferred: the v1/v2 + tenant/database API
  churn can't be validated here, and shipping an unvalidated connector against a
  moving API would over-promise; the offline `get(...)` path remains.
- **A live Postgres connector** (`postgres`/`sqlx`) — rejected: drags an async
  runtime into the engine for a convenience the offline pgvector path covers.
- **Only offline importers (status quo, ADR-0024)** — rejected: the manual export
  step is real friction for Qdrant, whose stable HTTP API makes a live pull both
  cheap and safe.
