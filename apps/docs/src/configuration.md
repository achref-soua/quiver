# Configuration reference

Quiver reads configuration from, in increasing precedence: built-in defaults → an
optional `quiver.toml` → `QUIVER_*` environment variables. The config is
**validated at startup** — the server refuses to boot on an insecure
configuration unless `QUIVER_INSECURE=true`. The annotated, copy-pasteable
template is [`.env.example`](https://github.com/achref-soua/quiver/blob/main/.env.example);
this page is the exhaustive reference for every variable.

> **Secure by default.** Without `QUIVER_INSECURE=true`, the server requires an
> API key and an encryption key, and refuses a non-loopback bind without TLS.

## Server

| Variable | Default | Req. | Description |
| --- | --- | --- | --- |
| `QUIVER_REST_ADDR` | `127.0.0.1:6333` | no | REST (HTTP/1.1+2) bind address. A non-loopback bind needs TLS (or `INSECURE`). |
| `QUIVER_GRPC_ADDR` | `127.0.0.1:6334` | no | gRPC (HTTP/2) bind address. |
| `QUIVER_DATA_DIR` | `./quiver-data` | no | Directory for segments, the WAL, and the manifest. |
| `QUIVER_API_KEYS` | — | **yes**¹ | Accepted API keys, comma-separated; each is an all-collections **admin** key. For role/collection-scoped keys, use `[[api_keys]]` tables in `quiver.toml`. |

¹ Required unless `QUIVER_INSECURE=true`. The coordinator needs keys too (see Cluster).

## Encryption at rest (on by default)

| Variable | Default | Req. | Description |
| --- | --- | --- | --- |
| `QUIVER_ENCRYPTION_KEY` | — | **yes**¹ | 256-bit master key as 64 hex chars (`openssl rand -hex 32`). Wraps a per-collection DEK; dropping a collection crypto-shreds it. |
| `QUIVER_MASTER_KEY_FILE` | — | no | Read the hex master key from a file instead of the env (mounted secret). Set exactly one of this or `QUIVER_ENCRYPTION_KEY`. Restrict to `0600` (warned otherwise). |

## TLS in transit

| Variable | Default | Req. | Description |
| --- | --- | --- | --- |
| `QUIVER_TLS_CERT` | — | no² | PEM certificate chain. |
| `QUIVER_TLS_KEY` | — | no² | PEM private key. Set together with `QUIVER_TLS_CERT`. |
| `QUIVER_TLS_CLIENT_CA` | — | no | PEM CA for **mutual TLS**: when set, both transports require a client cert chaining to this CA (in addition to the API key). Requires TLS. |

² Required for a non-loopback bind unless `QUIVER_INSECURE=true`.

## Development opt-out

| Variable | Default | Req. | Description |
| --- | --- | --- | --- |
| `QUIVER_INSECURE` | `false` | no | Disables the secure defaults: allows no API keys, no encryption-at-rest, and a non-loopback bind without TLS. **Never set in production.** |

## Observability

| Variable | Default | Req. | Description |
| --- | --- | --- | --- |
| `RUST_LOG` | `info` | no | Log filter (e.g. `debug`, `quiver_server=debug`). |
| `QUIVER_AUDIT_LOG` | — | no | Path for the append-only JSON-Lines audit log (mutations + denials). Always also emitted as `quiver::audit` tracing events. |
| `QUIVER_OTLP_ENDPOINT` | — | no | OTLP/gRPC endpoint for trace export (requires the `otlp` build feature). |
| `QUIVER_OTLP_SERVICE_NAME` | `quiver` | no | `service.name` resource attribute for exported traces. |
| `QUIVER_OTLP_TIMEOUT_SECS` | `3` | no | Export timeout. |

Prometheus metrics are always served at `GET /metrics` (open; bind privately).

## Replication (ADR-0030)

| Variable | Default | Req. | Description |
| --- | --- | --- | --- |
| `QUIVER_LEADER_URL` | — | no | Run this node as a read-replica **follower** of the leader's gRPC endpoint; it serves reads and refuses writes. Unset = a normal read-write leader. |
| `QUIVER_LEADER_API_KEY` | — | no | API key the follower presents to the leader's admin-scoped `Replicate` stream. |

## Query cost limits (ADR-0040)

Per-request caps; over-limit requests are rejected with HTTP 400 / gRPC
`InvalidArgument`.

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_MAX_K` | `10000` | Max top-k for search / multi-vector search. |
| `QUIVER_MAX_EF_SEARCH` | `4096` | Max search beam width. |
| `QUIVER_MAX_FETCH_LIMIT` | `10000` | Max `fetch` page size. |
| `QUIVER_MAX_VECTOR_DIM` | `8192` | Max collection dimension and query-vector length. |
| `QUIVER_MAX_PAYLOAD_BYTES` | `65536` | Max serialized-JSON payload per point (64 KiB). |
| `QUIVER_MAX_BATCH_SIZE` | `1000` | Max points/documents per upsert request. |
| `QUIVER_MAX_REQUEST_BODY_BYTES` | `33554432` | Max HTTP request body (32 MiB). |
| `QUIVER_MAX_SPARSE_TERMS` | `4096` | Max non-zero terms in a hybrid sparse query (ADR-0043). |
| `QUIVER_MAX_BULK_BATCH_SIZE` | `50000` | Max points per bulk upsert (`points:bulk`, ADR-0045). |

## Rate limiting (ADR-0049, opt-in)

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_RATE_LIMIT_REQUESTS_PER_SECOND` | `0` (off) | Per-key token-bucket refill rate; over-rate → HTTP 429 / gRPC `ResourceExhausted`. |
| `QUIVER_RATE_LIMIT_BURST` | `0` | Bucket burst capacity. |

## Concurrency

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_MVCC_READS` | `false` | **Experimental** (ADR-0064): serve reads of single-vector in-memory collections from a lock-free snapshot. Durability/crash gate unchanged. |

## Cluster router (ADR-0065/0066, opt-in)

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_CLUSTER_SHARDS` | — | Bracketed list of shard base URLs → turns this server into a stateless **router** (HRW sharding + scatter-gather). Empty = single node. |
| `QUIVER_CLUSTER_SHARD_KEY` | — | API key the router/coordinator presents to shards (and that a router presents to a keyed coordinator). |
| `QUIVER_CLUSTER_REPLICAS` | — | Per-shard read replicas, each `"<shard_index>=<replica_url>"` (repeatable). |
| `QUIVER_COORDINATOR` | `false` | Run this process as the cluster **coordinator** (membership API; data-plane-free). Its API is authenticated — set `QUIVER_API_KEYS` here too. |
| `QUIVER_COORDINATOR_STATE` | — | File where the coordinator persists its versioned map + id counter. Unset = in-memory only. |
| `QUIVER_COORDINATOR_URL` | — | On a router, refresh the shard map from this coordinator on an interval (no restart on membership change). |

## Autoscaling (ADR-0065 increment 5, opt-in, coordinator)

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_AUTOSCALE_ENABLED` | `false` | Enable automatic **scale-out** on the coordinator. |
| `QUIVER_AUTOSCALE_HIGH_WATER_POINTS` | `0` | Per-shard point count above which to scale out (`0` disables even when enabled). |
| `QUIVER_AUTOSCALE_STANDBY_URLS` | — | Pool of standby shard URLs to grow into, consumed one per scale-out. |
| `QUIVER_AUTOSCALE_INTERVAL_SECS` | — | How often to sample the load signal. |
| `QUIVER_AUTOSCALE_COOLDOWN_SECS` | — | Minimum delay between scale-outs. |
| `QUIVER_AUTOSCALE_MAX_SHARDS` | — | Cap on the shard count. |

Scale-*in* is not automated — shrink with a manual, drained `DELETE /cluster/shards/{id}`.

## Per-shard Raft write HA (ADR-0067, opt-in, `raft` build feature)

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_RAFT_NODE_ID` | — | This node's Raft id within its shard group. |
| `QUIVER_RAFT_MEMBERS` | — | The shard's Raft members (`<id>=<grpc_url>`, …). A write is acknowledged only after a quorum. |

## Embedded / CLI-only

| Variable | Default | Description |
| --- | --- | --- |
| `QUIVER_CONFIG` | `quiver.toml` | Config file path (the `mcp`/`admin` commands). |
| `QUIVER_TUI_URL` | `http://127.0.0.1:6333` | `quiver tui` target server. |
| `QUIVER_API_KEY` | — | Bearer token for `quiver tui`. |
| `QUIVER_DEMO_DIR` | platform data dir | Data directory override for `quiver demo`. |

Server-side embedding/rerank providers are configured per collection with
`[embedding.<collection>]` / `[rerank.<collection>]` tables in `quiver.toml`
(provider, model, endpoint, dim, `api_key_env`) — see the
[embedding guide](./features/embedding.md).
