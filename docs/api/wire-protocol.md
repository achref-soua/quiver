# Wire Protocol

This document defines Quiver's *logical* protocol — the operations, type model, error model, idempotency, pagination, and streaming semantics — independent of encoding. It is realized concretely by two transports (see [`rest-grpc.md`](rest-grpc.md)); the gRPC `.proto` in `quiver-proto` is the **source of truth**, the REST/OpenAPI surface is generated to match.

## A deliberate decision: gRPC *is* the compact binary protocol

We do **not** invent a third bespoke on-the-wire framing. gRPC (Protocol Buffers over HTTP/2) already gives us a compact, binary, streaming, multiplexed protocol with a mature, audited stack (`tonic`/`rustls`). Building a custom binary protocol alongside it would be needless surface area and a security/maintenance liability (NIH). REST/JSON exists for broad interop and human debuggability. This is a conscious choice, recorded here so its absence is not mistaken for an omission.

## Resource & type model

- **Collection** — `{ id, name, dim, dtype, metric, index_type, quantization, filterable_fields[], created_at }`.
- **Point** — `{ id: string|u128, vector: [dtype; dim], payload: json|bytes, payload_encrypted: bool }`.
- **Filter** — a typed predicate tree over filterable fields: `eq/neq/in/lt/lte/gt/gte/exists` combined with `and/or/not` (the structured form the planner turns into roaring-bitmap operations).
- **Query** — `{ vector, k, filter?, params{ ef|nprobe, rerank_factor }, with_payload, with_vector }`.
- **Match** — `{ id, score, payload?, vector? }`.

## Operations (logical)

`CreateCollection`, `GetCollection`, `ListCollections`, `DeleteCollection`; `Upsert`(batch), `DeletePoints`, `GetPoints`; `Search`, `BatchSearch`; `CreateApiKey`, `ListApiKeys`, `RevokeApiKey`; `Stats`, `Health`, `Ready`.

## Cross-cutting semantics

- **Idempotency.** Every mutating operation accepts an **idempotency key** (header `Idempotency-Key` / a request field). The server records the key + result for a TTL; a retry with the same key returns the original result without re-applying — making upserts safe under at-least-once delivery.
- **Pagination.** List/large-result operations use **opaque cursors** (forward-only), not offsets — stable under concurrent writes and cheap on the storage engine.
- **Streaming.** gRPC server-streaming for large `Upsert`/`Search` result sets and scans; REST uses chunked responses / cursors. Batch sizes and result sizes are bounded by configurable cost limits.
- **Error model.** A stable taxonomy maps engine errors → gRPC `Status` codes and → REST **RFC-9457** `application/problem+json` (ADR-0017). Messages are sanitized: no internal paths, no secrets. Categories: `invalid_argument`, `not_found`, `already_exists`, `permission_denied`, `unauthenticated`, `resource_exhausted` (rate/cost limit), `failed_precondition`, `internal`, `unavailable`.
- **Versioning.** The wire contract is **SemVer**; backward-compatible fields are additive; breaking changes bump the major and are gated behind a version negotiation. The storage format version (ADR-0004) is independent of the wire version.
- **Auth & limits.** Every request carries credentials (API key / mTLS); responses surface rate-limit headers (`RateLimit-Remaining`, `RateLimit-Reset`). See [ADR-0011](../adr/0011-authn-authz-tenancy.md).

## Embedded API parity

The embeddable library (`quiver-embed`) exposes the same operations as direct Rust calls (no transport, no auth) so server mode and library mode exercise identical engine semantics — the server is a thin policy/transport shell over the embedded handle.
