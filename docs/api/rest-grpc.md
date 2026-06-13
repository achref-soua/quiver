# gRPC & REST Surface

The concrete API. The gRPC service in `quiver-proto` is the source of truth; REST + OpenAPI 3.1 are generated to match (ADR-0018). Both run on the same `quiver-server` (gRPC on HTTP/2, REST on HTTP/1.1+2), behind the same auth, RBAC, rate-limit, and audit middleware.

## gRPC service (representative sketch)

```proto
syntax = "proto3";
package quiver.v1;

service Quiver {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc GetCollection(GetCollectionRequest) returns (Collection);
  rpc ListCollections(ListCollectionsRequest) returns (ListCollectionsResponse);
  rpc DeleteCollection(DeleteCollectionRequest) returns (DeleteCollectionResponse);

  rpc Upsert(stream UpsertRequest) returns (UpsertResponse);     // client-streaming batches
  rpc DeletePoints(DeletePointsRequest) returns (DeletePointsResponse);
  rpc GetPoints(GetPointsRequest) returns (GetPointsResponse);

  rpc Search(SearchRequest) returns (SearchResponse);
  rpc BatchSearch(BatchSearchRequest) returns (stream SearchResponse);

  rpc CreateApiKey(CreateApiKeyRequest) returns (ApiKey);        // admin scope
  rpc Stats(StatsRequest) returns (StatsResponse);
}

message SearchRequest {
  string collection = 1;
  repeated float vector = 2;        // dtype-specific encodings for f16/bf16/int8/binary
  uint32 k = 3;
  Filter filter = 4;                // structured predicate tree
  SearchParams params = 5;          // ef | nprobe | rerank_factor
  bool with_payload = 6;
  bool with_vector = 7;
  string idempotency_key = 15;
}

message Match { string id = 1; float score = 2; bytes payload = 3; repeated float vector = 4; }
message SearchResponse { repeated Match matches = 1; string next_cursor = 2; }
```

(Filter, dtype encodings, and the full message set are defined in the proto; this is the shape, not the whole file.)

## REST mapping

| Method & path | Operation |
|---|---|
| `POST /v1/collections` | CreateCollection |
| `GET /v1/collections/{id}` | GetCollection |
| `GET /v1/collections` | ListCollections (cursor) |
| `DELETE /v1/collections/{id}` | DeleteCollection (crypto-shred) |
| `POST /v1/collections/{id}/points` | Upsert (batch; `Idempotency-Key`) |
| `DELETE /v1/collections/{id}/points` | DeletePoints |
| `POST /v1/collections/{id}/query` | Search |
| `POST /v1/collections/{id}/query/batch` | BatchSearch |
| `POST /v1/keys` · `GET /v1/keys` · `DELETE /v1/keys/{id}` | API-key admin |
| `GET /v1/collections/{id}/stats` | Stats |
| `GET /healthz` · `GET /readyz` · `GET /metrics` | ops |

REST bodies are JSON; vectors are JSON arrays (or base64 for `int8`/`binary`). Errors are RFC-9457 `application/problem+json`; gRPC uses the mapped `Status` (ADR-0017).

## Auth, idempotency, limits (applied uniformly)

- **Auth:** `Authorization: Bearer <api-key>` (REST) / metadata `authorization` (gRPC), or mTLS client cert. Default-deny; scopes checked per resource (ADR-0011).
- **Idempotency:** `Idempotency-Key` header / field on all mutations (see [`wire-protocol.md`](wire-protocol.md)).
- **Limits:** per-key/tenant rate limits and query cost caps (`k`, `ef`, result size, concurrency); `RateLimit-*` headers; `resource_exhausted` / HTTP 429 on breach.
- **Pagination:** opaque `next_cursor` (forward-only).

## OpenAPI & SDKs

`quiver-server` serves the generated **OpenAPI 3.1** document (and a rendered reference); the Python (`uv`) and TypeScript (pnpm) SDKs are generated/maintained against the proto + OpenAPI and verified by **contract tests** against the spec (ADR-0018). The MCP server (`quiver-mcp`) exposes `CreateCollection`/`Upsert`/`Search`/key-admin as agent tools over this same surface.

## Observability hooks

Every RPC opens a `tracing` span (trace-id propagated from client headers when present), increments Prometheus counters/histograms (per-op QPS, latency, error class), and emits an audit record for mutating/admin operations (ADR-0014).
