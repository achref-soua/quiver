// SPDX-License-Identifier: AGPL-3.0-only
//! Per-key rate limiting over REST end-to-end (ADR-0049): a burst within the
//! bucket is admitted and carries the `RateLimit-*` headers; the request past the
//! bucket is refused with 429 + `Retry-After`; an unauthenticated open endpoint is
//! never limited.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use quiver_server::{Config, RateLimitConfig, serve};
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
async fn rest_rate_limit_admits_a_burst_then_returns_429() {
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
        // 1 token/sec, capacity 2 → a burst of 2, then refused.
        rate_limit: RateLimitConfig {
            requests_per_second: 1,
            burst: 2,
        },
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    let list = format!("{base}/v1/collections");

    // The two-token burst is admitted and advertises the RateLimit headers.
    for _ in 0..2 {
        let resp = http.get(&list).send().await.unwrap();
        assert!(resp.status().is_success());
        assert_eq!(resp.headers()["RateLimit-Limit"], "2");
        assert!(resp.headers().contains_key("RateLimit-Remaining"));
        assert!(resp.headers().contains_key("RateLimit-Reset"));
    }

    // The third immediate request exceeds the bucket → 429 with Retry-After.
    let limited = http.get(&list).send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("Retry-After"));
    assert_eq!(limited.headers()["RateLimit-Remaining"], "0");

    // The open health endpoint is never rate-limited, even past the bucket.
    for _ in 0..5 {
        assert!(
            http.get(format!("{base}/healthz"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }

    server.abort();
}

#[tokio::test]
async fn no_rate_limit_headers_when_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();
    // Default config: rate limiting disabled.
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

    // Many requests, none limited and no RateLimit headers when disabled.
    for _ in 0..20 {
        let resp = http
            .get(format!("{base}/v1/collections"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        assert!(!resp.headers().contains_key("RateLimit-Limit"));
    }

    server.abort();
}
