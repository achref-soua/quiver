# Grafana dashboard

`quiver-dashboard.json` is an importable Grafana dashboard for Quiver's
Prometheus metrics (ADR-0014, ADR-0054).

## Wiring

1. **Scrape Quiver.** Point Prometheus at the server's open `/metrics` endpoint:

   ```yaml
   # prometheus.yml
   scrape_configs:
     - job_name: quiver
       static_configs:
         - targets: ["quiver:8080"] # the REST address
   ```

   `/metrics` is unauthenticated by design so a scraper needs no API key; bind it
   on a private network or behind your ingress.

2. **Import the dashboard.** In Grafana: *Dashboards → New → Import →* upload
   `quiver-dashboard.json`, and select your Prometheus datasource when prompted.

## Panels

- **Request rate (QPS) by route** — `rate(quiver_http_requests_total[1m])`.
- **Error rate by route** — `rate(quiver_http_request_errors_total[1m])` (status ≥ 400).
- **Latency p50/p95/p99** — `histogram_quantile(…, quiver_http_request_duration_seconds_bucket)`.
- **Security** — `quiver_auth_failures_total` and `quiver_rate_limited_total` rates.

## Traces

Request spans are emitted via `tracing` (`quiver-server`), structured and
secret-free. Export them to an OpenTelemetry collector by attaching a
`tracing-opentelemetry` OTLP layer at startup (see ADR-0054); the span wiring is
already in place across the server's engine-facing operations.
