// SPDX-License-Identifier: AGPL-3.0-only
//! The Quiver daemon: gRPC and REST over the embeddable [`Database`], with
//! API-key auth and secure-by-default configuration.
//!
//! Both transports are thin shells over the same shared engine operations; the
//! engine is synchronous and CPU/`fsync`-bound, so every
//! call is offloaded with `spawn_blocking` and serialized behind a single mutex
//! (ADR-0002, single-writer per ADR-0006). The lock-free MVCC read path is
//! Phase 2.
//!
//! Phase 1 auth is a configured API key (Bearer / gRPC `authorization`
//! metadata), default-deny, with a fail-fast secure config (ADR-0011/0013).
//! Encryption-at-rest is on by default (ADR-0010): unless `insecure` is set, an
//! `encryption_key` is required and the engine is opened through
//! `quiver-crypto`'s AEAD codec. TLS, RBAC scopes, multi-tenancy, audit logging,
//! and rate limiting are later phases. Design: `docs/api/rest-grpc.md`.

mod error;
mod grpc;
mod rest;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

use quiver_crypto::AeadCodec;
use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams};
use quiver_query::Filter;

pub use error::Error;

/// Server configuration, layered defaults → `quiver.toml` → `QUIVER_*` env and
/// validated at startup (ADR-0013).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Data directory for the storage engine.
    pub data_dir: PathBuf,
    /// REST (HTTP/1.1) bind address.
    pub rest_addr: SocketAddr,
    /// gRPC (HTTP/2) bind address.
    pub grpc_addr: SocketAddr,
    /// Accepted API keys. Empty is allowed only with `insecure = true`.
    pub api_keys: Vec<String>,
    /// Hex-encoded 256-bit key for encryption-at-rest (64 hex characters).
    /// Required unless `insecure = true`; source it from the environment or a
    /// secret store, never the committed config. `None` ⇒ data is stored
    /// unencrypted (only valid in `insecure` mode).
    pub encryption_key: Option<String>,
    /// Opt out of the secure defaults (no auth, no encryption-at-rest, allow a
    /// non-loopback bind without TLS). For local development only; never the
    /// default.
    pub insecure: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./quiver-data"),
            rest_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6333),
            grpc_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6334),
            api_keys: Vec::new(),
            encryption_key: None,
            insecure: false,
        }
    }
}

impl Config {
    /// Load configuration from defaults, an optional `quiver.toml`, and
    /// `QUIVER_*` environment variables.
    pub fn load() -> Result<Self, Error> {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file("quiver.toml"))
            .merge(Env::prefixed("QUIVER_"))
            .extract()
            .map_err(|e| Error::Config(e.to_string()))
    }

    /// Reject insecure configurations unless explicitly opted out (ADR-0013):
    /// no anonymous access, encryption-at-rest on by default with a valid key,
    /// and no non-loopback bind without TLS (TLS lands in the next slice).
    pub fn validate(&self) -> Result<(), Error> {
        if self.api_keys.is_empty() && !self.insecure {
            return Err(Error::Config(
                "no api_keys configured: set QUIVER_API_KEYS (comma-separated) or \
                 set insecure=true for local development"
                    .to_owned(),
            ));
        }
        if self.encryption_key.is_none() && !self.insecure {
            return Err(Error::Config(
                "no encryption_key configured: encryption-at-rest is on by default — \
                 set QUIVER_ENCRYPTION_KEY to a 64-hex-character (256-bit) key, or set \
                 insecure=true to store data unencrypted (development only)"
                    .to_owned(),
            ));
        }
        // Fail fast on a malformed key rather than at first write.
        if let Some(key) = &self.encryption_key {
            AeadCodec::from_hex(key)
                .map_err(|e| Error::Config(format!("invalid encryption_key: {e}")))?;
        }
        let non_loopback = !self.rest_addr.ip().is_loopback() || !self.grpc_addr.ip().is_loopback();
        if non_loopback && !self.insecure {
            return Err(Error::Config(
                "non-loopback bind requires insecure=true until TLS lands with \
                 encryption-at-rest"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// Shared server state: the engine behind a single-writer lock, plus the
/// accepted API keys.
#[derive(Clone)]
pub(crate) struct AppState {
    db: Arc<Mutex<Database>>,
    api_keys: Arc<Vec<String>>,
}

/// A collection's metadata.
pub(crate) struct CollectionInfo {
    pub name: String,
    pub dim: u32,
    pub metric: DistanceMetric,
    pub count: u64,
}

/// A point to upsert.
pub(crate) struct PointIn {
    pub id: String,
    pub vector: Vec<f32>,
    pub payload: Value,
}

/// A fetched point.
pub(crate) struct PointOut {
    pub id: String,
    pub vector: Option<Vec<f32>>,
    pub payload: Value,
}

/// A search hit.
pub(crate) struct MatchOut {
    pub id: String,
    pub score: f32,
    pub payload: Option<Value>,
    pub vector: Option<Vec<f32>>,
}

impl AppState {
    /// Whether a presented bearer token is accepted. An empty key set means
    /// `insecure` mode (validated at startup), which accepts any caller.
    pub(crate) fn authorized(&self, presented: Option<&str>) -> bool {
        if self.api_keys.is_empty() {
            return true;
        }
        match presented {
            Some(token) => self
                .api_keys
                .iter()
                .any(|key| constant_time_eq(key.as_bytes(), token.as_bytes())),
            None => false,
        }
    }

    async fn run_blocking<T, F>(&self, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Database) -> quiver_embed::Result<T> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || -> Result<T, Error> {
            let mut guard = db
                .lock()
                .map_err(|_| Error::Internal("database lock poisoned".to_owned()))?;
            f(&mut guard).map_err(Error::Engine)
        })
        .await
        .map_err(|e| Error::Internal(format!("blocking task failed: {e}")))?
    }

    pub(crate) async fn create_collection(
        &self,
        name: String,
        dim: u32,
        metric: DistanceMetric,
    ) -> Result<CollectionInfo, Error> {
        let descriptor = Descriptor {
            dim,
            dtype: Dtype::F32,
            metric,
        };
        let owned = name.clone();
        self.run_blocking(move |db| db.create_collection(&owned, descriptor))
            .await?;
        Ok(CollectionInfo {
            name,
            dim,
            metric,
            count: 0,
        })
    }

    pub(crate) async fn get_collection(&self, name: String) -> Result<CollectionInfo, Error> {
        self.run_blocking(move |db| {
            let descriptor = db
                .descriptor(&name)
                .cloned()
                .ok_or_else(|| quiver_embed::Error::CollectionNotFound(name.clone()))?;
            let count = db.len(&name)? as u64;
            Ok(CollectionInfo {
                name,
                dim: descriptor.dim,
                metric: descriptor.metric,
                count,
            })
        })
        .await
    }

    pub(crate) async fn list_collections(&self) -> Result<Vec<CollectionInfo>, Error> {
        self.run_blocking(|db| {
            let mut out = Vec::new();
            for name in db.collection_names() {
                if let Some(descriptor) = db.descriptor(&name).cloned() {
                    let count = db.len(&name)? as u64;
                    out.push(CollectionInfo {
                        name,
                        dim: descriptor.dim,
                        metric: descriptor.metric,
                        count,
                    });
                }
            }
            Ok(out)
        })
        .await
    }

    pub(crate) async fn delete_collection(&self, name: String) -> Result<bool, Error> {
        self.run_blocking(move |db| db.drop_collection(&name)).await
    }

    pub(crate) async fn upsert(
        &self,
        collection: String,
        points: Vec<PointIn>,
    ) -> Result<u64, Error> {
        self.run_blocking(move |db| {
            let mut count = 0u64;
            for point in &points {
                db.upsert(&collection, &point.id, &point.vector, &point.payload)?;
                count += 1;
            }
            Ok(count)
        })
        .await
    }

    pub(crate) async fn delete_points(
        &self,
        collection: String,
        ids: Vec<String>,
    ) -> Result<u64, Error> {
        self.run_blocking(move |db| {
            let mut count = 0u64;
            for id in &ids {
                if db.delete(&collection, id)? {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await
    }

    pub(crate) async fn get_points(
        &self,
        collection: String,
        ids: Vec<String>,
        with_vector: bool,
    ) -> Result<Vec<PointOut>, Error> {
        self.run_blocking(move |db| {
            let mut out = Vec::new();
            for id in &ids {
                if let Some(m) = db.get(&collection, id)? {
                    out.push(PointOut {
                        id: m.id,
                        vector: if with_vector { m.vector } else { None },
                        payload: m.payload.unwrap_or(Value::Null),
                    });
                }
            }
            Ok(out)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn search(
        &self,
        collection: String,
        vector: Vec<f32>,
        k: usize,
        filter: Option<Filter>,
        ef_search: usize,
        with_payload: bool,
        with_vector: bool,
    ) -> Result<Vec<MatchOut>, Error> {
        self.run_blocking(move |db| {
            let params = SearchParams {
                k,
                filter,
                ef_search,
                with_payload,
                with_vector,
            };
            let matches = db.search(&collection, &vector, &params)?;
            Ok(matches
                .into_iter()
                .map(|m| MatchOut {
                    id: m.id,
                    score: m.score,
                    payload: m.payload,
                    vector: m.vector,
                })
                .collect())
        })
        .await
    }
}

/// Run the server from `config` until a shutdown signal (Ctrl-C).
pub async fn run(config: Config) -> Result<(), Error> {
    config.validate()?;
    let rest_listener = TcpListener::bind(config.rest_addr)
        .await
        .map_err(Error::Io)?;
    let grpc_listener = TcpListener::bind(config.grpc_addr)
        .await
        .map_err(Error::Io)?;
    tracing::info!(rest = %config.rest_addr, grpc = %config.grpc_addr, "quiver listening");
    tokio::select! {
        result = serve(config, rest_listener, grpc_listener) => result,
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            Ok(())
        }
    }
}

/// Serve REST and gRPC on the given (already-bound) listeners until a transport
/// error. Exposed so tests can bind ephemeral ports.
pub async fn serve(
    config: Config,
    rest_listener: TcpListener,
    grpc_listener: TcpListener,
) -> Result<(), Error> {
    let db = open_database(&config)?;
    let state = AppState {
        db: Arc::new(Mutex::new(db)),
        api_keys: Arc::new(config.api_keys.clone()),
    };

    let app = rest::router(state.clone());
    let grpc = grpc::service(state);

    let rest_fut = async move { axum::serve(rest_listener, app).await.map_err(Error::Io) };
    let grpc_fut = async move {
        tonic::transport::Server::builder()
            .add_service(grpc)
            .serve_with_incoming(TcpListenerStream::new(grpc_listener))
            .await
            .map_err(|e| Error::Internal(format!("grpc server: {e}")))
    };

    tokio::try_join!(rest_fut, grpc_fut)?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// Open the engine, enabling encryption-at-rest when a key is configured. With no
// key (only valid in `insecure` mode, enforced by `Config::validate`) the engine
// is opened in plaintext.
fn open_database(config: &Config) -> Result<Database, Error> {
    match &config.encryption_key {
        Some(key) => {
            let codec = AeadCodec::from_hex(key)
                .map_err(|e| Error::Config(format!("invalid encryption_key: {e}")))?;
            Ok(Database::open_with_codec(
                &config.data_dir,
                Box::new(codec),
            )?)
        }
        None => Ok(Database::open(&config.data_dir)?),
    }
}

/// Initialize structured logging from `RUST_LOG` (defaulting to `info`). Safe to
/// call once at startup; a second call is ignored.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

// Length-checked constant-time byte comparison for API keys.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid 64-hex-character (256-bit) test key.
    const TEST_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn config_rejects_missing_keys_unless_insecure() {
        let mut config = Config::default();
        assert!(config.validate().is_err());
        config.insecure = true;
        assert!(config.validate().is_ok());
        config.insecure = false;
        config.api_keys = vec!["secret".to_owned()];
        config.encryption_key = Some(TEST_KEY.to_owned());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_requires_encryption_key_unless_insecure() {
        let mut config = Config {
            api_keys: vec!["secret".to_owned()],
            ..Config::default()
        };
        // API key set but no encryption key, not insecure ⇒ rejected.
        assert!(config.validate().is_err());
        config.encryption_key = Some(TEST_KEY.to_owned());
        assert!(config.validate().is_ok());
        // A malformed key is rejected up front, not at first write.
        config.encryption_key = Some("not-a-valid-hex-key".to_owned());
        assert!(config.validate().is_err());
        // Insecure mode may run without encryption-at-rest.
        config.insecure = true;
        config.encryption_key = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_rejects_public_bind_without_optout() {
        let mut config = Config {
            api_keys: vec!["secret".to_owned()],
            encryption_key: Some(TEST_KEY.to_owned()),
            ..Config::default()
        };
        config.rest_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 6333);
        // Auth and encryption are satisfied, so the only failure is the bind rule.
        assert!(config.validate().is_err());
        config.insecure = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn constant_time_eq_matches_only_equal() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
