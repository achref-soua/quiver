# Observability (metrics & tracing)

Quiver exposes operational signal three ways (ADR-0014, ADR-0054): Prometheus
metrics, structured tracing spans, and health endpoints.

## Metrics — `GET /metrics`

An **open** endpoint (no API key, so a scraper needs no credential — bind it on a
private network) serving Prometheus text exposition:

| Metric | Type | Labels |
|---|---|---|
| `quiver_http_requests_total` | counter | `method`, `route` |
| `quiver_http_request_errors_total` | counter (status ≥ 400) | `method`, `route` |
| `quiver_http_request_duration_seconds` | histogram | `method`, `route` |
| `quiver_auth_failures_total` | counter | — |
| `quiver_rate_limited_total` | counter | — |

The `route` label is the **matched route template** (`/v1/collections/{name}/query`),
never the concrete path — so cardinality is bounded and no ids leak. Latency
p50/p95/p99 are derived from the histogram with `histogram_quantile`.

Scrape it with Prometheus:

```yaml
scrape_configs:
  - job_name: quiver
    static_configs:
      - targets: ["quiver:8080"]
```

## Grafana

An importable dashboard ships at `infra/grafana/quiver-dashboard.json` (QPS,
error rate, latency p50/p95/p99, and the security counters). See
`infra/grafana/README.md`.

## Tracing

Engine-facing server operations carry `#[tracing::instrument]` spans with
secret-free fields (collection, `k`, counts — never vectors or payloads). By
default they go to the `RUST_LOG`-filtered `fmt` logger.

### OpenTelemetry export (OTLP) — opt-in (ADR-0059)

To ship spans to an OTLP collector (Jaeger, Tempo, Grafana, …), build the server
with the `otlp` feature and point it at a collector. The feature is **off by
default**, so a normal build links none of the OpenTelemetry crates; even with
the feature compiled in, export stays off until an endpoint is configured.

```bash
# Build with the exporter compiled in.
cargo build -p quiverdb-cli --release --features otlp

# Enable it at runtime (OTLP/gRPC, default collector port 4317).
QUIVER_OTLP_ENDPOINT=http://localhost:4317 \
QUIVER_OTLP_SERVICE_NAME=quiver \
quiver serve
```

Equivalently, a `[otlp]` table in `quiver.toml`:

```toml
[otlp]
endpoint = "http://localhost:4317"   # empty / omitted = disabled
service_name = "quiver"
timeout_secs = 10
```

The transport is OTLP/gRPC (reusing the `tonic` already in the tree, so no extra
HTTP stack). Spans are batched and flushed on shutdown. A failure to build the
exporter logs a warning and falls back to `fmt`-only — telemetry never takes the
server down.

## Health

- `GET /healthz` — liveness.
- `GET /readyz` — readiness (storage open, indexes loaded).
