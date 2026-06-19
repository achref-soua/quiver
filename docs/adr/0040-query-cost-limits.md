# ADR-0040 — Query cost limits

**Status:** Accepted
**Date:** 2026-06-19
**Deciders:** Achref Soua

---

## Context

The threat model lists, under denial of service, "query **cost limits** (caps on `k`, `ef`, result
size, concurrent queries)" (`docs/security/threat-model.md:60`) and counts "rate & cost limits" among
the controls against a malicious or compromised client (`threat-model.md:17`). The v0.17.0 security
audit repeats the claim: "Query cost limits cap `k`, `ef_search`, and result sizes"
(`docs/security/audit-0.17.0.md:118`).

The code does not enforce them. `SearchBody` (`crates/quiver-server/src/rest.rs:434`) deserializes
`k: usize` and `ef_search: usize` and passes them straight to the search path (`rest.rs:487` →
`AppState::search` → `Database::search`) with no upper bound; `FetchBody.limit`, the per-request
upsert batch size, the declared collection dimension, the query-vector length, and the payload size
are likewise unbounded. The gRPC handlers in `grpc.rs` share the same `AppState` methods, so they
inherit the gap.

The consequence is a real availability risk: a holder of a valid API key (including a leaked or
over-scoped key) can issue `k = 10_000_000` / `ef_search = 10_000_000`, or upsert a single
multi-megabyte vector, and force unbounded work. Under the single-writer concurrency model (ADR-0006)
an expensive query also blocks every other writer/reader for its duration, so one request degrades the
whole node. This is the security gap behind the one code/docs coherence mismatch recorded in
`docs/analysis/state-of-quiver-v0.17.md`.

A note on scope: a hard *wall-clock* query timeout that cancels work already running is not achievable
under the current `spawn_blocking` execution model — a dropped future stops being awaited but the
blocking computation continues on its thread. Bounding the *inputs* that determine how much work a
request can request is therefore the correct, achievable mitigation; it makes the worst-case cost of
any single authenticated request finite and predictable. Per-key rate limiting and concurrency caps
remain a separate, later control.

## Decision

Enforce per-request cost limits at the **`AppState` choke point** (the same layer that already centralises
authorization), so REST and gRPC are both covered by one implementation, and reject — rather than
silently clamp — any request that exceeds a limit with `CoreError::InvalidArgument`. That error already
maps to HTTP 400 / gRPC `InvalidArgument` (`crates/quiver-server/src/error.rs:49`), so the surfaces need
no new error plumbing. Rejecting (not clamping) is chosen for honest developer experience: a silently
truncated `k` or `ef_search` would return surprising, lower-quality results with no signal.

The limits are configuration fields on `Config` (ADR-0013: defaults → `quiver.toml` → `QUIVER_*` env,
validated at startup), each with a generous default that no legitimate client should hit:

| Field (`QUIVER_*` env) | Default | Bounds |
|---|---|---|
| `max_k` | `10_000` | top-k for `search` / `search_multi_vector` |
| `max_ef_search` | `4_096` | search beam width |
| `max_fetch_limit` | `10_000` | `fetch` page size |
| `max_vector_dim` | `8_192` | declared collection dimension and query-vector length (covers all common embedding sizes, e.g. 3072) |
| `max_payload_bytes` | `65_536` | serialized JSON payload per point (64 KiB) |
| `max_batch_size` | `1_000` | points / documents per upsert request |
| `max_request_body_bytes` | `33_554_432` | HTTP request body (32 MiB), via axum's built-in `DefaultBodyLimit` — no new dependency |

Enforcement points:

- `search` / `search_multi_vector`: reject `k > max_k`, `ef_search > max_ef_search`, and a query whose
  vector length (per token, for multi-vector) exceeds `max_vector_dim`.
- `fetch`: reject `limit > max_fetch_limit`.
- `create_collection`: reject `dim > max_vector_dim`.
- `upsert` / `upsert_documents`: reject a batch larger than `max_batch_size` and any point whose
  serialized payload exceeds `max_payload_bytes`.
- The REST router gains a `DefaultBodyLimit` layer sized from `max_request_body_bytes`.

`Config::validate` additionally rejects a nonsensical configuration (any limit set to `0`).

Documentation is reconciled in the same implementation PR: `threat-model.md` and `audit-0.17.0.md` are
updated to describe the now-real caps and to state plainly what remains deferred (per-key rate limiting;
a work-cancelling query timeout under the blocking model). `.env.example` documents every new knob.

## Consequences

- The threat-model/audit claim becomes true; the High-severity authenticated query-cost DoS finding in
  the state-of-Quiver assessment is closed, and the worst-case cost of any single request is bounded.
- A legitimate request that genuinely needs a larger `k`, dimension, or batch fails with a clear 400
  until the operator raises the corresponding `QUIVER_*` limit — an explicit, documented knob rather
  than a silent failure.
- The limits live in one place; new query surfaces inherit them by calling the same `AppState` methods.
- Not addressed here (and so stated as deferred, not claimed): per-key rate limiting / quotas,
  concurrent-query caps, and a wall-clock timeout that cancels in-flight blocking work.

## Alternatives considered

- **Clamp instead of reject.** Rejected: silently lowering `k`/`ef_search` returns fewer/worse results
  with no signal to the caller — worse than a clear error.
- **Enforce at the DTO/handler layer (per transport).** Rejected: would duplicate the checks across
  REST and gRPC and risk drift; the `AppState` choke point already exists for authorization.
- **Hard query timeout only.** Rejected as the primary control: unachievable under `spawn_blocking`
  without cooperative cancellation in the engine, and it would not prevent the memory blow-up from an
  oversized `k`/result set. Input caps bound both compute and memory.
