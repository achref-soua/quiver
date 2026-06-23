// SPDX-License-Identifier: AGPL-3.0-only
//! DCPE vector encryption end-to-end (ADR-0031): prove that a client can search
//! its embeddings on a server that **never holds the plaintext vectors or the
//! key**. The client DCPE-encrypts vectors with `quiver_crypto::dcpe`, upserts
//! the ciphertexts, and an encrypted query returns the right neighbour
//! (Euclidean distance comparison is preserved) — yet the plaintext vector never
//! appears on disk.
//!
//! Encryption-at-rest is deliberately turned **off** here (`insecure = true`, no
//! `encryption_key`), so the only thing hiding the vectors is the client-side
//! DCPE transform, not the storage codec. This is the vector-side analogue of the
//! payload proof in `client_side_encryption.rs`.
//!
//! DCPE is experimental and intentionally weaker than semantic security: it
//! leaks the approximate distance-comparison relation by design (that is what
//! makes the encrypted search work). This test proves the scoped guarantee —
//! plaintext vectors never reach the server — not secrecy of distances.
//!
//! Integration-test helpers are not `#[test]` fns, so the crate's `clippy.toml`
//! unwrap/expect allowance does not reach them; opt in explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use quiver_crypto::dcpe::DcpeCipher;
use quiver_server::{Config, serve};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// A 256-bit DCPE key, present only on the client — never in the server config.
const CLIENT_KEY_HEX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
// A cleartext payload marker. Payloads are not DCPE-protected (only vectors are),
// so finding this on disk is a positive control that the scanner works.
const MARKER: &str = "PLAINTEXT_MARKER_b2c1a0_clearmark";

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
async fn server_searches_encrypted_vectors_without_seeing_plaintext() {
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
        // At-rest encryption OFF on purpose: isolate the client-side DCPE
        // transform as the only thing protecting the vectors.
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
        otlp: Default::default(),
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // A DCPE-encrypted, L2 collection (the metric DCPE preserves).
    let resp = http
        .post(format!("{base}/v1/collections"))
        .bearer_auth(key)
        .json(&json!({
            "name": "vault", "dim": 8, "metric": "l2", "vector_encryption": "dcpe"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // The CLIENT holds the key (absent from the server config) and a small
    // approximation factor for high recall.
    let cipher = DcpeCipher::from_hex(CLIENT_KEY_HEX, 0.02).unwrap();

    // A distinctive target vector whose plaintext bytes are essentially
    // collision-proof, plus several well-separated decoys.
    let target = [7.13f32, 7.17, 7.19, 7.23, 7.29, 7.31, 7.37, 7.41];
    let mut plaintexts: Vec<(String, [f32; 8])> = vec![("target".to_owned(), target)];
    for k in 0..11 {
        plaintexts.push((format!("decoy{k}"), [20.0 + k as f32; 8]));
    }

    // Upsert only DCPE ciphertexts; the plaintext vectors never leave the client.
    for (id, plain) in &plaintexts {
        let sealed = cipher.encrypt(plain).unwrap();
        let resp = http
            .post(format!("{base}/v1/collections/vault/points"))
            .bearer_auth(key)
            .json(&json!({
                "points": [{ "id": id, "vector": sealed.ciphertext, "payload": {"marker": MARKER} }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }

    // The server answers the nearest-neighbour query over ciphertexts: an
    // encrypted query for the target returns the target (distance preserved).
    let eq = cipher.encrypt_query(&target).unwrap();
    let resp = http
        .post(format!("{base}/v1/collections/vault/query"))
        .bearer_auth(key)
        .json(&json!({ "vector": eq, "k": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "the encrypted query matched a neighbour");
    assert_eq!(
        matches[0]["id"],
        json!("target"),
        "encrypted search preserved the nearest neighbour"
    );

    // The plaintext target vector never reaches disk: only its ciphertext was
    // ever sent, so the engine never holds `s·m`'s preimage.
    assert!(
        !tree_contains(tmp.path(), &vector_bytes(&target)),
        "the plaintext vector leaked to disk"
    );
    // Positive control: the cleartext payload marker IS on disk (payloads are not
    // DCPE-protected), proving the scanner works on this database.
    assert!(
        tree_contains(tmp.path(), MARKER.as_bytes()),
        "cleartext payload should be visible on disk with at-rest off (control)"
    );

    server.abort();
}
