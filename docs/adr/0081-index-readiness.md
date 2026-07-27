# ADR-0081: Index readiness — never answer a query from an index that was never built

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** Achref Soua
- **Builds on:** [ADR-0062](0062-off-lock-index-rebuild.md) (off-lock index rebuild),
  [ADR-0045](0045-hybrid-everywhere-and-fast-ingest.md) (bulk ingest, deferred index pass)

## Context

Two decisions that are individually right combine into a wrong answer.

[ADR-0045](0045-hybrid-everywhere-and-fast-ingest.md) made a bulk load defer its index
work: `upsert_bulk` marks the collection's index **stale** so a single rebuild pass
replaces N incremental inserts. Its contract, stated in the doc comment, is that "the
next search rebuilds it".

[ADR-0062](0062-off-lock-index-rebuild.md) then made reads non-blocking: a search that
observes a stale index no longer rebuilds inline under the exclusive lock. It **serves
the prior snapshot** — "snapshot-isolated, slightly stale" — and schedules an off-lock
rebuild so the *next* read is fresh (`crates/quiver-server/src/lib.rs:793-796`).

Composed, the embedded engine still honours ADR-0045 (its `search` does rebuild in
place), but **the server does not**: nothing rebuilds before the answer is produced.
For a collection that has been written but whose index has never been built, "the prior
snapshot" is *empty*, and "slightly stale" degrades to **completely wrong**.

This is not a rare edge. `CollectionIndex` holds HNSW as `Hnsw(Hnsw)` — present from
creation, so it absorbs inserts immediately — but `Ivf`, `Vamana`, `Disk` and `Colbert`
are all `Option<…>` and start as `None`. Until their first rebuild lands there is no
live index to absorb a write, so every write takes the `_ => mark_stale(handle)` arm.
**Four of the five index kinds are affected, including `disk_vamana`** — the
memory-frugality path the project leads with.

Measured on the reference box (WSL2, i7-12700H, 15 GiB), 1500 × 32-d points written
through the Python SDK, then queried in a tight loop:

| Collection | index | first query | result |
| --- | --- | --- | --- |
| `race_hnsw` | `hnsw` | t+0.00 s | **10 hits** — correct immediately |
| `race_ivf` | `ivf` | t+0.00 s | **0 hits** |
| `race_ivf` | `ivf` | t+0.26 s | **0 hits** |
| `race_ivf` | `ivf` | t+0.55 s | 10 hits — correct from here on |

The window is ~0.55 s at 1500 points, but it is a *build*, so it scales with the
collection: the recorded first-build for 1M vectors is **236 s**. Nothing in the API
distinguishes the two cases — `GET /v1/collections/{name}` reports the full `count`
(1500) throughout, and the query returns `200 OK` with an empty `matches` array. A
client cannot tell "no vector matched your query" from "this index does not exist yet".

That is a silent wrong answer on the primary read path. For the RAG and agentic use
cases the project documents, it is the worst possible failure shape: the retriever
returns no context and the model answers from nothing, confidently.

## Decision

**A query is never answered from an index that has never been built.** Staleness and
absence are different states and stop sharing a code path.

1. **Distinguish "stale" from "unbuilt".** A collection whose index has a prior
   snapshot keeps exactly today's ADR-0062 behaviour — serve the snapshot, schedule the
   off-lock rebuild, never block. This is a good trade and is not being reversed.
   A collection with **no** prior snapshot has nothing to be isolated *to*, so the
   snapshot-isolation argument does not apply to it.

2. **The first build is awaited, not raced.** A read against an unbuilt index waits for
   the scheduled rebuild rather than returning an empty result. This restores ADR-0045's
   stated contract ("the next search rebuilds it") on the server, where it had silently
   stopped holding. The wait is bounded by the same cost the caller would have paid in
   the embedded engine, and it is paid **once** per collection.

3. **Readiness becomes observable.** `GET /v1/collections/{name}` reports an
   `index_ready` boolean, so a client that would rather poll than block — an ingestion
   job, an operator, the cockpit — can see the state instead of inferring it from an
   empty result set.

The invariant, stated so it can be tested: **an acknowledged write is visible to a
subsequent search on the same connection, or that search blocks until it is; it is
never silently absent.**

## Consequences

- The first query against a freshly-loaded IVF / Vamana / DiskVamana / ColBERT
  collection becomes **slower** (it now includes the build it previously skipped) and
  **correct** (it previously returned nothing). This is the intended trade: a bulk load
  followed by a query is the single most common first-run path, and it currently
  produces an empty result that reads as "Quiver found nothing".
- Steady-state reads are unchanged. A collection with a built index never takes the new
  path, so ADR-0062's non-blocking rebuild — the reason concurrent read throughput is
  what it is — is untouched.
- `index_ready` is additive on the wire, so it is a compatible change to the REST
  surface ahead of the 1.0 stability promise.
- The recorder for the cockpit cast already had to work around this by hand (waiting
  until every collection answers a query before recording); that workaround documents
  the bug and can stay as a cheap guard.

## Alternatives considered

- **Document it and move on.** Rejected. "Your search silently returns nothing for the
  first N seconds after loading data" is not a documentable limitation for a database at
  1.0; it is a bug, and it is invisible precisely when it does the most damage.
- **Return `503` / an `index_building` error instead of blocking.** Honest, and it was
  close. Rejected as the default because it pushes a retry loop into every client and
  every SDK for a condition the server can simply wait out — but this is what
  `index_ready` lets a caller implement deliberately if it prefers polling to blocking.
- **Build the index eagerly at collection creation.** Does not help: the index is built
  from the collection's contents, and at creation there are none. The build has to
  follow the data.
- **Make every index kind non-optional like HNSW, so writes always land in a live
  index.** This is the deepest fix and probably right eventually, but it means an empty
  IVF with untrained centroids and an empty ColBERT with no codebook must both behave
  sensibly under insert — a much larger change to four index families, landing on the
  eve of a 1.0. Sequenced as post-1.0 work; the readiness gate makes it an optimization
  rather than a correctness fix.
- **Reverse ADR-0062 and rebuild inline on read.** Rejected — it would trade a rare
  first-build correctness bug for a permanent concurrency regression on every read.
