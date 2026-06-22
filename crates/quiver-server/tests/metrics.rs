// SPDX-License-Identifier: AGPL-3.0-only
//! The Prometheus `/metrics` endpoint end-to-end (ADR-0054): served requests are
//! counted and timed by matched-route template, and the security counters
//! increment on an auth failure. `/metrics` is open (scrapable without a key).
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, serve};
use tokio::net::TcpListener;

async fn wait_ready(http: &reqwest::Client, base: &str) {
    for _ in 0..200 {
        if let Ok(resp) = http.get(format!("{base}/healthz")).send().await
            && resp.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not become ready");
}

#[tokio::test]
async fn metrics_endpoint_reports_request_counters_and_histograms() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();
    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        insecure: true,
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // Drive a couple of requests against a real route.
    http.post(format!("{base}/v1/collections"))
        .json(&serde_json::json!({"name": "kb", "dim": 4, "metric": "l2"}))
        .send()
        .await
        .unwrap();
    for _ in 0..3 {
        http.get(format!("{base}/v1/collections"))
            .send()
            .await
            .unwrap();
    }

    let text = http
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Request counter for the matched-route template (not the concrete path).
    assert!(
        text.contains("quiver_http_requests_total{method=\"GET\",route=\"/v1/collections\"} 3"),
        "missing request counter; got:\n{text}"
    );
    // Histogram families for that series.
    assert!(text.contains(
        "quiver_http_request_duration_seconds_count{method=\"GET\",route=\"/v1/collections\"} 3"
    ));
    assert!(text.contains(
        "quiver_http_request_duration_seconds_bucket{method=\"GET\",route=\"/v1/collections\",le=\"+Inf\"} 3"
    ));
    // The security counters are always present.
    assert!(text.contains("quiver_auth_failures_total"));
    assert!(text.contains("quiver_rate_limited_total"));

    server.abort();
}

#[tokio::test]
async fn metrics_counts_auth_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();
    // A configured key means an unauthenticated request is a 401.
    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        insecure: true,
        api_keys: vec!["secret".into()],
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // No Authorization header → 401, recorded as an auth failure.
    let resp = http
        .get(format!("{base}/v1/collections"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let text = http
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // At least one auth failure was counted.
    assert!(
        text.lines()
            .any(|l| l.starts_with("quiver_auth_failures_total ")
                && l.rsplit(' ')
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
                    >= 1),
        "expected an auth failure count; got:\n{text}"
    );

    server.abort();
}
