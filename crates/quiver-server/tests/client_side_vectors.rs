// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side opaque vector encryption end-to-end (ADR-0032): prove that a
//! client can store vectors on a server that learns **nothing** about them — no
//! coordinates, no distances, no geometry — and still get correct
//! nearest-neighbour results by fetching the entitled set and ranking locally.
//!
//! This is the **semantically secure** end of Quiver's encrypted-search spectrum.
//! Unlike DCPE (`tests/dcpe.rs`), which lets the server rank ciphertexts and leaks
//! the distance-comparison relation by design, here the server stores only
//! XChaCha20-Poly1305 ciphertext (in the payload, under `__quiver_vec__`) plus a
//! zero placeholder vector, does **no** distance math, and **rejects** a ranked
//! query. The client fetches, decrypts with `quiver_crypto::vector::VectorCipher`,
//! and ranks.
//!
//! Encryption-at-rest is deliberately **off** (`insecure = true`, no
//! `encryption_key`), so the only thing protecting the vectors is the client-side
//! AEAD — not the storage codec. The proof: the plaintext vectors never reach disk,
//! the server cannot rank, and the recovered nearest neighbour is correct.
//!
//! Integration-test helpers are not `#[test]` fns, so the crate's `clippy.toml`
//! unwrap/expect allowance does not reach them; opt in explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use quiver_crypto::vector::VectorCipher;
use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// A 256-bit vector-encryption key, present only on the client — never in the
// server config.
const CLIENT_KEY_HEX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
// A cleartext payload marker the client leaves unencrypted (e.g. to stay
// server-filterable). Finding it on disk is a positive control that the scanner
// works; the sealed vector must NOT be findable.
const MARKER: &str = "PLAINTEXT_MARKER_c5v_clearmark";

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
            if !needle.is_empty() && bytes.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        }
    }
    false
}

// The little-endian byte image of a vector, as the engine stores f32 vectors.
fn vector_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
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
async fn server_stores_opaque_vectors_and_the_client_ranks() {
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
        // At-rest encryption OFF on purpose: isolate the client-side AEAD as the
        // only thing protecting the vectors.
        encryption_key: None,
        tls_cert: None,
        tls_key: None,
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        leader_url: None,
        leader_api_key: None,
        insecure: true,
        limits: quiver_server::Limits::default(),
        embedding: Default::default(),
        rerank: Default::default(),
        rate_limit: Default::default(),
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // A client-side-encrypted collection. The metric is advisory (the server never
    // ranks); the client ranks by L2 below.
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&json!({
            "name": "vault", "dim": 8, "metric": "l2", "vector_encryption": "client_side"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["vector_encryption"], json!("client_side"));

    // The CLIENT holds the key (absent from the server config).
    let cipher = VectorCipher::from_hex(CLIENT_KEY_HEX).unwrap();

    // A distinctive target plus several well-separated decoys.
    let target = [7.13f32, 7.17, 7.19, 7.23, 7.29, 7.31, 7.37, 7.41];
    let mut plaintexts: Vec<(String, [f32; 8])> = vec![("target".to_owned(), target)];
    for k in 0..11 {
        plaintexts.push((format!("decoy{k}"), [20.0 + k as f32; 8]));
    }

    // Upsert opaque points: a zero placeholder vector the server cannot rank, plus
    // the sealed vector blob and a cleartext marker in the payload.
    for (id, plain) in &plaintexts {
        let sealed = cipher.seal(plain).unwrap();
        let mut payload = sealed.as_object().unwrap().clone();
        payload.insert("marker".to_owned(), json!(MARKER));
        let placeholder = vec![0.0f32; 8];
        let resp = http
            .post(format!("{base}/v1/collections/vault/points"))
            .bearer_auth(key)
            .json(&json!({
                "points": [{ "id": id, "vector": placeholder, "payload": payload }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }

    // A ranked query is REJECTED: the server holds only opaque ciphertext and
    // cannot rank it (ADR-0032). This is the enforcement that makes the mode honest.
    let zero_query = vec![0.0f32; 8];
    let resp = http
        .post(format!("{base}/v1/collections/vault/query"))
        .bearer_auth(key)
        .json(&json!({ "vector": zero_query, "k": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "the server must refuse to rank opaque vectors"
    );

    // The client fetches the entitled set (the server returns ciphertext blobs, no
    // ranking), decrypts each locally, and ranks by L2 itself.
    let resp = http
        .post(format!("{base}/v1/collections/vault/fetch"))
        .bearer_auth(key)
        .json(&json!({ "limit": 100 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let points = body["points"].as_array().unwrap();
    assert_eq!(points.len(), 12, "fetch returned the whole entitled set");

    let mut best: Option<(String, f32)> = None;
    for p in points {
        let id = p["id"].as_str().unwrap().to_owned();
        let recovered = cipher
            .open(&p["payload"])
            .expect("client decrypts the blob");
        let d = l2_sq(&recovered, &target);
        if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
            best = Some((id, d));
        }
    }
    assert_eq!(
        best.unwrap().0,
        "target",
        "client-side ranking recovered the true nearest neighbour"
    );

    // The plaintext target vector never reaches disk: only its ciphertext (in the
    // payload) and a zero placeholder were ever sent.
    assert!(
        !tree_contains(tmp.path(), &vector_bytes(&target)),
        "the plaintext vector leaked to disk"
    );
    // Positive control: the cleartext marker IS on disk (at-rest off), proving the
    // scanner works on this database.
    assert!(
        tree_contains(tmp.path(), MARKER.as_bytes()),
        "cleartext payload should be visible on disk with at-rest off (control)"
    );

    server.abort();
}
