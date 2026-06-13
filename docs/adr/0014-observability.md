# ADR-0014: Observability

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Operators self-host Quiver and need to see health, performance, and security-relevant events; the TUI cockpit needs a live metrics source. Observability must never leak secrets or PII.

## Decision

- **Tracing:** `tracing` spans across `server → query → index → core`, OpenTelemetry-exportable, with trace-id propagation from inbound requests. CPU-bound search spans are recorded so latency is attributable per stage.
- **Metrics:** a Prometheus endpoint `GET /metrics` — per-op QPS, latency histograms (p50/p95/p99), sampled recall, RAM/disk usage, index-build and compaction progress, cache hit rate, auth failures, rate-limit hits. This is the TUI cockpit's data source.
- **Logs:** structured JSON via `tracing-subscriber`, leveled; **secrets/PII never logged**; errors sanitized (ADR-0017).
- **Health:** `GET /healthz` (liveness) and `GET /readyz` (readiness — storage open, indexes loaded, not shedding).
- **Security events** (auth failures, rate-limit/cost-limit breaches, key admin) feed both the audit log (ADR-0011) and metrics.

## Consequences

- **+** First-class operability; one metrics source for Prometheus *and* the cockpit; security signals are visible.
- **−** Instrumentation overhead (kept low: cheap counters, sampled histograms; tracing at a configurable level). Care required to keep sensitive values out of spans/logs (enforced by review + tests on the redaction layer).

## Alternatives considered

- **Logs only** — rejected: no real-time metrics for the cockpit/alerting.
- **A bespoke metrics format** — rejected: Prometheus is the ecosystem standard and OTel-compatible.
