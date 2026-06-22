# ADR-0054: Prometheus `/metrics` and request tracing

- **Status:** Accepted
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

ADR-0014 committed to a Prometheus `/metrics` endpoint and OpenTelemetry-
exportable `tracing` spans, but `/metrics` shipped as a placeholder
(`"# quiver metrics\n"`) and there were no request spans. Operators self-hosting
Quiver need real request/latency/error signal and security counters, and the
documentation promised a Grafana dashboard. This ADR records the concrete
implementation.

## Decision

- **Metrics registry.** A small, **dependency-free** in-process registry
  (`quiver-server/src/metrics.rs`) rendered as Prometheus text exposition. It
  tracks, per `(method, matched-route-template)`:
  - `quiver_http_requests_total` (counter),
  - `quiver_http_request_errors_total` (counter, status ≥ 400),
  - `quiver_http_request_duration_seconds` (histogram, fixed buckets
    0.5 ms … 2.5 s + `+Inf`, with `_sum` / `_count`);
  plus two process-wide security counters shared by both transports,
  `quiver_auth_failures_total` and `quiver_rate_limited_total`.
  The label is the **matched route template** (`/v1/collections/{name}/query`),
  never the concrete path, so the label set is bounded and never leaks ids.
  No `metrics`/`prometheus` crate is pulled — the exposition format is a few
  lines of text and the histogram is a small array, so a dependency (and its
  `cargo deny` surface) would buy nothing.
- **Recording.** A REST middleware times every routed request (outermost, so a
  401/429 is still counted) and records it after routing (the matched-path
  template is then available). Auth failures and rate-limit rejections are
  recorded at the single auth choke point of **each** transport (the REST `auth`
  middleware and the gRPC `authenticate`), so both are covered with no
  per-handler changes.
- **Endpoint.** `GET /metrics` is **open** (no API key) so a Prometheus scraper
  needs no credential — bind it on a private network / behind ingress.
- **Tracing.** Engine-facing server operations (`upsert`, `search`, `snapshot`,
  …) carry `#[tracing::instrument]` spans with safe fields (collection, k,
  counts) and **never** vectors or payloads. Spans are OTel-exportable: attach a
  `tracing-opentelemetry` OTLP layer at startup to ship them to a collector. The
  exporter crate is **not** bundled — it is a heavy, network-side-effect
  dependency that cannot be exercised in CI, so it stays an opt-in wiring point
  rather than a default dependency (honest about what CI verifies).
- **Grafana.** An importable dashboard (`infra/grafana/quiver-dashboard.json`)
  with QPS, error-rate, latency p50/p95/p99, and the security counters.

## Consequences

- **+** Real, scrapable metrics and request spans with zero new runtime
  dependencies; a published Grafana dashboard; the standing "placeholder
  `/metrics`" gap is closed.
- **+** Security signal (auth failures, rate-limit hits) is visible to alerting,
  feeding the same story as the audit log (ADR-0011).
- **−** Metric recording takes a short global mutex per request (mirrors the
  rate limiter, ADR-0049); per-route sharded atomics are the upgrade path if a
  profile shows contention.
- **−** Histogram buckets are fixed at compile time; a workload far outside the
  0.5 ms … 2.5 s range gets coarse tail resolution. Re-tune the constant if a
  deployment needs it.
- **−** gRPC per-RPC latency histograms are not yet recorded (only the shared
  security counters on the gRPC path); a tonic layer is the follow-up. REST —
  the SDK and dashboard surface — is fully covered.

## Alternatives considered

- **`metrics` + `metrics-exporter-prometheus`** — idiomatic, but adds a
  multi-crate dependency surface for output a few lines of text produce; rejected
  on the dependency-frugality and `cargo deny` grounds.
- **OTLP exporter bundled and always-on** — rejected: a network side effect that
  CI cannot verify and most self-hosters don't run; kept as an opt-in layer.
- **Concrete path as the label** — rejected: unbounded cardinality and an
  id-leak; the matched template is bounded and safe.

## Implementation

`crates/quiver-server/src/metrics.rs` — a dependency-free registry rendered as Prometheus text on `GET /metrics`. A REST middleware records per-(method, matched-route) request/error counters + a latency histogram; both transports' auth choke points record `quiver_auth_failures_total` / `quiver_rate_limited_total`. Engine ops carry `#[tracing::instrument]` spans. Grafana dashboard in `infra/grafana/`.

## Verification

Unit tests (render shape, cumulative+monotonic buckets, `+Inf` = count, security counters) and e2e (`/metrics` shows the matched-route counter + histogram families; an unauthenticated request increments the auth-failure counter).
