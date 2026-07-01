// SPDX-License-Identifier: AGPL-3.0-only
//! Prometheus metrics (ADR-0014, ADR-0054): a small, dependency-free registry
//! rendered as Prometheus text on `GET /metrics`.
//!
//! Per `(method, route-template)` it tracks a request counter, an error counter
//! (status ≥ 400), and a latency histogram; plus two process-wide security
//! counters (auth failures, rate-limit rejections) shared by both transports.
//! The route label is the matched **template** (`/v1/collections/{name}/query`),
//! never the concrete path, so it is bounded and never leaks ids.
//!
//! Deliberately hand-rolled rather than pulling the `metrics` /
//! `prometheus` crate stack: the exposition format is a few lines of text and a
//! fixed-bucket histogram is a small array, so a dependency (and its
//! `cargo deny` surface) buys nothing here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

// Latency histogram bucket upper bounds, in seconds (the Prometheus `le`s).
const BUCKETS: [f64; 12] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];

#[derive(Default)]
struct RouteStat {
    count: u64,
    errors: u64,
    sum_seconds: f64,
    // Non-cumulative counts per bucket; observations above the last bound fall
    // into the implicit `+Inf` bucket (`count - sum(bucket_counts)`).
    bucket_counts: [u64; BUCKETS.len()],
}

impl RouteStat {
    fn observe(&mut self, status: u16, secs: f64) {
        self.count += 1;
        if status >= 400 {
            self.errors += 1;
        }
        self.sum_seconds += secs;
        for (i, &le) in BUCKETS.iter().enumerate() {
            if secs <= le {
                self.bucket_counts[i] += 1;
                break;
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    routes: HashMap<String, RouteStat>,
    auth_failures: u64,
    rate_limited: u64,
}

/// Process-wide metrics registry, rendered as Prometheus text on `/metrics`.
///
/// TODO(perf): one global mutex (mirrors the rate limiter, ADR-0049). Recording is
/// a HashMap lookup + a few integer adds under the lock; per-route sharded
/// atomics are the upgrade path if a profile ever shows contention here.
#[derive(Default)]
pub(crate) struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned metrics mutex must never break a request — recover the guard.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one served request by `(method, route-template, status)`.
    pub(crate) fn observe_request(
        &self,
        method: &str,
        route: &str,
        status: u16,
        elapsed: Duration,
    ) {
        let key = format!("{method} {route}");
        self.lock()
            .routes
            .entry(key)
            .or_default()
            .observe(status, elapsed.as_secs_f64());
    }

    /// An authentication failure (missing or invalid key) on either transport.
    pub(crate) fn incr_auth_failure(&self) {
        self.lock().auth_failures += 1;
    }

    /// A request rejected by the per-key rate limiter (ADR-0049).
    pub(crate) fn incr_rate_limited(&self) {
        self.lock().rate_limited += 1;
    }

    /// Render the current values in the Prometheus text exposition format.
    pub(crate) fn render(&self) -> String {
        let inner = self.lock();
        let mut out = String::new();

        out.push_str("# HELP quiver_http_requests_total Total HTTP requests.\n");
        out.push_str("# TYPE quiver_http_requests_total counter\n");
        let mut keys: Vec<&String> = inner.routes.keys().collect();
        keys.sort();
        for key in &keys {
            let (method, route) = split_key(key);
            let stat = &inner.routes[*key];
            out.push_str(&format!(
                "quiver_http_requests_total{{method=\"{method}\",route=\"{}\"}} {}\n",
                esc(route),
                stat.count
            ));
        }

        out.push_str("# HELP quiver_http_request_errors_total HTTP requests with status >= 400.\n");
        out.push_str("# TYPE quiver_http_request_errors_total counter\n");
        for key in &keys {
            let (method, route) = split_key(key);
            out.push_str(&format!(
                "quiver_http_request_errors_total{{method=\"{method}\",route=\"{}\"}} {}\n",
                esc(route),
                inner.routes[*key].errors
            ));
        }

        out.push_str(
            "# HELP quiver_http_request_duration_seconds HTTP request latency in seconds.\n",
        );
        out.push_str("# TYPE quiver_http_request_duration_seconds histogram\n");
        for key in &keys {
            let (method, route) = split_key(key);
            let route = esc(route);
            let stat = &inner.routes[*key];
            let mut cumulative = 0u64;
            for (i, &le) in BUCKETS.iter().enumerate() {
                cumulative += stat.bucket_counts[i];
                out.push_str(&format!(
                    "quiver_http_request_duration_seconds_bucket{{method=\"{method}\",route=\"{route}\",le=\"{le}\"}} {cumulative}\n",
                ));
            }
            out.push_str(&format!(
                "quiver_http_request_duration_seconds_bucket{{method=\"{method}\",route=\"{route}\",le=\"+Inf\"}} {}\n",
                stat.count
            ));
            out.push_str(&format!(
                "quiver_http_request_duration_seconds_sum{{method=\"{method}\",route=\"{route}\"}} {}\n",
                stat.sum_seconds
            ));
            out.push_str(&format!(
                "quiver_http_request_duration_seconds_count{{method=\"{method}\",route=\"{route}\"}} {}\n",
                stat.count
            ));
        }

        out.push_str("# HELP quiver_auth_failures_total Authentication failures.\n");
        out.push_str("# TYPE quiver_auth_failures_total counter\n");
        out.push_str(&format!(
            "quiver_auth_failures_total {}\n",
            inner.auth_failures
        ));
        out.push_str("# HELP quiver_rate_limited_total Requests rejected by the rate limiter.\n");
        out.push_str("# TYPE quiver_rate_limited_total counter\n");
        out.push_str(&format!(
            "quiver_rate_limited_total {}\n",
            inner.rate_limited
        ));

        out
    }
}

// Split a `"METHOD route"` key back into its parts.
fn split_key(key: &str) -> (&str, &str) {
    key.split_once(' ').unwrap_or((key, ""))
}

// Escape a Prometheus label value: backslash, double-quote, newline.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_counters_and_a_histogram() {
        let m = Metrics::default();
        m.observe_request("GET", "/v1/collections", 200, Duration::from_millis(2));
        m.observe_request("GET", "/v1/collections", 500, Duration::from_millis(40));
        m.incr_auth_failure();
        m.incr_rate_limited();
        m.incr_rate_limited();
        let text = m.render();

        assert!(
            text.contains("quiver_http_requests_total{method=\"GET\",route=\"/v1/collections\"} 2")
        );
        assert!(text.contains(
            "quiver_http_request_errors_total{method=\"GET\",route=\"/v1/collections\"} 1"
        ));
        // The +Inf bucket equals the total count; the sum/count families are present.
        assert!(text.contains("le=\"+Inf\"} 2"));
        assert!(text.contains(
            "quiver_http_request_duration_seconds_count{method=\"GET\",route=\"/v1/collections\"} 2"
        ));
        assert!(text.contains("quiver_auth_failures_total 1"));
        assert!(text.contains("quiver_rate_limited_total 2"));
        // Each histogram series carries every bucket plus +Inf.
        assert!(text.contains("le=\"0.0005\""));
        assert!(text.contains("le=\"2.5\""));
    }

    #[test]
    fn buckets_are_cumulative_and_monotonic() {
        let m = Metrics::default();
        // One fast (1ms) and one slow (300ms) request.
        m.observe_request("POST", "/v1/q", 200, Duration::from_millis(1));
        m.observe_request("POST", "/v1/q", 200, Duration::from_millis(300));
        let text = m.render();
        // At le=0.005 only the fast one has accrued; at le=0.5 both have.
        assert!(text.contains("le=\"0.005\"} 1"));
        assert!(text.contains("le=\"0.5\"} 2"));
    }
}
