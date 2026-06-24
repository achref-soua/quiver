# ADR-0059: OpenTelemetry traces exporter (opt-in, feature-gated)

- **Status:** Accepted
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

ADR-0054 added Prometheus `/metrics` and `#[tracing::instrument]` spans across the
upsert/search/snapshot paths, but **deliberately left the OTLP exporter
unbundled**: the `opentelemetry` crate tree is heavy, and forcing it on every
build (and into `cargo-deny`'s scope) for a feature most single-node operators
don't use was the wrong default. The spans existed but could only be seen in the
`fmt` logs — there was no way to ship them to Jaeger / Tempo / Grafana / any OTLP
collector for distributed tracing. This ADR closes that gap without changing the
default footprint.

## Decision

**Add an OpenTelemetry traces exporter behind a `otlp` cargo feature (off by
default) that activates only when an OTLP endpoint is configured at runtime.**

### Two independent gates

1. **Build:** the OpenTelemetry crates (`opentelemetry`, `opentelemetry_sdk`,
   `opentelemetry-otlp`, `tracing-opentelemetry`) are **optional** dependencies of
   `quiver-server`, enabled only by `--features otlp`. With the feature off none
   of them are linked.
2. **Runtime:** even compiled in, export is off unless `OtlpConfig::endpoint`
   (`[otlp] endpoint` / `QUIVER_OTLP_ENDPOINT`) is set. Empty endpoint ⇒ `fmt`-only.

### Transport and wiring

- **OTLP/gRPC** (`opentelemetry-otlp` with `default-features = false,
  features = ["grpc-tonic", "trace"]`), so it **reuses the `tonic`/`prost`
  already in the tree** rather than pulling `reqwest`. The standard collector
  endpoint is `http://localhost:4317`.
- A batched (`SdkTracerProvider` + batch span exporter) provider is built and the
  `tracing_opentelemetry` layer is added to the subscriber.
- `init_tracing()` becomes a thin wrapper over a new `init_observability(&Config)`
  that composes the `fmt` layer with the optional OTLP layer in one `try_init`.
  The CLI loads config first, calls `init_observability`, runs, then calls
  `shutdown_observability()` to flush batched spans on exit.
- Config (`OtlpConfig`: `endpoint`, `service_name` = "quiver", `timeout_secs` =
  10) is **always present and unit-tested**, with the flat `QUIVER_OTLP_*` env
  overrides applied like the other sections — so a `quiver.toml` is validated
  identically regardless of build features.

### Failure posture

Telemetry must never take the database down: a failure to build the exporter
**logs a warning and falls back to `fmt`-only**, rather than erroring out of
startup.

## Consequences

- Operators who want distributed traces build with `--features otlp` and set an
  endpoint; everyone else is unaffected (no new deps, no behaviour change).
- `cargo-deny` was run over the full opentelemetry tree and is **clean**
  (advisories / bans / licenses / sources ok); the only effect is a few
  `multiple-versions = "warn"` duplicates, which the repo policy already permits.
- CI gains a `cargo clippy -p quiverdb-server --features otlp` step so the gated
  exporter cannot rot even though the default build excludes it.
- **Testing honesty:** the `OtlpConfig` parsing/enablement logic is fully
  unit-tested. The code that builds the live exporter and connects to a collector
  is feature-gated and **not exercised in CI** (it needs a running OTLP
  collector) — a thin shell over the `opentelemetry-otlp` builder, stated rather
  than faked, consistent with ADR-0047's posture for the embedding providers.

## Alternatives considered

- **Bundle OpenTelemetry unconditionally.** Rejected: imposes the heavy tree and
  its advisory surface on every build/deploy for a feature many won't use — the
  exact reason ADR-0054 left it out.
- **HTTP/protobuf transport (`reqwest`).** Rejected: would add `reqwest` (hyper,
  etc.) to the production tree; `grpc-tonic` reuses dependencies already present.
- **Always-on at runtime when compiled.** Rejected: the endpoint gate lets a
  single binary be shipped with the feature compiled but inert until an operator
  opts in via config.
