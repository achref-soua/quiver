// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in, provider-agnostic embedding & reranking adapters (ADR-0047/0058).
//!
//! The Quiver engine is deliberately model-agnostic: it stores and searches
//! float vectors and knows nothing about embedding models. This crate is the
//! **edge** adapter that lets an operator turn *"give me text"* into a
//! stored/searched vector without the client running an embedding model — the
//! single biggest RAG friction. It lives in its own lean crate (no axum/tonic)
//! so it can be shared by both the network server (`quiver-server`) and the
//! in-process MCP server (`quiver-mcp`) without either pulling the other's
//! dependency tree (ADR-0058); it is never used by `quiver-core` or the
//! `quiver-embed` engine crate, so library-mode users pay nothing.
//!
//! ## Design (ADR-0047)
//! - **Provider-agnostic.** [`EmbeddingProvider`] / [`RerankProvider`] are traits;
//!   OpenAI-compatible servers (OpenAI, Ollama's `/v1` endpoint, vLLM, LM Studio,
//!   llama.cpp, …) share one HTTP adapter parameterized by base URL + auth, Cohere
//!   has its own shape, and a deterministic [`FakeEmbedder`]/[`FakeReranker`] backs
//!   tests and the acceptance script. No vendor is hard-coded; selection is config.
//! - **Opt-in, per collection, default off.** Configured in the **server config**
//!   (`[embedding.<collection>]` / `[rerank.<collection>]`), not the on-disk
//!   descriptor — so the engine and the crash gate are untouched.
//! - **No secrets on disk.** Config stores the *name* of an environment variable
//!   ([`EmbeddingConfig::api_key_env`]); the value is resolved at registry-build
//!   time and never persisted.
//!
//! ## Testing honesty
//! The pure request-build and response-parse functions are unit-tested, and the
//! `fake` provider exercises the full text-in/text-out path. The methods that make
//! a live HTTP call ([`OpenAiCompatEmbedder::embed`], [`CohereEmbedder::embed`],
//! [`CohereReranker::rerank`]) are thin shells around those tested helpers and a
//! `ureq` call; live network calls are **not** in CI (stated, not faked).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use figment::{
    Figment,
    providers::{Format, Toml},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// A timeout for any single provider HTTP call. Embedding/reranking is a
/// best-effort convenience; a slow provider must not pin a server thread forever.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);

/// An error from a provider call or its configuration.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The HTTP request to the provider failed (transport, status, or timeout).
    #[error("embedding provider request failed: {0}")]
    Http(String),
    /// The provider returned a body we could not parse into embeddings/scores.
    #[error("embedding provider returned a malformed response: {0}")]
    Parse(String),
    /// A configured `api_key_env` variable is not set in the environment.
    #[error("api key environment variable {0} is not set")]
    MissingKey(String),
    /// The configuration named a provider/endpoint combination we cannot build
    /// (e.g. `http`/`ollama` without an `endpoint`).
    #[error("invalid embedding configuration: {0}")]
    Config(String),
}

/// Which provider backs a collection's embedding (or rerank). The OpenAI-compatible
/// trio (`openai`, `ollama`, `http`) share one adapter; `cohere` is its own; `fake`
/// is deterministic and for tests/acceptance only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI `/v1/embeddings` (Bearer auth).
    Openai,
    /// An Ollama server's OpenAI-compatible `/v1/embeddings` (usually no auth).
    Ollama,
    /// Any OpenAI-compatible server at an explicit `endpoint` (vLLM, LM Studio, …).
    Http,
    /// Cohere `/v2/embed` and `/v2/rerank`.
    Cohere,
    /// A deterministic, network-free hash embedder/reranker (tests/acceptance).
    Fake,
}

/// A collection's embedding configuration (server config table
/// `[embedding.<collection>]`). Secrets are referenced by env-var *name* only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// The provider backing this collection.
    pub provider: ProviderKind,
    /// The model id passed to the provider (ignored by `fake`).
    #[serde(default)]
    pub model: String,
    /// Base URL override; required for `http`/`ollama`, optional for `openai`/`cohere`
    /// (which default to their public endpoints).
    #[serde(default)]
    pub endpoint: String,
    /// The collection's vector dimension; the embedder must return this many floats.
    pub dim: u32,
    /// The *name* of the environment variable holding the API key (resolved at
    /// call time; never persisted). Empty ⇒ no auth header (e.g. local Ollama).
    #[serde(default)]
    pub api_key_env: String,
}

/// A collection's rerank configuration (server config table `[rerank.<collection>]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankConfig {
    /// The provider backing rerank (`cohere` or `fake`).
    pub provider: ProviderKind,
    /// The rerank model id (ignored by `fake`).
    #[serde(default)]
    pub model: String,
    /// Base URL override (defaults to the provider's public endpoint).
    #[serde(default)]
    pub endpoint: String,
    /// The *name* of the environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: String,
}

/// Embeds a batch of texts into dense vectors (one per input).
pub trait EmbeddingProvider: Send + Sync {
    /// Embed `texts`, returning one `dim`-length vector per input, in order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError>;
    /// The dimensionality every returned vector must have.
    fn dim(&self) -> usize;
}

/// Scores `(query, document)` pairs for relevance; higher is more relevant.
pub trait RerankProvider: Send + Sync {
    /// Return one relevance score per `doc`, in the input order.
    fn rerank(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, ProviderError>;
}

// ---------------------------------------------------------------------------
// Fake provider (deterministic, network-free) — tests & the acceptance script.
// ---------------------------------------------------------------------------

/// A deterministic embedder that hashes text into a unit-ish vector. Never used
/// in production config paths beyond the explicit `fake` selection; it exists so
/// the whole text-in/text-out path is testable without a network.
pub struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    /// A fake embedder producing `dim`-length vectors.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

/// FNV-1a over bytes — the same stable hash the tokenizer uses, kept local so this
/// module has no cross-crate coupling for a one-liner.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl EmbeddingProvider for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        Ok(texts
            .iter()
            .map(|t| {
                // Per-dimension hash of (text, i) → a stable, content-dependent
                // vector. Deterministic so tests can assert identical-text →
                // identical-vector and different-text → different-vector.
                (0..self.dim)
                    .map(|i| {
                        let h = fnv1a(format!("{t}:{i}").as_bytes());
                        // Map the high bits into [-1, 1).
                        (h >> 40) as f32 / f32::from(1u16 << 11) - 1.0
                    })
                    .collect()
            })
            .collect())
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

/// A deterministic reranker scoring documents by lexical token overlap with the
/// query. Network-free; backs tests and the acceptance script.
pub struct FakeReranker;

impl RerankProvider for FakeReranker {
    fn rerank(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, ProviderError> {
        let q: std::collections::HashSet<String> =
            query.split_whitespace().map(|w| w.to_lowercase()).collect();
        Ok(docs
            .iter()
            .map(|d| {
                let overlap = d
                    .split_whitespace()
                    .filter(|w| q.contains(&w.to_lowercase()))
                    .count();
                overlap as f32
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible embedder (OpenAI / Ollama / any OpenAI-shaped server).
// ---------------------------------------------------------------------------

/// An embedder that speaks the OpenAI `/v1/embeddings` request/response shape.
pub struct OpenAiCompatEmbedder {
    url: String,
    model: String,
    api_key: Option<String>,
    dim: usize,
}

/// Build the OpenAI `/v1/embeddings` request body. Pure (unit-tested).
fn openai_body(model: &str, texts: &[String]) -> Value {
    json!({ "model": model, "input": texts })
}

/// Parse an OpenAI `/v1/embeddings` response into vectors (order preserved). Pure.
fn parse_openai(body: &Value) -> Result<Vec<Vec<f32>>, ProviderError> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("missing `data` array".into()))?;
    data.iter()
        .map(|row| {
            row.get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(|| ProviderError::Parse("a `data` row had no `embedding` array".into()))
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                        .collect()
                })
        })
        .collect()
}

impl EmbeddingProvider for OpenAiCompatEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        // Live HTTP shell around the tested `openai_body` / `parse_openai` helpers
        // (not exercised in CI; the `fake` provider covers the path).
        let mut req = ureq::post(&self.url).timeout(PROVIDER_TIMEOUT);
        if let Some(key) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }
        let resp = req
            .send_json(openai_body(&self.model, texts))
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        let body: Value = resp
            .into_json()
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        let vectors = parse_openai(&body)?;
        check_dims(&vectors, self.dim)?;
        Ok(vectors)
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// Cohere embedder + reranker.
// ---------------------------------------------------------------------------

/// An embedder that speaks Cohere `/v2/embed`.
pub struct CohereEmbedder {
    url: String,
    model: String,
    api_key: String,
    dim: usize,
}

/// Build the Cohere `/v2/embed` request body. Pure (unit-tested).
fn cohere_embed_body(model: &str, texts: &[String]) -> Value {
    json!({
        "model": model,
        "texts": texts,
        "input_type": "search_document",
        "embedding_types": ["float"],
    })
}

/// Parse a Cohere `/v2/embed` response (`{"embeddings":{"float":[[...]]}}`). Pure.
fn parse_cohere_embed(body: &Value) -> Result<Vec<Vec<f32>>, ProviderError> {
    let floats = body
        .get("embeddings")
        .and_then(|e| e.get("float"))
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("missing `embeddings.float` array".into()))?;
    Ok(floats
        .iter()
        .map(|row| {
            row.as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect())
}

impl EmbeddingProvider for CohereEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        let resp = ureq::post(&self.url)
            .timeout(PROVIDER_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(cohere_embed_body(&self.model, texts))
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        let body: Value = resp
            .into_json()
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        let vectors = parse_cohere_embed(&body)?;
        check_dims(&vectors, self.dim)?;
        Ok(vectors)
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

/// A reranker that speaks Cohere `/v2/rerank`.
pub struct CohereReranker {
    url: String,
    model: String,
    api_key: String,
}

/// Build the Cohere `/v2/rerank` request body. Pure (unit-tested).
fn cohere_rerank_body(model: &str, query: &str, docs: &[String]) -> Value {
    json!({ "model": model, "query": query, "documents": docs })
}

/// Parse a Cohere `/v2/rerank` response into a score *per input document*, in the
/// original order. Cohere returns `{"results":[{"index":i,"relevance_score":s}]}`
/// (possibly truncated/reordered), so we scatter by `index`. Pure (unit-tested).
fn parse_cohere_rerank(body: &Value, n_docs: usize) -> Result<Vec<f32>, ProviderError> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("missing `results` array".into()))?;
    let mut scores = vec![0.0_f32; n_docs];
    for r in results {
        let idx = r
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::Parse("a result had no `index`".into()))?
            as usize;
        let score = r
            .get("relevance_score")
            .and_then(Value::as_f64)
            .ok_or_else(|| ProviderError::Parse("a result had no `relevance_score`".into()))?
            as f32;
        if idx < n_docs {
            scores[idx] = score;
        }
    }
    Ok(scores)
}

impl RerankProvider for CohereReranker {
    fn rerank(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, ProviderError> {
        let resp = ureq::post(&self.url)
            .timeout(PROVIDER_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(cohere_rerank_body(&self.model, query, docs))
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        let body: Value = resp
            .into_json()
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        parse_cohere_rerank(&body, docs.len())
    }
}

/// Reject a provider response whose vectors do not all match the collection's dim
/// (a misconfigured model is a clear error, not a silent wrong-length insert).
fn check_dims(vectors: &[Vec<f32>], dim: usize) -> Result<(), ProviderError> {
    if let Some(bad) = vectors.iter().find(|v| v.len() != dim) {
        return Err(ProviderError::Parse(format!(
            "provider returned a {}-dim vector but the collection expects {dim}",
            bad.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registry: build the per-collection providers from config (resolves secrets).
// ---------------------------------------------------------------------------

/// The default base URLs for the public providers.
const OPENAI_DEFAULT: &str = "https://api.openai.com/v1/embeddings";
const COHERE_EMBED_DEFAULT: &str = "https://api.cohere.com/v2/embed";
const COHERE_RERANK_DEFAULT: &str = "https://api.cohere.com/v2/rerank";

/// Resolve an `api_key_env` name to its value, or `None` when the name is empty.
fn resolve_key(api_key_env: &str) -> Result<Option<String>, ProviderError> {
    if api_key_env.is_empty() {
        return Ok(None);
    }
    std::env::var(api_key_env)
        .map(Some)
        .map_err(|_| ProviderError::MissingKey(api_key_env.to_owned()))
}

/// The `[embedding.*]` and `[rerank.*]` tables of a Quiver config file, used by
/// [`EmbedRegistry::from_toml_path`]. Every other config key is ignored.
#[derive(Debug, Default, Deserialize)]
struct ProviderTables {
    #[serde(default)]
    embedding: HashMap<String, EmbeddingConfig>,
    #[serde(default)]
    rerank: HashMap<String, RerankConfig>,
}

/// Per-collection embedding/rerank providers, built once at startup from config.
#[derive(Clone, Default)]
pub struct EmbedRegistry {
    embedders: HashMap<String, Arc<dyn EmbeddingProvider>>,
    rerankers: HashMap<String, Arc<dyn RerankProvider>>,
}

impl EmbedRegistry {
    /// Build the registry from the server config's `embedding`/`rerank` tables,
    /// resolving each `api_key_env` from the environment (a missing required key is
    /// a hard startup error, surfacing misconfiguration immediately).
    pub fn from_config(
        embedding: &HashMap<String, EmbeddingConfig>,
        rerank: &HashMap<String, RerankConfig>,
    ) -> Result<Self, ProviderError> {
        let mut embedders: HashMap<String, Arc<dyn EmbeddingProvider>> = HashMap::new();
        for (collection, cfg) in embedding {
            embedders.insert(collection.clone(), build_embedder(cfg)?);
        }
        let mut rerankers: HashMap<String, Arc<dyn RerankProvider>> = HashMap::new();
        for (collection, cfg) in rerank {
            rerankers.insert(collection.clone(), build_reranker(cfg)?);
        }
        Ok(Self {
            embedders,
            rerankers,
        })
    }

    /// Build a registry from the `[embedding.*]` / `[rerank.*]` tables of a Quiver
    /// TOML config file — the same tables `quiver serve` reads — so the MCP server
    /// (`quiver mcp`) can offer text-in/text-out tools with the same configuration
    /// surface as the network server (ADR-0058). Any other config keys are ignored.
    ///
    /// A missing file yields an *empty* registry rather than an error: the MCP
    /// server still starts, and the text tools report "no embedding provider
    /// configured" only when actually invoked. A malformed file, or a provider that
    /// cannot be built (e.g. a missing required API key), is a hard error.
    pub fn from_toml_path(path: &Path) -> Result<Self, ProviderError> {
        let tables: ProviderTables = Figment::from(Toml::file(path))
            .extract()
            .map_err(|e| ProviderError::Config(e.to_string()))?;
        Self::from_config(&tables.embedding, &tables.rerank)
    }

    /// The embedder configured for `collection`, if any.
    #[must_use]
    pub fn embedder(&self, collection: &str) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedders.get(collection).cloned()
    }

    /// The reranker configured for `collection`, if any.
    #[must_use]
    pub fn reranker(&self, collection: &str) -> Option<Arc<dyn RerankProvider>> {
        self.rerankers.get(collection).cloned()
    }

    /// Whether any embedding or rerank provider is configured (so callers can skip
    /// per-request work entirely on the common no-providers path).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.embedders.is_empty() && self.rerankers.is_empty()
    }
}

/// Build one embedder from its config, resolving the API key from the environment.
fn build_embedder(cfg: &EmbeddingConfig) -> Result<Arc<dyn EmbeddingProvider>, ProviderError> {
    let dim = cfg.dim as usize;
    match cfg.provider {
        ProviderKind::Fake => Ok(Arc::new(FakeEmbedder::new(dim))),
        ProviderKind::Openai => Ok(Arc::new(OpenAiCompatEmbedder {
            url: if cfg.endpoint.is_empty() {
                OPENAI_DEFAULT.to_owned()
            } else {
                cfg.endpoint.clone()
            },
            model: cfg.model.clone(),
            api_key: resolve_key(&cfg.api_key_env)?,
            dim,
        })),
        ProviderKind::Ollama | ProviderKind::Http => {
            if cfg.endpoint.is_empty() {
                return Err(ProviderError::Config(format!(
                    "provider {:?} requires an `endpoint` (e.g. http://localhost:11434/v1/embeddings)",
                    cfg.provider
                )));
            }
            Ok(Arc::new(OpenAiCompatEmbedder {
                url: cfg.endpoint.clone(),
                model: cfg.model.clone(),
                api_key: resolve_key(&cfg.api_key_env)?,
                dim,
            }))
        }
        ProviderKind::Cohere => Ok(Arc::new(CohereEmbedder {
            url: if cfg.endpoint.is_empty() {
                COHERE_EMBED_DEFAULT.to_owned()
            } else {
                cfg.endpoint.clone()
            },
            model: cfg.model.clone(),
            api_key: resolve_key(&cfg.api_key_env)?.ok_or_else(|| {
                ProviderError::Config("cohere embedding requires api_key_env".into())
            })?,
            dim,
        })),
    }
}

/// Build one reranker from its config (only `cohere` and `fake` are supported).
fn build_reranker(cfg: &RerankConfig) -> Result<Arc<dyn RerankProvider>, ProviderError> {
    match cfg.provider {
        ProviderKind::Fake => Ok(Arc::new(FakeReranker)),
        ProviderKind::Cohere => Ok(Arc::new(CohereReranker {
            url: if cfg.endpoint.is_empty() {
                COHERE_RERANK_DEFAULT.to_owned()
            } else {
                cfg.endpoint.clone()
            },
            model: cfg.model.clone(),
            api_key: resolve_key(&cfg.api_key_env)?.ok_or_else(|| {
                ProviderError::Config("cohere rerank requires api_key_env".into())
            })?,
        })),
        other => Err(ProviderError::Config(format!(
            "rerank provider {other:?} is not supported (use `cohere` or `fake`)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embedder_is_deterministic_and_content_dependent() {
        let e = FakeEmbedder::new(8);
        let a = e.embed(&["hello world".into()]).unwrap();
        let b = e.embed(&["hello world".into()]).unwrap();
        let c = e.embed(&["different text".into()]).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].len(), 8);
        assert_eq!(a, b, "identical text → identical vector");
        assert_ne!(a, c, "different text → different vector");
        assert_eq!(e.dim(), 8);
    }

    #[test]
    fn fake_embedder_batches_in_order() {
        let e = FakeEmbedder::new(4);
        let batch = e.embed(&["a".into(), "b".into()]).unwrap();
        let a = e.embed(&["a".into()]).unwrap();
        let b = e.embed(&["b".into()]).unwrap();
        assert_eq!(batch[0], a[0]);
        assert_eq!(batch[1], b[0]);
    }

    #[test]
    fn fake_reranker_scores_by_overlap() {
        let r = FakeReranker;
        let scores = r
            .rerank(
                "quick brown fox",
                &[
                    "the quick brown fox".into(),
                    "lazy dog".into(),
                    "fox".into(),
                ],
            )
            .unwrap();
        assert_eq!(scores, vec![3.0, 0.0, 1.0]);
    }

    #[test]
    fn openai_body_and_parse_roundtrip() {
        let body = openai_body("text-embedding-3-small", &["hi".into(), "yo".into()]);
        assert_eq!(body["model"], "text-embedding-3-small");
        assert_eq!(body["input"][1], "yo");
        let resp = json!({"data":[{"embedding":[0.1,0.2]},{"embedding":[0.3,0.4]}]});
        let vecs = parse_openai(&resp).unwrap();
        assert_eq!(vecs, vec![vec![0.1_f32, 0.2], vec![0.3, 0.4]]);
    }

    #[test]
    fn parse_openai_rejects_malformed() {
        assert!(parse_openai(&json!({"oops": 1})).is_err());
        assert!(parse_openai(&json!({"data":[{"no_embedding": 1}]})).is_err());
    }

    #[test]
    fn cohere_embed_body_and_parse_roundtrip() {
        let body = cohere_embed_body("embed-v4.0", &["doc".into()]);
        assert_eq!(body["model"], "embed-v4.0");
        assert_eq!(body["input_type"], "search_document");
        assert_eq!(body["texts"][0], "doc");
        let resp = json!({"embeddings":{"float":[[1.0,2.0,3.0]]}});
        assert_eq!(
            parse_cohere_embed(&resp).unwrap(),
            vec![vec![1.0_f32, 2.0, 3.0]]
        );
        assert!(parse_cohere_embed(&json!({"embeddings":{}})).is_err());
    }

    #[test]
    fn cohere_rerank_scatters_by_index() {
        let body = cohere_rerank_body("rerank-v3.5", "q", &["a".into(), "b".into()]);
        assert_eq!(body["query"], "q");
        // Cohere may reorder and reference docs by their input index.
        let resp = json!({"results":[
            {"index":1,"relevance_score":0.9},
            {"index":0,"relevance_score":0.1},
        ]});
        assert_eq!(parse_cohere_rerank(&resp, 2).unwrap(), vec![0.1_f32, 0.9]);
        // Out-of-range indices are ignored, missing fields error.
        assert_eq!(
            parse_cohere_rerank(&json!({"results":[{"index":9,"relevance_score":1.0}]}), 2)
                .unwrap(),
            vec![0.0_f32, 0.0]
        );
        assert!(parse_cohere_rerank(&json!({"nope":1}), 2).is_err());
        assert!(parse_cohere_rerank(&json!({"results":[{"index":0}]}), 1).is_err());
    }

    #[test]
    fn check_dims_enforces_collection_dim() {
        assert!(check_dims(&[vec![1.0, 2.0]], 2).is_ok());
        assert!(check_dims(&[vec![1.0, 2.0, 3.0]], 2).is_err());
    }

    #[test]
    fn registry_builds_fake_and_resolves_emptiness() {
        let mut embedding = HashMap::new();
        embedding.insert(
            "docs".to_owned(),
            EmbeddingConfig {
                provider: ProviderKind::Fake,
                model: String::new(),
                endpoint: String::new(),
                dim: 16,
                api_key_env: String::new(),
            },
        );
        let mut rerank = HashMap::new();
        rerank.insert(
            "docs".to_owned(),
            RerankConfig {
                provider: ProviderKind::Fake,
                model: String::new(),
                endpoint: String::new(),
                api_key_env: String::new(),
            },
        );
        let reg = EmbedRegistry::from_config(&embedding, &rerank).unwrap();
        assert!(!reg.is_empty());
        assert_eq!(reg.embedder("docs").unwrap().dim(), 16);
        assert!(reg.embedder("missing").is_none());
        assert!(reg.reranker("docs").is_some());
        assert!(EmbedRegistry::default().is_empty());
    }

    #[test]
    fn from_toml_path_loads_embedding_and_rerank_tables() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quiver.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        // A fake provider needs no network/keys, so the loaded registry is usable
        // in-process. Unrelated tables (here `[server]`) must be ignored.
        writeln!(
            f,
            r#"
[server]
host = "127.0.0.1"

[embedding.docs]
provider = "fake"
dim = 16

[rerank.docs]
provider = "fake"
"#
        )
        .unwrap();
        let reg = EmbedRegistry::from_toml_path(&path).unwrap();
        assert_eq!(reg.embedder("docs").unwrap().dim(), 16);
        assert!(reg.reranker("docs").is_some());
        assert!(reg.embedder("missing").is_none());
    }

    #[test]
    fn from_toml_path_missing_file_is_empty_not_an_error() {
        let reg = EmbedRegistry::from_toml_path(Path::new("definitely-not-here.toml")).unwrap();
        assert!(reg.is_empty());
    }

    #[test]
    fn from_toml_path_propagates_a_misconfigured_provider() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quiver.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        // `http` requires an `endpoint`; omitting it is a hard configuration error.
        writeln!(
            f,
            r#"
[embedding.docs]
provider = "http"
dim = 8
"#
        )
        .unwrap();
        assert!(matches!(
            EmbedRegistry::from_toml_path(&path),
            Err(ProviderError::Config(_))
        ));
    }

    #[test]
    fn http_provider_requires_endpoint() {
        let cfg = EmbeddingConfig {
            provider: ProviderKind::Http,
            model: "m".into(),
            endpoint: String::new(),
            dim: 4,
            api_key_env: String::new(),
        };
        assert!(matches!(
            build_embedder(&cfg),
            Err(ProviderError::Config(_))
        ));
    }

    #[test]
    fn missing_api_key_is_a_hard_error() {
        let cfg = EmbeddingConfig {
            provider: ProviderKind::Openai,
            model: "m".into(),
            endpoint: String::new(),
            dim: 4,
            api_key_env: "QUIVER_TEST_DEFINITELY_UNSET_KEY".into(),
        };
        assert!(matches!(
            build_embedder(&cfg),
            Err(ProviderError::MissingKey(_))
        ));
    }

    #[test]
    fn openai_endpoint_defaults_and_overrides() {
        // Default endpoint when unset, no key required.
        let def = EmbeddingConfig {
            provider: ProviderKind::Openai,
            model: "m".into(),
            endpoint: String::new(),
            dim: 4,
            api_key_env: String::new(),
        };
        assert!(build_embedder(&def).is_ok());
        // Cohere without a key is rejected (rerank too).
        let cohere = EmbeddingConfig {
            provider: ProviderKind::Cohere,
            model: "m".into(),
            endpoint: String::new(),
            dim: 4,
            api_key_env: String::new(),
        };
        assert!(matches!(
            build_embedder(&cohere),
            Err(ProviderError::Config(_))
        ));
        let rr = RerankConfig {
            provider: ProviderKind::Openai,
            model: "m".into(),
            endpoint: String::new(),
            api_key_env: String::new(),
        };
        assert!(matches!(build_reranker(&rr), Err(ProviderError::Config(_))));
    }
}
