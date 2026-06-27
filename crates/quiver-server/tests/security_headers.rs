// SPDX-License-Identifier: AGPL-3.0-only
//! Every response carries the baseline security headers flagged by the v0.29.0
//! OWASP ZAP scan — on the open endpoints and the authed API alike.
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

fn assert_security_headers(headers: &reqwest::header::HeaderMap, label: &str) {
    assert_eq!(
        headers.get("x-content-type-options").map(|v| v.as_bytes()),
        Some(&b"nosniff"[..]),
        "{label}: missing x-content-type-options: nosniff"
    );
    assert_eq!(
        headers
            .get("cross-origin-resource-policy")
            .map(|v| v.as_bytes()),
        Some(&b"same-origin"[..]),
        "{label}: missing cross-origin-resource-policy"
    );
}

#[tokio::test]
async fn every_response_carries_security_headers() {
    let tmp = tempfile::tempdir().unwrap();
    let key = "test-api-key";

    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        api_keys: vec![key.into()],
        encryption_key: Some(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_owned(),
        ),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // Open endpoint.
    let h = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert_security_headers(h.headers(), "/healthz");

    // Authed API — a 401 (no key) must still carry the headers.
    let unauth = http
        .get(format!("{base}/v1/collections"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_security_headers(unauth.headers(), "/v1/collections 401");

    // Authed API — a 200 response.
    let ok = http
        .get(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), reqwest::StatusCode::OK);
    assert_security_headers(ok.headers(), "/v1/collections 200");

    server.abort();
}
