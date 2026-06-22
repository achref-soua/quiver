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
secret-free fields (collection, `k`, counts — never vectors or payloads). Spans
are OpenTelemetry-exportable: attach a `tracing-opentelemetry` OTLP layer at
startup to ship them to a collector (the exporter is an opt-in layer, not a
bundled dependency).

## Health

- `GET /healthz` — liveness.
- `GET /readyz` — readiness (storage open, indexes loaded).
