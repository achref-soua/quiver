// SPDX-License-Identifier: AGPL-3.0-only
//! Server-side embedding & rerank over REST end-to-end (ADR-0047), driven by the
//! deterministic `fake` provider so the whole text-in/text-out path runs without a
//! network: `upsert_text` embeds + co-populates `__quiver_text__`, `search_text`
//! embeds the query and fuses dense ⊕ BM25 (so the lexically matching document
//! wins deterministically despite the hash embedder), the opt-in rerank stage runs,
//! and a collection with no configured provider is a clean 400.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::time::Duration;

use quiver_server::{Config, EmbeddingConfig, ProviderKind, RerankConfig, serve};
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

fn fake_embedding(dim: u32) -> EmbeddingConfig {
    EmbeddingConfig {
        provider: ProviderKind::Fake,
        model: String::new(),
        endpoint: String::new(),
        dim,
        api_key_env: String::new(),
    }
}

fn fake_rerank() -> RerankConfig {
    RerankConfig {
        provider: ProviderKind::Fake,
        model: String::new(),
        endpoint: String::new(),
        api_key_env: String::new(),
    }
}

#[tokio::test]
async fn text_ingest_search_and_rerank_over_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest_listener.local_addr().unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();

    // "docs" has an embedding + rerank provider; "plain" has neither.
    let mut embedding = HashMap::new();
    embedding.insert("docs".to_owned(), fake_embedding(8));
    let mut rerank = HashMap::new();
    rerank.insert("docs".to_owned(), fake_rerank());

    let config = Config {
        data_dir: tmp.path().to_path_buf(),
        rest_addr,
        grpc_addr,
        insecure: true,
        embedding,
        rerank,
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });
    let http = reqwest::Client::new();
    let base = format!("http://{rest_addr}");
    wait_ready(&http, &base).await;

    // Collection must match the provider dim (8).
    let create = http
        .post(format!("{base}/v1/collections"))
        .json(&serde_json::json!({"name": "docs", "dim": 8, "metric": "cosine"}))
        .send()
        .await
        .unwrap();
    assert!(create.status().is_success());

    // Ingest by text — no client-side embedding.
    let up = http
        .post(format!("{base}/v1/collections/docs/points:text"))
        .json(&serde_json::json!({"points": [
            {"id": "fox",  "text": "the quick brown fox jumps", "payload": {"src": "a"}},
            {"id": "dog",  "text": "a lazy sleeping dog"},
            {"id": "moon", "text": "the moon orbits the earth"}
        ]}))
        .send()
        .await
        .unwrap();
    assert!(
        up.status().is_success(),
        "upsert_text failed: {}",
        up.status()
    );
    assert_eq!(up.json::<serde_json::Value>().await.unwrap()["upserted"], 3);

    // upsert_text co-populated `__quiver_text__`, so a fetch shows the text and the
    // user payload is preserved.
    let got = http
        .post(format!("{base}/v1/collections/docs/fetch"))
        .json(&serde_json::json!({"limit": 10}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let fox = got["points"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "fox")
        .unwrap();
    assert_eq!(
        fox["payload"]["__quiver_text__"],
        "the quick brown fox jumps"
    );
    assert_eq!(fox["payload"]["src"], "a");

    // Text query: the dense side is hash-noise, but BM25 over the co-populated text
    // ranks the lexically matching document first.
    let q = format!("{base}/v1/collections/docs/query/text");
    let body = http
        .post(&q)
        .json(&serde_json::json!({"text": "quick brown fox", "k": 3}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        body["matches"][0]["id"].as_str().unwrap(),
        "fox",
        "lexical match should rank first; got {body}"
    );

    // Same query with the rerank stage on: still returns the lexical match on top,
    // exercising the retrieve→rerank path in one call.
    let reranked = http
        .post(&q)
        .json(&serde_json::json!({"text": "quick brown fox", "k": 2, "rerank": true}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let ranked: Vec<&str> = reranked["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0], "fox", "reranked top should be the overlap match");

    // A collection with no embedding provider rejects text operations with a 400.
    let create_plain = http
        .post(format!("{base}/v1/collections"))
        .json(&serde_json::json!({"name": "plain", "dim": 8, "metric": "cosine"}))
        .send()
        .await
        .unwrap();
    assert!(create_plain.status().is_success());
    let no_provider = http
        .post(format!("{base}/v1/collections/plain/query/text"))
        .json(&serde_json::json!({"text": "anything", "k": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_provider.status(), 400);
    let no_provider_up = http
        .post(format!("{base}/v1/collections/plain/points:text"))
        .json(&serde_json::json!({"points": [{"id": "x", "text": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_provider_up.status(), 400);

    server.abort();
}
