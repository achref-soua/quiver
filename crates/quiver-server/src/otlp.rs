// SPDX-License-Identifier: AGPL-3.0-only
//! OpenTelemetry traces export (ADR-0059).
//!
//! ADR-0054 added Prometheus `/metrics` and `#[tracing::instrument]` spans but
//! **deliberately left the OTLP exporter unbundled** — the `opentelemetry` crate
//! tree is heavy. This module adds it back as a strictly opt-in capability:
//!
//! - **Compiled only behind the `otlp` cargo feature** (off by default). With the
//!   feature off, none of the OpenTelemetry crates are linked.
//! - **Activated only when an endpoint is configured** at runtime
//!   ([`OtlpConfig::endpoint`] / `QUIVER_OTLP_ENDPOINT`). Compiling the feature in
//!   but leaving the endpoint empty exports nothing.
//!
//! The configuration ([`OtlpConfig`]) is always present and unit-tested, so a
//! `quiver.toml` is validated identically regardless of build features. The code
//! that builds the live exporter and talks to a collector is feature-gated and is
//! **not exercised in CI** (it needs a running OTLP collector) — it is a thin
//! shell over the `opentelemetry-otlp` builder, stated rather than faked.

use serde::{Deserialize, Serialize};

/// The default `service.name` resource attribute reported to the collector.
const DEFAULT_SERVICE_NAME: &str = "quiver";
/// Default per-export timeout, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// OpenTelemetry traces export configuration (`[otlp]` in `quiver.toml`, or the
/// `QUIVER_OTLP_*` environment variables). Disabled unless an `endpoint` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OtlpConfig {
    /// The OTLP/gRPC collector endpoint, e.g. `http://localhost:4317`. Empty (the
    /// default) disables export entirely, even when the `otlp` feature is built.
    pub endpoint: String,
    /// The `service.name` resource attribute reported to the collector.
    pub service_name: String,
    /// Per-export timeout, in seconds.
    pub timeout_secs: u64,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            service_name: DEFAULT_SERVICE_NAME.to_owned(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl OtlpConfig {
    /// Whether traces should be exported (an endpoint is configured). The exporter
    /// also requires the `otlp` feature to be compiled in to have any effect.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.endpoint.trim().is_empty()
    }

    /// Apply the flat `QUIVER_OTLP_*` environment overrides (figment nests env
    /// keys under tables, so the flat keys are applied explicitly, as for the
    /// other config sections).
    ///
    /// # Errors
    /// Returns an error if `QUIVER_OTLP_TIMEOUT_SECS` is set to a non-integer.
    pub fn apply_env_overrides(&mut self) -> Result<(), String> {
        if let Ok(v) = std::env::var("QUIVER_OTLP_ENDPOINT") {
            self.endpoint = v;
        }
        if let Ok(v) = std::env::var("QUIVER_OTLP_SERVICE_NAME") {
            self.service_name = v;
        }
        if let Ok(v) = std::env::var("QUIVER_OTLP_TIMEOUT_SECS") {
            self.timeout_secs = v
                .parse()
                .map_err(|_| format!("QUIVER_OTLP_TIMEOUT_SECS must be an integer, got {v:?}"))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Live exporter — feature-gated, not exercised in CI (needs a collector).
// ---------------------------------------------------------------------------

#[cfg(feature = "otlp")]
mod live {
    use std::sync::OnceLock;
    use std::time::Duration;

    use super::OtlpConfig;

    /// Holds the tracer provider so [`shutdown`] can flush batched spans on exit.
    static PROVIDER: OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> = OnceLock::new();

    /// Build a batched OTLP/gRPC tracer provider for `cfg`. Returns an error
    /// string (never panics) so a telemetry misconfiguration degrades to "no
    /// export" instead of taking the server down.
    pub fn build_provider(
        cfg: &OtlpConfig,
    ) -> Result<opentelemetry_sdk::trace::SdkTracerProvider, String> {
        use opentelemetry_otlp::WithExportConfig;
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&cfg.endpoint)
            .with_timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| format!("building OTLP span exporter: {e}"))?;
        let resource = opentelemetry_sdk::Resource::builder()
            .with_service_name(cfg.service_name.clone())
            .build();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();
        Ok(provider)
    }

    /// Remember the provider for shutdown-time flushing.
    pub fn store_provider(provider: opentelemetry_sdk::trace::SdkTracerProvider) {
        let _ = PROVIDER.set(provider);
    }

    /// Flush and shut down the provider, if one was installed.
    pub fn shutdown() {
        if let Some(provider) = PROVIDER.get() {
            let _ = provider.shutdown();
        }
    }
}

#[cfg(feature = "otlp")]
pub use live::{build_provider, shutdown, store_provider};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        let c = OtlpConfig::default();
        assert!(!c.is_enabled());
        assert_eq!(c.service_name, "quiver");
        assert_eq!(c.timeout_secs, 10);
    }

    #[test]
    fn enabled_when_endpoint_set_and_defaults_apply() {
        // A partial config fills the rest from defaults (the `#[serde(default)]`).
        let c: OtlpConfig =
            serde_json::from_value(serde_json::json!({"endpoint":"http://localhost:4317"}))
                .unwrap();
        assert!(c.is_enabled());
        assert_eq!(c.service_name, "quiver");
        assert_eq!(c.timeout_secs, 10);
    }

    #[test]
    fn whitespace_endpoint_is_not_enabled() {
        let c: OtlpConfig = serde_json::from_value(serde_json::json!({"endpoint":"   "})).unwrap();
        assert!(!c.is_enabled());
    }

    #[test]
    fn fields_deserialize() {
        let c: OtlpConfig = serde_json::from_value(serde_json::json!({
            "endpoint":"http://collector:4317","service_name":"q-prod","timeout_secs":3
        }))
        .unwrap();
        assert_eq!(c.service_name, "q-prod");
        assert_eq!(c.timeout_secs, 3);
        assert!(c.is_enabled());
    }

    #[test]
    fn env_overrides_apply() {
        // SAFETY: test-only; these QUIVER_OTLP_* vars are read by no other test.
        unsafe {
            std::env::set_var("QUIVER_OTLP_ENDPOINT", "http://envhost:4317");
            std::env::set_var("QUIVER_OTLP_SERVICE_NAME", "from-env");
            std::env::set_var("QUIVER_OTLP_TIMEOUT_SECS", "7");
        }
        let mut c = OtlpConfig::default();
        c.apply_env_overrides().unwrap();
        assert_eq!(c.endpoint, "http://envhost:4317");
        assert_eq!(c.service_name, "from-env");
        assert_eq!(c.timeout_secs, 7);

        // A non-integer timeout is a clear error.
        unsafe { std::env::set_var("QUIVER_OTLP_TIMEOUT_SECS", "soon") }
        assert!(OtlpConfig::default().apply_env_overrides().is_err());

        unsafe {
            std::env::remove_var("QUIVER_OTLP_ENDPOINT");
            std::env::remove_var("QUIVER_OTLP_SERVICE_NAME");
            std::env::remove_var("QUIVER_OTLP_TIMEOUT_SECS");
        }
    }
}
