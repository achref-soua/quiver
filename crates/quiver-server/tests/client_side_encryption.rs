// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side payload encryption end-to-end (ADR-0012): prove that a payload
//! sealed with a key the server never sees is **unreadable by the server** — it
//! is returned verbatim as ciphertext over the API and never appears in
//! plaintext on disk — while the client that holds the key recovers it, and a
//! cleartext sibling field stays server-filterable.
//!
//! Encryption-at-rest is deliberately turned **off** here (`insecure = true`, no
//! `encryption_key`), so the only thing hiding the secret is the client-side
//! envelope, not the storage codec. The companion at-rest proof lives in
//! `quiver-crypto/tests/at_rest.rs`.
//!
//! Integration-test helpers are not `#[test]` fns, so the crate's `clippy.toml`
//! unwrap/expect allowance does not reach them; opt in explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use quiver_crypto::PayloadCipher;
use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// The secret never leaves the client in the clear. Underscores keep it outside
// the base64 alphabet, so it cannot appear by coincidence inside the ciphertext
// encoding either — its absence on disk is unambiguous.
const SECRET: &str = "SECRET_SSN_078_05_1120_do_not_log";
// A cleartext, server-filterable sibling field. Unique enough that finding it on
// disk is a meaningful positive control for the scanner.
const TIER: &str = "platinum_clearmark_9d4e1f";
// A 256-bit client key, present only on the client — never in the server config.
const CLIENT_KEY_HEX: &str = "fedcba98765432100123456789abcdeffedcba98765432100123456789abcdef";

// Recursively read every file under `root` and report whether any contains the
// needle bytes (mirrors the at-rest disk scanner).
fn tree_contains(root: &Path, needle: &[u8]) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        } else if meta.is_file() {
            let bytes = fs::read(&path).unwrap();
            if bytes.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        }
    }
    false
}

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
async fn server_cannot_read_client_encrypted_payload() {
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
        // At-rest encryption OFF on purpose: isolate the client-side envelope as
        // the only thing protecting the secret. `insecure` permits this.
        encryption_key: None,
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        insecure: true,
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // Create a collection declaring `tier` filterable — a cleartext field the
    // server may index, in contrast to the encrypted payload it cannot.
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&json!({
            "name": "vault",
            "dim": 4,
            "metric": "l2",
            "filterable": [{"path": "tier", "field_type": "keyword"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // The CLIENT seals the secret with a key the server config never contains,
    // then merges the envelope alongside a cleartext, filterable `tier` field.
    let cipher = PayloadCipher::from_hex(CLIENT_KEY_HEX).unwrap();
    let sealed = cipher.seal(&json!({ "ssn": SECRET })).unwrap();
    let mut payload = json!({ "tier": TIER });
    payload
        .as_object_mut()
        .unwrap()
        .extend(sealed.as_object().unwrap().clone());

    let resp = http
        .post(format!("{base}/v1/collections/vault/points"))
        .bearer_auth(key)
        .json(&json!({
            "points": [{ "id": "p1", "vector": [1.0, 2.0, 3.0, 4.0], "payload": payload }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // (1) The server returns only ciphertext over the API.
    let resp = http
        .get(format!("{base}/v1/collections/vault/points/p1"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let returned = &body["payload"];
    assert_eq!(returned["tier"], json!(TIER), "cleartext sibling survives");
    assert!(
        returned.get("__quiver_enc__").is_some(),
        "the sealed envelope is stored and returned verbatim"
    );
    let whole_response = serde_json::to_string(&body).unwrap();
    assert!(
        !whole_response.contains(SECRET),
        "the server returned the client's plaintext secret: {whole_response}"
    );

    // (2) The cleartext sibling stays server-filterable; the encrypted field
    // cannot be (the honest ADR-0012 tradeoff, shown positively).
    let resp = http
        .post(format!("{base}/v1/collections/vault/query"))
        .bearer_auth(key)
        .json(&json!({
            "vector": [1.0, 2.0, 3.0, 4.0],
            "k": 5,
            "filter": {"eq": {"field": "tier", "value": TIER}},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(
        matches.len(),
        1,
        "the filter on the cleartext field matched"
    );
    assert_eq!(matches[0]["id"], json!("p1"));
    let search_response = serde_json::to_string(&body).unwrap();
    assert!(
        !search_response.contains(SECRET),
        "search results must not contain the plaintext secret"
    );

    // (3) The secret never reaches disk in plaintext, even with at-rest off,
    // while the cleartext sibling does — the scanner works and the boundary is
    // exactly the sealed field.
    assert!(
        !tree_contains(tmp.path(), SECRET.as_bytes()),
        "client-encrypted secret leaked to disk"
    );
    assert!(
        tree_contains(tmp.path(), TIER.as_bytes()),
        "cleartext field should be visible on disk with at-rest off (positive control)"
    );

    // (4) Only the client, holding the key, recovers the plaintext.
    let recovered = cipher.open(returned).unwrap();
    assert_eq!(recovered, json!({ "ssn": SECRET }));

    server.abort();
}
