# gRPC & REST Surface

The concrete API. The gRPC service in `quiver-proto` is the source of truth; REST + OpenAPI 3.1 are generated to match (ADR-0018). Both run on the same `quiver-server` (gRPC on HTTP/2, REST on HTTP/1.1+2), behind the same auth, RBAC, cost-limit (ADR-0040), and audit middleware.

## gRPC service (representative sketch)

```proto
syntax = "proto3";
package quiver.v1;

service Quiver {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc GetCollection(GetCollectionRequest) returns (Collection);
  rpc ListCollections(ListCollectionsRequest) returns (ListCollectionsResponse);
  rpc DeleteCollection(DeleteCollectionRequest) returns (DeleteCollectionResponse);

  rpc Upsert(UpsertRequest) returns (UpsertResponse);
  rpc UpsertStream(stream UpsertRequest) returns (UpsertResponse); // client-streaming bulk load (ADR-0045)
  rpc UpsertText(UpsertTextRequest) returns (UpsertResponse);      // server-side embedding (ADR-0047)
  rpc DeletePoints(DeletePointsRequest) returns (DeletePointsResponse);
  rpc GetPoints(GetPointsRequest) returns (GetPointsResponse);
  rpc Fetch(FetchRequest) returns (FetchResponse);                // unranked list (ADR-0032)

  rpc Search(SearchRequest) returns (SearchResponse);
  rpc HybridSearch(HybridSearchRequest) returns (SearchResponse); // dense ⊕ sparse BM25, RRF
  rpc SearchText(SearchTextRequest) returns (SearchResponse);     // embed query, optional rerank

  rpc UpsertMultiVector(UpsertMultiVectorRequest) returns (UpsertMultiVectorResponse);
  rpc SearchMultiVector(SearchMultiVectorRequest) returns (SearchMultiVectorResponse); // MaxSim
  rpc DeleteDocuments(DeleteDocumentsRequest) returns (DeleteDocumentsResponse);

  rpc Replicate(ReplicateRequest) returns (stream ReplicationOp); // leader→follower (ADR-0030, admin)
}

// A separate RaftService carries per-shard Raft AppendEntries/Vote/InstallSnapshot
// when the `raft` build feature is enabled (ADR-0067). API keys are provisioned
// through configuration (ADR-0011), not a runtime RPC.

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
| `POST /v1/collections/{id}/points:bulk` | Upsert (bulk load; one fsync + one index rebuild) |
| `POST /v1/collections/{id}/points:text` | UpsertText (server-side embedding, ADR-0047) |
| `DELETE /v1/collections/{id}/points` | DeletePoints |
| `POST /v1/collections/{id}/query` | Search |
| `POST /v1/collections/{id}/query/hybrid` | HybridSearch (dense ⊕ sparse/BM25, RRF) |
| `POST /v1/collections/{id}/query/text` | SearchText (embed query, ⊕ BM25, optional rerank) |
| `POST /v1/collections/{id}/fetch` | Fetch (list points without ranking; the client-side-encryption retrieval path, ADR-0032) |
| `GET /v1/collections/{id}/points/{point_id}` | GetPoints (one point by id) |
| `POST /v1/collections/{id}/documents` | UpsertMultiVector (late-interaction docs) |
| `DELETE /v1/collections/{id}/documents` | DeleteDocuments |
| `POST /v1/collections/{id}/documents/query` | SearchMultiVector (MaxSim) |
| `POST /v1/snapshot` | Snapshot — consistent online backup to a server-local dir (ADR-0050, admin) |
| `GET /cluster/map` | The shard map a router has adopted (404 on a non-router server; read-only) |
| `POST /cluster/raft/voters` · `DELETE /cluster/raft/voters/{id}` | Add/remove a per-shard Raft voter at runtime (ADR-0067 increment 4c, admin; requires the `raft` build feature) |
| `GET /healthz` · `GET /readyz` · `GET /metrics` | ops |

The complete, machine-readable contract for this surface is the committed [OpenAPI 3.1 spec](./openapi.yaml) (`docs/api/openapi.yaml`), pinned to the router by a coverage test. The cluster **coordinator** runs a separate admin API (`/cluster/shards`, `/cluster/shards/grow`, `/cluster/shards/{id}/promote`, `DELETE /cluster/shards/{id}`, `/cluster/health`) — authenticated like the data plane (ADR-0011): reads need any valid key, the mutating shard ops need the admin role.

`CreateCollection` selects the per-collection index (ADR-0007): the JSON body and
the proto request carry `index` (`hnsw` | `vamana` | `disk_vamana` | `ivf`,
default `hnsw`) and an optional `pq_subspaces` for the quantized kinds. `Collection`
responses echo both, so a client can confirm the memory-frugal `disk_vamana` path
was selected. Inner-product (`dot`) is rejected for the graph/IVF kinds (400).

The request also carries `filterable` — payload fields to index for pre-filtered
(hybrid) search (ADR-0022), each a `{ "path": "user.city", "field_type":
"keyword" | "numeric" }`. Declared fields are extracted into the secondary index
at flush time; a `Search` whose `filter` is selective on them is then answered by
an exact scan of the narrowed rows instead of post-filtering ANN hits (perfect
recall, no filtered-search cliff). `Collection` responses echo the declared
fields. Fields left undeclared still filter correctly — they fall back to
post-filtering — they just do not get the pre-filter speed-up.

REST bodies are JSON; vectors are JSON arrays (or base64 for `int8`/`binary`). Errors are RFC-9457 `application/problem+json`; gRPC uses the mapped `Status` (ADR-0017).

## Auth, idempotency, limits (applied uniformly)

- **Auth:** `Authorization: Bearer <api-key>` (REST) / metadata `authorization` (gRPC), or mTLS client cert. Default-deny; scopes checked per resource (ADR-0011).
- **Idempotency:** `Idempotency-Key` header / field on all mutations (see [`wire-protocol.md`](wire-protocol.md)).
- **Limits:** query cost caps — `k`, `ef_search`, `fetch` limit, vector dimension, payload size, upsert batch size, and HTTP request body size (ADR-0040) — rejected with HTTP 400 / gRPC `InvalidArgument` when exceeded. Configure with `QUIVER_MAX_*` (see `.env.example`).
- **Rate limiting:** opt-in per-key token bucket (ADR-0049) — `QUIVER_RATE_LIMIT_REQUESTS_PER_SECOND` / `_BURST` (`0` = off). Over-rate requests get HTTP 429 / gRPC `ResourceExhausted` with `Retry-After`; successful REST responses carry the `RateLimit-Limit` / `RateLimit-Remaining` / `RateLimit-Reset` headers. In-memory, per node.
- **Pagination:** opaque `next_cursor` (forward-only).

## OpenAPI & SDKs

The **OpenAPI 3.1** contract is committed at [`docs/api/openapi.yaml`](./openapi.yaml) and kept in lock-step with the router by a coverage test (`crates/quiver-server/tests/openapi.rs` fails if a route is added or removed without updating the spec). The Python (`uv`) and TypeScript (pnpm) SDKs are maintained against the proto + this spec. The MCP server (`quiver-mcp`) exposes the collection/upsert/search/fetch/document tools over this same surface (ADR-0018/0058).

## Observability hooks

`GET /metrics` serves Prometheus exposition (ADR-0014/0054): per matched-route-template request counters, error counters, and latency histograms (p50/p95/p99 derivable), plus process-wide `quiver_auth_failures_total` and `quiver_rate_limited_total`. The endpoint is open (no API key) so a scraper needs no credential — bind it privately. Engine-facing operations carry secret-free `tracing` spans, OTLP-exportable via a `tracing-opentelemetry` layer. Mutating/admin operations also emit an audit record (ADR-0011). An importable Grafana dashboard ships in `infra/grafana/`.
