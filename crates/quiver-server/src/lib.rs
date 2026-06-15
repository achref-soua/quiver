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
//! Auth is by scoped API key (Bearer / gRPC `authorization` metadata) with
//! default-deny RBAC: each key carries a role (read ⊆ write ⊆ admin) and a
//! collection scope, enforced on every operation at the shared op layer
//! (ADR-0011/0013, the `auth` module). Encryption-at-rest is on by default
//! (ADR-0010): unless `insecure` is set, an `encryption_key` is required and the
//! engine is opened through `quiver-crypto`'s AEAD codec; payloads may also be
//! client-side-encrypted (ADR-0012). TLS-in-transit uses `rustls` over the
//! audited `ring` provider — REST via `axum-server`, gRPC via tonic's `tls-ring`
//! — and a non-loopback bind requires it; setting a client CA additionally
//! requires mutual TLS. Mutating and administrative operations, and every
//! access-control denial, are recorded to an append-only audit log (ADR-0011,
//! the `audit` module) when `audit_log` is set. Per-tenant engine partitioning
//! and rate limiting are later phases. Design: `docs/api/rest-grpc.md`.

mod audit;
mod auth;
mod error;
mod grpc;
mod rest;

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum_server::tls_rustls::RustlsConfig;
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use quiver_crypto::AeadCodec;
use quiver_embed::{
    Database, Descriptor, DistanceMetric, Dtype, FilterableField, IndexSpec, SearchParams,
};
use quiver_query::Filter;

pub use auth::{Action, ApiKey, CollectionScope};
pub use error::Error;

use audit::{AuditLog, Outcome};
use auth::Principal;

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
    /// Accepted API keys with their RBAC scopes (ADR-0011). A bare secret —
    /// from a comma-separated `QUIVER_API_KEYS` (the env form) or a plain TOML
    /// array entry — is an all-collections admin key; a structured
    /// `{secret, role, collections}` entry pins a narrower scope. Empty is
    /// allowed only with `insecure = true`.
    #[serde(default, deserialize_with = "auth::de_api_keys")]
    pub api_keys: Vec<ApiKey>,
    /// Hex-encoded 256-bit **master key** for encryption-at-rest (64 hex
    /// characters). It wraps per-collection data-encryption keys (ADR-0010).
    /// Required unless `insecure = true` or [`master_key_file`] is set; source it
    /// from the environment or a secret store, never the committed config. `None`
    /// ⇒ data is stored unencrypted (only valid in `insecure` mode).
    ///
    /// [`master_key_file`]: Config::master_key_file
    pub encryption_key: Option<String>,
    /// Path to a file holding the hex master key, as an alternative to
    /// [`encryption_key`] (set exactly one). Lets the key arrive as a mounted
    /// secret (Docker/Kubernetes) or a KMS-decrypted file rather than an
    /// environment variable. It should be mode `0600`; a group/world-accessible
    /// file is warned about at startup.
    ///
    /// [`encryption_key`]: Config::encryption_key
    pub master_key_file: Option<PathBuf>,
    /// Path to the PEM-encoded TLS certificate chain. Must be set together with
    /// `tls_key`. Required for a non-loopback bind unless `insecure = true`.
    pub tls_cert: Option<PathBuf>,
    /// Path to the PEM-encoded TLS private key. Must be set together with
    /// `tls_cert`.
    pub tls_key: Option<PathBuf>,
    /// Path to a PEM-encoded CA certificate that signs accepted client
    /// certificates. When set, both transports require **mutual TLS**: a client
    /// must present a certificate chaining to this CA to connect (ADR-0011).
    /// Requires `tls_cert`/`tls_key`; bearer API keys still carry the RBAC scope.
    pub tls_client_ca: Option<PathBuf>,
    /// Path to an append-only audit log file (ADR-0011). When set, every
    /// mutating and administrative operation and every access-control denial is
    /// appended as one JSON object per line (JSON Lines); records are always
    /// also emitted as `tracing` events. Unset ⇒ tracing only. Secrets are never
    /// written — see `docs/security/audit.md`.
    pub audit_log: Option<PathBuf>,
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
            master_key_file: None,
            tls_cert: None,
            tls_key: None,
            tls_client_ca: None,
            audit_log: None,
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
    /// and no non-loopback bind without TLS.
    pub fn validate(&self) -> Result<(), Error> {
        if self.api_keys.is_empty() && !self.insecure {
            return Err(Error::Config(
                "no api_keys configured: set QUIVER_API_KEYS (comma-separated) or \
                 set insecure=true for local development"
                    .to_owned(),
            ));
        }
        // Resolve the master key from the env var or a key file (exactly one).
        let master_key = self.master_key_hex()?;
        if master_key.is_none() && !self.insecure {
            return Err(Error::Config(
                "no encryption key configured: encryption-at-rest is on by default — \
                 set QUIVER_ENCRYPTION_KEY to a 64-hex-character (256-bit) key (or \
                 QUIVER_MASTER_KEY_FILE to a file holding it), or set insecure=true to \
                 store data unencrypted (development only)"
                    .to_owned(),
            ));
        }
        // Fail fast on a malformed key rather than at first write.
        if let Some(key) = &master_key {
            AeadCodec::from_hex(key)
                .map_err(|e| Error::Config(format!("invalid master key: {e}")))?;
        }
        // TLS certificate and key are set together or not at all.
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            return Err(Error::Config(
                "tls_cert and tls_key must be set together".to_owned(),
            ));
        }
        // mTLS layers on top of server TLS: a client CA needs a server cert/key.
        if self.tls_client_ca.is_some() && !(self.tls_cert.is_some() && self.tls_key.is_some()) {
            return Err(Error::Config(
                "tls_client_ca (mutual TLS) requires tls_cert and tls_key".to_owned(),
            ));
        }
        let tls_enabled = self.tls_cert.is_some() && self.tls_key.is_some();
        let non_loopback = !self.rest_addr.ip().is_loopback() || !self.grpc_addr.ip().is_loopback();
        if non_loopback && !tls_enabled && !self.insecure {
            return Err(Error::Config(
                "non-loopback bind requires TLS: set tls_cert and tls_key (PEM files), \
                 or insecure=true for local development"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The effective hex master key: from [`master_key_file`] when set (read and
    /// trimmed), otherwise [`encryption_key`]. `None` means no key is configured
    /// (only valid with `insecure`).
    ///
    /// # Errors
    /// [`Error::Config`] if both sources are set, or the key file cannot be read.
    ///
    /// [`master_key_file`]: Config::master_key_file
    /// [`encryption_key`]: Config::encryption_key
    pub(crate) fn master_key_hex(&self) -> Result<Option<String>, Error> {
        let env_key = self
            .encryption_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        match (&self.master_key_file, env_key) {
            (Some(_), Some(_)) => Err(Error::Config(
                "set either encryption_key (QUIVER_ENCRYPTION_KEY) or master_key_file \
                 (QUIVER_MASTER_KEY_FILE), not both"
                    .to_owned(),
            )),
            (Some(path), None) => {
                warn_if_world_readable(path);
                let hex = std::fs::read_to_string(path).map_err(|e| {
                    Error::Config(format!("reading master_key_file {}: {e}", path.display()))
                })?;
                Ok(Some(hex.trim().to_owned()))
            }
            (None, Some(key)) => Ok(Some(key.to_owned())),
            (None, None) => Ok(None),
        }
    }
}

// Warn (don't fail — Docker/Kubernetes secrets often mount group/world-readable)
// when a master-key file is more permissive than `0600`.
#[cfg(unix)]
fn warn_if_world_readable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:o}", meta.permissions().mode() & 0o777),
            "master key file is group/world-accessible; restrict it to 0600"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &std::path::Path) {}

/// Shared server state: the engine behind a single-writer lock, the accepted
/// API keys with their RBAC scopes, and the audit log.
#[derive(Clone)]
pub(crate) struct AppState {
    db: Arc<Mutex<Database>>,
    keys: Arc<Vec<ApiKey>>,
    audit: Arc<AuditLog>,
}

/// A collection's metadata.
pub(crate) struct CollectionInfo {
    pub name: String,
    pub dim: u32,
    pub metric: DistanceMetric,
    pub count: u64,
    pub index: IndexSpec,
    pub filterable: Vec<FilterableField>,
    pub multivector: bool,
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

/// A multi-vector document to upsert.
pub(crate) struct DocumentIn {
    pub id: String,
    pub vectors: Vec<Vec<f32>>,
    pub payload: Value,
}

/// A multi-vector document hit (MaxSim).
pub(crate) struct DocumentMatchOut {
    pub id: String,
    pub score: f32,
    pub payload: Option<Value>,
    pub vectors: Option<Vec<Vec<f32>>>,
}

impl AppState {
    /// Authenticate a presented bearer token to its [`Principal`], or `None`
    /// (a 401). An empty key set means `insecure` mode (validated at startup),
    /// which admits any caller as an all-collections admin.
    pub(crate) fn authenticate(&self, presented: Option<&str>) -> Option<Principal> {
        auth::authenticate(&self.keys, presented)
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

    // Authorize `action` on `resource`, recording a denial in the audit log
    // before propagating it. The shared choke point for both transports.
    fn authorize(
        &self,
        principal: &Principal,
        action: Action,
        op: &str,
        resource: &str,
    ) -> Result<(), Error> {
        principal
            .require(action, Some(resource))
            .inspect_err(|_| self.audit.deny(principal.actor(), op, resource))
    }

    // Authorize a collection-agnostic operation (listing): only the role is
    // checked; a denial is recorded against the `*` resource.
    fn authorize_global(
        &self,
        principal: &Principal,
        action: Action,
        op: &str,
    ) -> Result<(), Error> {
        principal
            .require(action, None)
            .inspect_err(|_| self.audit.deny(principal.actor(), op, "*"))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_collection(
        &self,
        principal: &Principal,
        name: String,
        dim: u32,
        metric: DistanceMetric,
        index: IndexSpec,
        filterable: Vec<FilterableField>,
        multivector: bool,
    ) -> Result<CollectionInfo, Error> {
        self.authorize(principal, Action::Admin, "create_collection", &name)?;
        let descriptor = Descriptor::new(dim, Dtype::F32, metric)
            .with_index(index)
            .with_filterable(filterable.clone())
            .with_multivector(multivector);
        let owned = name.clone();
        let result = self
            .run_blocking(move |db| db.create_collection(&owned, descriptor))
            .await;
        self.audit.record(
            principal.actor(),
            "create_collection",
            &name,
            Outcome::of(&result),
        );
        result?;
        Ok(CollectionInfo {
            name,
            dim,
            metric,
            count: 0,
            index,
            filterable,
            multivector,
        })
    }

    pub(crate) async fn get_collection(
        &self,
        principal: &Principal,
        name: String,
    ) -> Result<CollectionInfo, Error> {
        self.authorize(principal, Action::Read, "get_collection", &name)?;
        self.run_blocking(move |db| {
            let descriptor = db
                .descriptor(&name)
                .cloned()
                .ok_or_else(|| quiver_embed::Error::CollectionNotFound(name.clone()))?;
            // A multi-vector collection reports its document count, not its
            // (much larger) token-row count.
            let count = if descriptor.multivector {
                db.document_count(&name)? as u64
            } else {
                db.len(&name)? as u64
            };
            Ok(CollectionInfo {
                name,
                dim: descriptor.dim,
                metric: descriptor.metric,
                count,
                index: descriptor.index,
                filterable: descriptor.filterable,
                multivector: descriptor.multivector,
            })
        })
        .await
    }

    pub(crate) async fn list_collections(
        &self,
        principal: &Principal,
    ) -> Result<Vec<CollectionInfo>, Error> {
        self.authorize_global(principal, Action::Read, "list_collections")?;
        let mut infos = self
            .run_blocking(|db| {
                let mut out = Vec::new();
                for name in db.collection_names() {
                    if let Some(descriptor) = db.descriptor(&name).cloned() {
                        let count = if descriptor.multivector {
                            db.document_count(&name)? as u64
                        } else {
                            db.len(&name)? as u64
                        };
                        out.push(CollectionInfo {
                            name,
                            dim: descriptor.dim,
                            metric: descriptor.metric,
                            count,
                            index: descriptor.index,
                            filterable: descriptor.filterable,
                            multivector: descriptor.multivector,
                        });
                    }
                }
                Ok(out)
            })
            .await?;
        // Never reveal collections outside the caller's scope.
        infos.retain(|info| principal.can_see(&info.name));
        Ok(infos)
    }

    pub(crate) async fn delete_collection(
        &self,
        principal: &Principal,
        name: String,
    ) -> Result<bool, Error> {
        self.authorize(principal, Action::Admin, "delete_collection", &name)?;
        let resource = name.clone();
        let result = self.run_blocking(move |db| db.drop_collection(&name)).await;
        self.audit.record(
            principal.actor(),
            "delete_collection",
            &resource,
            Outcome::of(&result),
        );
        result
    }

    pub(crate) async fn upsert(
        &self,
        principal: &Principal,
        collection: String,
        points: Vec<PointIn>,
    ) -> Result<u64, Error> {
        self.authorize(principal, Action::Write, "upsert", &collection)?;
        let resource = collection.clone();
        let result = self
            .run_blocking(move |db| {
                let mut count = 0u64;
                for point in &points {
                    db.upsert(&collection, &point.id, &point.vector, &point.payload)?;
                    count += 1;
                }
                Ok(count)
            })
            .await;
        self.audit
            .record(principal.actor(), "upsert", &resource, Outcome::of(&result));
        result
    }

    pub(crate) async fn delete_points(
        &self,
        principal: &Principal,
        collection: String,
        ids: Vec<String>,
    ) -> Result<u64, Error> {
        self.authorize(principal, Action::Write, "delete_points", &collection)?;
        let resource = collection.clone();
        let result = self
            .run_blocking(move |db| {
                let mut count = 0u64;
                for id in &ids {
                    if db.delete(&collection, id)? {
                        count += 1;
                    }
                }
                Ok(count)
            })
            .await;
        self.audit.record(
            principal.actor(),
            "delete_points",
            &resource,
            Outcome::of(&result),
        );
        result
    }

    pub(crate) async fn get_points(
        &self,
        principal: &Principal,
        collection: String,
        ids: Vec<String>,
        with_vector: bool,
    ) -> Result<Vec<PointOut>, Error> {
        self.authorize(principal, Action::Read, "get_points", &collection)?;
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
        principal: &Principal,
        collection: String,
        vector: Vec<f32>,
        k: usize,
        filter: Option<Filter>,
        ef_search: usize,
        with_payload: bool,
        with_vector: bool,
    ) -> Result<Vec<MatchOut>, Error> {
        self.authorize(principal, Action::Read, "search", &collection)?;
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

    pub(crate) async fn upsert_documents(
        &self,
        principal: &Principal,
        collection: String,
        documents: Vec<DocumentIn>,
    ) -> Result<u64, Error> {
        self.authorize(principal, Action::Write, "upsert_documents", &collection)?;
        let resource = collection.clone();
        let result = self
            .run_blocking(move |db| {
                let mut count = 0u64;
                for doc in &documents {
                    db.upsert_document(&collection, &doc.id, &doc.vectors, &doc.payload)?;
                    count += 1;
                }
                Ok(count)
            })
            .await;
        self.audit.record(
            principal.actor(),
            "upsert_documents",
            &resource,
            Outcome::of(&result),
        );
        result
    }

    pub(crate) async fn delete_documents(
        &self,
        principal: &Principal,
        collection: String,
        ids: Vec<String>,
    ) -> Result<u64, Error> {
        self.authorize(principal, Action::Write, "delete_documents", &collection)?;
        let resource = collection.clone();
        let result = self
            .run_blocking(move |db| {
                let mut count = 0u64;
                for id in &ids {
                    if db.delete_document(&collection, id)? {
                        count += 1;
                    }
                }
                Ok(count)
            })
            .await;
        self.audit.record(
            principal.actor(),
            "delete_documents",
            &resource,
            Outcome::of(&result),
        );
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn search_multi_vector(
        &self,
        principal: &Principal,
        collection: String,
        query: Vec<Vec<f32>>,
        k: usize,
        filter: Option<Filter>,
        ef_search: usize,
        with_payload: bool,
        with_vector: bool,
    ) -> Result<Vec<DocumentMatchOut>, Error> {
        self.authorize(principal, Action::Read, "search_multi_vector", &collection)?;
        self.run_blocking(move |db| {
            let params = SearchParams {
                k,
                filter,
                ef_search,
                with_payload,
                with_vector,
            };
            let matches = db.search_multi_vector(&collection, &query, &params)?;
            Ok(matches
                .into_iter()
                .map(|m| DocumentMatchOut {
                    id: m.id,
                    score: m.score,
                    payload: m.payload,
                    vectors: m.vectors,
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
    let audit = Arc::new(AuditLog::open(config.audit_log.as_deref())?);
    let state = AppState {
        db: Arc::new(Mutex::new(db)),
        keys: Arc::new(config.api_keys.clone()),
        audit,
    };

    let app = rest::router(state.clone());
    let grpc = grpc::service(state);

    let tls = load_tls(&config)?;

    // REST: terminate TLS with axum-server when configured, else serve plaintext.
    let rest_fut: Pin<Box<dyn Future<Output = Result<(), Error>> + Send>> = match &tls {
        Some(material) => {
            let rustls_config = RustlsConfig::from_config(Arc::clone(&material.rest_config));
            let std_listener = rest_listener.into_std().map_err(Error::Io)?;
            let server =
                axum_server::from_tcp_rustls(std_listener, rustls_config).map_err(Error::Io)?;
            Box::pin(async move {
                server
                    .serve(app.into_make_service())
                    .await
                    .map_err(Error::Io)
            })
        }
        None => Box::pin(async move { axum::serve(rest_listener, app).await.map_err(Error::Io) }),
    };

    // gRPC: tonic terminates TLS itself (ring provider) when an identity is set.
    let mut grpc_builder = tonic::transport::Server::builder();
    if let Some(material) = &tls {
        let identity = Identity::from_pem(&material.cert_pem, &material.key_pem);
        let mut tls_config = ServerTlsConfig::new().identity(identity);
        // Require client certificates chaining to the configured CA (mTLS).
        if let Some(ca_pem) = &material.client_ca_pem {
            tls_config = tls_config.client_ca_root(Certificate::from_pem(ca_pem));
        }
        grpc_builder = grpc_builder
            .tls_config(tls_config)
            .map_err(|e| Error::Internal(format!("grpc tls config: {e}")))?;
    }
    let grpc_fut = async move {
        grpc_builder
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

// Open the engine, enabling encryption-at-rest when a key is configured. The
// configured key is the **master key** of an envelope key-ring (ADR-0010): it
// wraps a per-collection data-encryption key, so dropping a collection
// crypto-shreds it. With no key (only valid in `insecure` mode, enforced by
// `Config::validate`) the engine is opened in plaintext.
fn open_database(config: &Config) -> Result<Database, Error> {
    let master_key = config.master_key_hex()?;
    let keyring =
        quiver_crypto::open_keyring(&config.data_dir, master_key.as_deref(), config.insecure)
            .map_err(|e| Error::Config(e.to_string()))?;
    let db = match keyring {
        Some(keyring) => Database::open_with_keyring(&config.data_dir, keyring)?,
        None => Database::open(&config.data_dir)?,
    };
    Ok(db)
}

// TLS material shared by both transports: the raw PEM (for tonic's `Identity`
// and `client_ca_root`) and a parsed rustls server config (for axum-server's
// REST acceptor). `client_ca_pem` is set only when mutual TLS is configured.
struct TlsMaterial {
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    client_ca_pem: Option<Vec<u8>>,
    rest_config: Arc<rustls::ServerConfig>,
}

// Read the configured certificate, key, and optional client CA, returning `None`
// when TLS is not configured. `Config::validate` already enforces that the cert
// and key are set together, that a client CA requires them, and that a
// non-loopback bind requires TLS.
fn load_tls(config: &Config) -> Result<Option<TlsMaterial>, Error> {
    match (&config.tls_cert, &config.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert_pem = std::fs::read(cert_path).map_err(Error::Io)?;
            let key_pem = std::fs::read(key_path).map_err(Error::Io)?;
            let client_ca_pem = config
                .tls_client_ca
                .as_ref()
                .map(std::fs::read)
                .transpose()
                .map_err(Error::Io)?;
            let rest_config = Arc::new(rustls_server_config(
                &cert_pem,
                &key_pem,
                client_ca_pem.as_deref(),
            )?);
            Ok(Some(TlsMaterial {
                cert_pem,
                key_pem,
                client_ca_pem,
                rest_config,
            }))
        }
        (None, None) => Ok(None),
        _ => Err(Error::Config(
            "tls_cert and tls_key must be set together".to_owned(),
        )),
    }
}

// Build a rustls server config from PEM bytes over the audited `ring` provider
// (no OpenSSL, no aws-lc-rs C toolchain). TLS 1.3 and 1.2 are offered. When a
// client CA is supplied, client certificates chaining to it are required (mTLS).
fn rustls_server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_ca_pem: Option<&[u8]>,
) -> Result<rustls::ServerConfig, Error> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    let certs = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Config(format!("parsing tls_cert: {e}")))?;
    if certs.is_empty() {
        return Err(Error::Config(
            "tls_cert contains no certificates".to_owned(),
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| Error::Config(format!("parsing tls_key: {e}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Internal(format!("tls protocol versions: {e}")))?;
    let builder = match client_ca_pem {
        Some(ca_pem) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(ca_pem) {
                let cert =
                    cert.map_err(|e| Error::Config(format!("parsing tls_client_ca: {e}")))?;
                roots
                    .add(cert)
                    .map_err(|e| Error::Config(format!("adding tls_client_ca: {e}")))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider,
            )
            .build()
            .map_err(|e| Error::Config(format!("client certificate verifier: {e}")))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    builder
        .with_single_cert(certs, key)
        .map_err(|e| Error::Config(format!("tls certificate/key: {e}")))
}

/// Initialize structured logging from `RUST_LOG` (defaulting to `info`). Safe to
/// call once at startup; a second call is ignored.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
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
        config.api_keys = vec!["secret".into()];
        config.encryption_key = Some(TEST_KEY.to_owned());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_requires_encryption_key_unless_insecure() {
        let mut config = Config {
            api_keys: vec!["secret".into()],
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
    fn master_key_file_is_an_alternative_to_the_env_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        // A trailing newline (as editors and `echo` add) is trimmed off.
        std::fs::write(&path, format!("{TEST_KEY}\n")).unwrap();

        let mut config = Config {
            api_keys: vec!["secret".into()],
            master_key_file: Some(path.clone()),
            ..Config::default()
        };
        // The file alone satisfies encryption-at-rest and resolves to the key.
        assert!(config.validate().is_ok());
        assert_eq!(config.master_key_hex().unwrap().as_deref(), Some(TEST_KEY));

        // Setting both the env key and a file is rejected as ambiguous.
        config.encryption_key = Some(TEST_KEY.to_owned());
        assert!(config.validate().is_err());

        // A file holding malformed hex is rejected up front.
        config.encryption_key = None;
        std::fs::write(&path, "not-a-valid-key").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn config_rejects_public_bind_without_optout() {
        let mut config = Config {
            api_keys: vec!["secret".into()],
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
    fn config_public_bind_allowed_with_tls() {
        let config = Config {
            api_keys: vec!["secret".into()],
            encryption_key: Some(TEST_KEY.to_owned()),
            tls_cert: Some(PathBuf::from("cert.pem")),
            tls_key: Some(PathBuf::from("key.pem")),
            rest_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 6333),
            ..Config::default()
        };
        // TLS configured ⇒ a non-loopback bind is allowed without insecure.
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_tls_cert_and_key_must_pair() {
        let mut config = Config {
            api_keys: vec!["secret".into()],
            encryption_key: Some(TEST_KEY.to_owned()),
            tls_cert: Some(PathBuf::from("cert.pem")),
            ..Config::default()
        };
        // Cert without key ⇒ rejected.
        assert!(config.validate().is_err());
        config.tls_key = Some(PathBuf::from("key.pem"));
        assert!(config.validate().is_ok());
    }
}
