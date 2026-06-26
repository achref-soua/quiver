// SPDX-License-Identifier: AGPL-3.0-only
//! The cluster **coordinator** (ADR-0066): a thin service, off the data path, that
//! owns the authoritative **versioned** shard map and per-shard health. Routers
//! refresh the map from it (`GET /cluster/map`) into their `ArcSwap` with no
//! restart, and an operator (or, later, an autoscaler) grows or shrinks the cluster
//! through it. It is **not** a query dependency: a router caches the map, so the
//! coordinator being briefly down stops *membership changes*, not *serving*.
//!
//! Single-node in this increment — its state (the map + a monotonic id counter) is
//! persisted to a JSON file so a restart recovers; coordinator HA can later ride the
//! per-shard Raft increment. Runs over a trusted network, like the shards
//! themselves (ADR-0030).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router, response::IntoResponse};
use quiver_cluster::ShardMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::Config;
use crate::auth::{self, Action, ApiKey, Principal};
use crate::error::Error;

// A grace window ≥ the router's map-refresh interval, so every router has adopted the
// new map version (and its dual-write / routing) before the coordinator relies on it
// — once before the copy (dual-write active), once after the flip (slice routed to
// the new shard) before dropping the donors' copies.
const MIGRATION_GRACE: Duration = Duration::from_secs(3);
// Page size for scrolling a donor's points during the copy.
const COPY_PAGE: usize = 1_000;

/// Opt-in automatic **scale-out** policy for the coordinator (ADR-0065 increment 5).
/// When enabled, the coordinator samples each shard's point count and, when the
/// busiest crosses `high_water_points`, grows the cluster by joining one of the
/// `standby_urls` — driving the same safe online migration as a manual
/// `POST /cluster/shards/grow`. An explicit policy, not magic: nothing scales
/// without a configured threshold and a standby to grow into, a cooldown bounds the
/// rate, and a migration in flight is never interrupted. Scale-*in* is deliberately
/// **not** automated here (a safe online drain is a separate increment); shrink
/// stays a manual, drained `DELETE /cluster/shards/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoscaleConfig {
    /// Turn the policy on. Default `false`.
    pub enabled: bool,
    /// Per-shard point-count high-water mark: when any shard exceeds it, scale out.
    /// `0` disables scale-out even when `enabled`.
    pub high_water_points: u64,
    /// Pool of standby shard base URLs to grow into, consumed one per scale-out.
    pub standby_urls: Vec<String>,
    /// How often to sample the load signal, in seconds.
    pub interval_secs: u64,
    /// Minimum seconds between scale actions (hysteresis), so a migration settles
    /// before another can be triggered.
    pub cooldown_secs: u64,
    /// Hard cap on the shard count the policy will grow to. `0` = no cap.
    pub max_shards: usize,
}

impl Default for AutoscaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            high_water_points: 0,
            standby_urls: Vec::new(),
            interval_secs: 30,
            cooldown_secs: 300,
            max_shards: 0,
        }
    }
}

// What the coordinator persists so a restart recovers exactly: the versioned map
// plus the monotonic id counter (so an id is never reused even across restarts).
#[derive(Serialize, Deserialize)]
struct Persisted {
    next_id: u64,
    map: ShardMap,
}

/// The coordinator's in-memory state behind its REST API.
struct CoordinatorState {
    map: RwLock<ShardMap>,
    next_id: AtomicU64,
    // Where the state is persisted on each change; `None` = in-memory only.
    path: Option<PathBuf>,
    // An HTTP client for shard health probes and migration copy/drop.
    http: reqwest::Client,
    // Optional API key the coordinator presents to shards (a cluster runs over a
    // trusted network, like the router — ADR-0030).
    shard_key: Option<String>,
    // Configured API keys (RBAC, ADR-0011). The membership API requires a valid
    // key — `admin` for the mutating shard ops, any role for reads — so a
    // network-reachable coordinator cannot be reshaped by an unauthenticated
    // caller. Empty only in `insecure` mode (enforced at startup by
    // `Config::validate`), where `authenticate` admits any caller as admin.
    keys: Arc<Vec<ApiKey>>,
    // Opt-in automatic scale-out policy (ADR-0065 increment 5).
    autoscale: AutoscaleConfig,
    // Remaining standby shard URLs to grow into, consumed one per scale-out.
    standby: tokio::sync::Mutex<Vec<String>>,
    // When the policy last scaled (for the cooldown).
    last_scale: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl CoordinatorState {
    // Load persisted state if the file exists, else bootstrap from the operator's
    // `QUIVER_CLUSTER_SHARDS` / `QUIVER_CLUSTER_REPLICAS` (version 0, ids 0..N).
    fn bootstrap(config: &Config) -> Result<Self, Error> {
        let path = config.coordinator_state.clone();
        if let Some(p) = &path
            && p.exists()
        {
            let bytes = std::fs::read(p).map_err(Error::Io)?;
            let persisted: Persisted = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Config(format!("coordinator state {p:?}: {e}")))?;
            return Ok(Self {
                map: RwLock::new(persisted.map),
                next_id: AtomicU64::new(persisted.next_id),
                path,
                http: reqwest::Client::new(),
                shard_key: config.cluster_shard_key.clone(),
                keys: Arc::new(config.api_keys.clone()),
                autoscale: config.autoscale.clone(),
                standby: tokio::sync::Mutex::new(config.autoscale.standby_urls.clone()),
                last_scale: tokio::sync::Mutex::new(None),
            });
        }
        let map = build_seed_map(config)?;
        let next_id = map.len() as u64; // ids 0..N are taken; the next free id is N
        let state = Self {
            map: RwLock::new(map),
            next_id: AtomicU64::new(next_id),
            path,
            http: reqwest::Client::new(),
            shard_key: config.cluster_shard_key.clone(),
            keys: Arc::new(config.api_keys.clone()),
            autoscale: config.autoscale.clone(),
            standby: tokio::sync::Mutex::new(config.autoscale.standby_urls.clone()),
            last_scale: tokio::sync::Mutex::new(None),
        };
        Ok(state)
    }

    // Persist the current map + id counter (called inside the map write lock so the
    // file and memory never diverge). A no-op when no path is configured.
    fn persist(&self, map: &ShardMap) -> Result<(), Error> {
        let Some(p) = &self.path else { return Ok(()) };
        let persisted = Persisted {
            next_id: self.next_id.load(Ordering::SeqCst),
            map: map.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&persisted)
            .map_err(|e| Error::Internal(format!("serialize coordinator state: {e}")))?;
        std::fs::write(p, bytes).map_err(Error::Io)
    }

    // --- Automated online migration (ADR-0066 increment 3c-ii) -------------

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.shard_key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }

    // Send a JSON request to a shard, returning its parsed body (or `Null`).
    async fn send_json(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Value,
    ) -> Result<Value, Error> {
        let resp = self
            .auth(self.http.request(method, url).json(&body))
            .send()
            .await
            .map_err(|e| Error::Internal(format!("shard {url} unreachable: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Internal(format!(
                "shard {url} returned {status}: {text}"
            )));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    // The shard's collection schemas (the `GET /v1/collections` array of DTOs).
    async fn list_collection_metas(&self, url: &str) -> Result<Vec<Value>, Error> {
        let body = self
            .send_json(
                reqwest::Method::GET,
                &format!("{url}/v1/collections"),
                Value::Null,
            )
            .await?;
        Ok(body.as_array().cloned().unwrap_or_default())
    }

    // Create `dto`'s collection on the new shard if it is missing (it predates the
    // shard's join, so the cluster broadcast never reached it).
    async fn ensure_collection(&self, new_url: &str, dto: &Value) -> Result<(), Error> {
        let name = dto["name"].as_str().unwrap_or_default();
        let exists = self
            .auth(self.http.get(format!("{new_url}/v1/collections/{name}")))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if exists {
            return Ok(());
        }
        let mut body = json!({
            "name": dto["name"],
            "dim": dto["dim"],
            "metric": dto["metric"],
            "index": dto["index"],
        });
        for k in ["pq_subspaces", "filterable", "vector_encryption"] {
            if let Some(v) = dto.get(k) {
                body[k] = v.clone();
            }
        }
        self.send_json(
            reqwest::Method::POST,
            &format!("{new_url}/v1/collections"),
            body,
        )
        .await
        .map(|_| ())
    }

    // One page of a donor's points (id + payload, and the vector when `with_vector`).
    async fn fetch_page(
        &self,
        url: &str,
        collection: &str,
        offset: usize,
        with_vector: bool,
    ) -> Result<Vec<Value>, Error> {
        let body = self
            .send_json(
                reqwest::Method::POST,
                &format!("{url}/v1/collections/{collection}/fetch"),
                json!({"offset": offset, "limit": COPY_PAGE, "with_payload": true, "with_vector": with_vector}),
            )
            .await?;
        Ok(body["points"].as_array().cloned().unwrap_or_default())
    }

    // Copy the slice owned by `new_id` from one donor to the new shard, paginated.
    // **Get-if-absent**: a point already on the new shard was put there by a concurrent
    // dual-write (the latest value), so the copy never overwrites it with the donor's
    // possibly-older read — no lost update.
    async fn copy_slice(
        &self,
        donor: &str,
        new_url: &str,
        collection: &str,
        map: &ShardMap,
        new_id: u64,
    ) -> Result<(), Error> {
        let mut offset = 0usize;
        loop {
            let page = self.fetch_page(donor, collection, offset, true).await?;
            let n = page.len();
            for pt in &page {
                let Some(id) = pt["id"].as_str() else {
                    continue;
                };
                if map.shard_for(id).id != new_id {
                    continue;
                }
                let get = format!("{new_url}/v1/collections/{collection}/points/{id}");
                let present = self
                    .auth(self.http.get(&get))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                if present {
                    continue;
                }
                self.send_json(
                    reqwest::Method::POST,
                    &format!("{new_url}/v1/collections/{collection}/points"),
                    json!({"points": [{"id": id, "vector": pt["vector"], "payload": pt["payload"]}]}),
                )
                .await?;
            }
            offset += n;
            if n < COPY_PAGE {
                return Ok(());
            }
        }
    }

    // After the flip, delete the donor's now-stale copies of `new_id`'s slice.
    async fn drop_slice(
        &self,
        donor: &str,
        collection: &str,
        map: &ShardMap,
        new_id: u64,
    ) -> Result<(), Error> {
        let mut offset = 0usize;
        let mut ids: Vec<String> = Vec::new();
        loop {
            let page = self.fetch_page(donor, collection, offset, false).await?;
            let n = page.len();
            for pt in &page {
                if let Some(id) = pt["id"].as_str()
                    && map.shard_for(id).id == new_id
                {
                    ids.push(id.to_owned());
                }
            }
            offset += n;
            if n < COPY_PAGE {
                break;
            }
        }
        for chunk in ids.chunks(COPY_PAGE) {
            self.send_json(
                reqwest::Method::DELETE,
                &format!("{donor}/v1/collections/{collection}/points"),
                json!({ "ids": chunk }),
            )
            .await?;
        }
        Ok(())
    }

    // The full migration: wait for dual-write to be live, copy each donor's slice to
    // the new shard, flip ownership, wait for routers to adopt the flip, then drop the
    // donors' copies. Single-vector collections only (the scroll path); a multivector
    // collection aborts the migration honestly rather than silently dropping its slice.
    async fn run_migration(&self, new_id: u64) -> Result<(), Error> {
        tokio::time::sleep(MIGRATION_GRACE).await;
        // Snapshot the map: membership is stable during a migration (no concurrent
        // grow), so this is a valid HRW oracle for the whole copy + drop.
        let map = self.map.read().await.clone();
        let new_url = map
            .shards()
            .iter()
            .find(|s| s.id == new_id)
            .map(|s| s.primary_url.clone())
            .ok_or_else(|| Error::Internal("joining shard left the map".into()))?;
        let donors: Vec<String> = map
            .active_shards()
            .iter()
            .map(|s| s.primary_url.clone())
            .collect();
        let donor0 = donors
            .first()
            .ok_or_else(|| Error::Internal("no donor for migration".into()))?;
        let collections = self.list_collection_metas(donor0).await?;
        if collections
            .iter()
            .any(|c| c["multivector"].as_bool().unwrap_or(false))
        {
            return Err(Error::BadRequest(
                "auto-migration does not yet support multivector collections".into(),
            ));
        }
        for c in &collections {
            let name = c["name"].as_str().unwrap_or_default().to_owned();
            self.ensure_collection(&new_url, c).await?;
            for donor in &donors {
                self.copy_slice(donor, &new_url, &name, &map, new_id)
                    .await?;
            }
        }
        // Flip ownership atomically.
        {
            let mut m = self.map.write().await;
            m.promote(new_id)
                .map_err(|e| Error::BadRequest(e.to_string()))?;
            self.persist(&m)?;
        }
        tokio::time::sleep(MIGRATION_GRACE).await;
        for c in &collections {
            let name = c["name"].as_str().unwrap_or_default().to_owned();
            for donor in &donors {
                self.drop_slice(donor, &name, &map, new_id).await?;
            }
        }
        tracing::info!(shard = new_id, "cluster migration complete");
        Ok(())
    }

    // --- Grow (shared by the manual endpoint and the autoscaler) -----------

    // Add a shard in the joining state and run the online migration in the
    // background, returning the joining map immediately. On migration failure the
    // join is reverted. The body of `POST /cluster/shards/grow`, reused by the
    // autoscaler.
    async fn grow_shard(
        self: &Arc<Self>,
        primary_url: String,
        replica_urls: Vec<String>,
    ) -> Result<ShardMap, Error> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let snapshot = {
            let mut map = self.map.write().await;
            map.add_joining_shard(id, &primary_url, replica_urls)
                .map_err(|e| Error::BadRequest(e.to_string()))?;
            self.persist(&map)?;
            map.clone()
        };
        let bg = self.clone();
        tokio::spawn(async move {
            if let Err(e) = bg.run_migration(id).await {
                tracing::error!(shard = id, error = %e, "cluster migration failed; reverting the join");
                let mut map = bg.map.write().await;
                let _ = map.remove_shard(id);
                let _ = bg.persist(&map);
            }
        });
        Ok(snapshot)
    }

    // --- Autoscale policy (ADR-0065 increment 5) ---------------------------

    // The load signal for one shard: the sum of its point counts across collections
    // (from `GET /v1/collections`). Best-effort — an unreachable shard reads 0.
    async fn shard_points(&self, primary_url: &str) -> u64 {
        let Ok(metas) = self.list_collection_metas(primary_url).await else {
            return 0;
        };
        metas.iter().filter_map(|c| c["count"].as_u64()).sum()
    }

    // One autoscale tick: if the busiest active shard exceeds the high-water mark and
    // the policy allows (cooldown elapsed, under the shard cap, no migration in
    // flight, a standby available), grow the cluster into a standby.
    async fn maybe_scale_out(self: &Arc<Self>) {
        let cfg = &self.autoscale;
        if !cfg.enabled || cfg.high_water_points == 0 {
            return;
        }
        if let Some(t) = *self.last_scale.lock().await
            && t.elapsed() < Duration::from_secs(cfg.cooldown_secs)
        {
            return; // cooldown: let the previous migration settle
        }
        let (active, migrating) = {
            let map = self.map.read().await;
            let active: Vec<String> = map
                .active_shards()
                .iter()
                .map(|s| s.primary_url.clone())
                .collect();
            let migrating = map.shards().iter().any(|s| map.is_joining(s.id));
            (active, migrating)
        };
        if migrating {
            return; // never interrupt an in-flight migration
        }
        if cfg.max_shards != 0 && active.len() >= cfg.max_shards {
            return;
        }
        let mut max_points = 0u64;
        for url in &active {
            max_points = max_points.max(self.shard_points(url).await);
        }
        if max_points <= cfg.high_water_points {
            return;
        }
        let standby = self.standby.lock().await.pop();
        let Some(url) = standby else {
            tracing::warn!(
                max_points,
                "autoscale: high-water exceeded but the standby pool is empty"
            );
            return;
        };
        tracing::info!(max_points, standby = %url, "autoscale: growing the cluster");
        match self.grow_shard(url.clone(), Vec::new()).await {
            Ok(_) => *self.last_scale.lock().await = Some(std::time::Instant::now()),
            Err(e) => {
                tracing::error!(error = %e, "autoscale grow failed; returning the standby to the pool");
                self.standby.lock().await.push(url);
            }
        }
    }
}

// Build the version-0 seed map from the configured shard URLs + replica specs (the
// same `<shard_id>=<url>` form the router parses).
fn build_seed_map(config: &Config) -> Result<ShardMap, Error> {
    let mut map = ShardMap::from_urls(config.cluster_shards.clone())
        .map_err(|e| Error::Config(e.to_string()))?;
    for spec in &config.cluster_replicas {
        let (id, url) = spec.split_once('=').ok_or_else(|| {
            Error::Config(format!("replica entry {spec:?} must be \"<id>=<url>\""))
        })?;
        let id: u64 = id
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("replica entry {spec:?} has a non-numeric id")))?;
        map.add_replica(id, url)
            .map_err(|e| Error::Config(e.to_string()))?;
    }
    Ok(map)
}

/// Run the coordinator REST service on `listener` until shutdown. Routes:
/// `GET /healthz`, `GET /cluster/map`, `POST /cluster/shards`,
/// `DELETE /cluster/shards/{id}`, `GET /cluster/health`.
pub async fn serve_coordinator(config: Config, listener: TcpListener) -> Result<(), Error> {
    let state = Arc::new(CoordinatorState::bootstrap(&config)?);
    let n = state.map.read().await.len();
    tracing::info!(shards = n, "quiver cluster coordinator started");

    // Opt-in autoscale: a background task samples the load signal and grows the
    // cluster into a standby when the busiest shard crosses the high-water mark
    // (ADR-0065 increment 5).
    if state.autoscale.enabled {
        let st = state.clone();
        let interval = Duration::from_secs(state.autoscale.interval_secs.max(1));
        tracing::info!(
            interval_secs = interval.as_secs(),
            high_water = state.autoscale.high_water_points,
            standby = state.autoscale.standby_urls.len(),
            "autoscale policy enabled (scale-out)"
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                st.maybe_scale_out().await;
            }
        });
    }
    // Every route except liveness is authenticated (ADR-0011): the read-only
    // `/cluster/map` and `/cluster/health` need any valid key; the mutating shard
    // ops additionally require the `admin` role, checked in each handler. With no
    // keys configured (insecure mode, enforced at startup by `Config::validate`)
    // `authenticate` admits any caller, so a dev/loopback cluster is unchanged.
    let authed = Router::new()
        .route("/cluster/map", get(get_map))
        .route("/cluster/shards", post(add_shard))
        .route("/cluster/shards/grow", post(grow))
        .route("/cluster/shards/joining", post(add_joining_shard))
        .route("/cluster/shards/{id}/promote", post(promote_shard))
        .route("/cluster/shards/{id}", axum::routing::delete(remove_shard))
        .route("/cluster/health", get(health))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            coordinator_auth,
        ))
        .with_state(state);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .merge(authed);
    axum::serve(listener, app).await.map_err(Error::Io)
}

/// Authenticate every non-liveness coordinator request against the configured API
/// keys (ADR-0011): a 401 if the bearer token is missing or invalid. The caller's
/// [`Principal`] rides the request so a mutating handler can require the `admin`
/// role. In `insecure` mode (no keys, enforced at startup) any caller is admitted.
async fn coordinator_auth(
    State(st): State<Arc<CoordinatorState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::to_owned);
    match auth::authenticate(&st.keys, presented.as_deref()) {
        Some(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        None => {
            let body = json!({
                "type": "about:blank",
                "title": "Unauthorized",
                "status": 401,
                "detail": "missing or invalid API key",
            });
            (StatusCode::UNAUTHORIZED, Json(body)).into_response()
        }
    }
}

async fn healthz() -> &'static str {
    "ok"
}

// The authoritative versioned map — a router's refresh source.
async fn get_map(State(st): State<Arc<CoordinatorState>>) -> Json<ShardMap> {
    Json(st.map.read().await.clone())
}

#[derive(Deserialize)]
struct AddShardReq {
    primary_url: String,
    #[serde(default)]
    replica_urls: Vec<String>,
}

// Add a shard with the next monotonic id, bump the version, persist, return the new
// map. The id counter advances even on a rejected add, so an id is never reused.
async fn add_shard(
    State(st): State<Arc<CoordinatorState>>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<AddShardReq>,
) -> Result<Json<ShardMap>, Error> {
    principal.require(Action::Admin, None)?;
    let id = st.next_id.fetch_add(1, Ordering::SeqCst);
    let mut map = st.map.write().await;
    map.add_shard(id, req.primary_url, req.replica_urls)
        .map_err(|e| Error::BadRequest(e.to_string()))?;
    st.persist(&map)?;
    Ok(Json(map.clone()))
}

// Grow the cluster by one shard (ADR-0066 increment 3c-ii): add it as joining, then
// run the whole online migration — copy its slice from the donors, flip ownership,
// drop the donors' copies — in the background, so the request returns immediately
// with the joining map. The slice stays queryable and no acknowledged write is lost
// throughout (the data plane of increment 3c-i). On any failure the join is reverted.
async fn grow(
    State(st): State<Arc<CoordinatorState>>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<AddShardReq>,
) -> Result<Json<ShardMap>, Error> {
    principal.require(Action::Admin, None)?;
    let snapshot = st.grow_shard(req.primary_url, req.replica_urls).await?;
    Ok(Json(snapshot))
}

// Add a shard in the **joining** state (ADR-0066 increment 3c): it is in the map so
// HRW routes its slice to it, but the donor still serves the slice until the flip.
// Drives the start of an online migration; the data copy + `promote` flip follow.
async fn add_joining_shard(
    State(st): State<Arc<CoordinatorState>>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<AddShardReq>,
) -> Result<Json<ShardMap>, Error> {
    principal.require(Action::Admin, None)?;
    let id = st.next_id.fetch_add(1, Ordering::SeqCst);
    let mut map = st.map.write().await;
    map.add_joining_shard(id, req.primary_url, req.replica_urls)
        .map_err(|e| Error::BadRequest(e.to_string()))?;
    st.persist(&map)?;
    Ok(Json(map.clone()))
}

// Promote a joining shard to the authoritative slice owner (the migration **flip**,
// ADR-0066): the router now routes the slice to it, and the donor may drop the copy.
async fn promote_shard(
    State(st): State<Arc<CoordinatorState>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<u64>,
) -> Result<Json<ShardMap>, Error> {
    principal.require(Action::Admin, None)?;
    let mut map = st.map.write().await;
    map.promote(id)
        .map_err(|e| Error::BadRequest(e.to_string()))?;
    st.persist(&map)?;
    Ok(Json(map.clone()))
}

// Remove a shard, bump the version, persist, return the new map. (Increment 3b does
// not migrate the removed shard's data — that is increment 3c; here removal is for a
// drained or empty shard.)
async fn remove_shard(
    State(st): State<Arc<CoordinatorState>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<u64>,
) -> Result<Json<ShardMap>, Error> {
    principal.require(Action::Admin, None)?;
    let mut map = st.map.write().await;
    map.remove_shard(id)
        .map_err(|e| Error::BadRequest(e.to_string()))?;
    st.persist(&map)?;
    Ok(Json(map.clone()))
}

// Best-effort per-shard liveness: probe each primary's `/healthz`. Off the data
// path — purely for operability.
async fn health(State(st): State<Arc<CoordinatorState>>) -> impl IntoResponse {
    let shards = st.map.read().await.shards().to_vec();
    let mut out = serde_json::Map::new();
    for shard in shards {
        let url = format!("{}/healthz", shard.primary_url.trim_end_matches('/'));
        let up = matches!(st.http.get(&url).send().await, Ok(r) if r.status().is_success());
        out.insert(shard.id.to_string(), json!(up));
    }
    Json(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(shards: Vec<&str>, replicas: Vec<&str>) -> Config {
        Config {
            cluster_shards: shards.into_iter().map(String::from).collect(),
            cluster_replicas: replicas.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn build_seed_map_assigns_ids_and_attaches_replicas() {
        let map = build_seed_map(&config(
            vec!["http://s0:6333", "http://s1:6333"],
            vec!["1=http://s1b:6333"],
        ))
        .unwrap();
        assert_eq!(map.version(), 0);
        assert_eq!(
            map.shards().iter().map(|s| s.id).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(map.shards()[1].replica_urls, ["http://s1b:6333"]);
    }

    #[test]
    fn build_seed_map_rejects_malformed_replica_specs() {
        let err = |replicas| match build_seed_map(&config(vec!["http://s0"], replicas)) {
            Err(Error::Config(_)) => {}
            other => panic!("expected a Config error, got {:?}", other.map(|_| "Ok")),
        };
        err(vec!["http://no-equals"]); // missing `<id>=`
        err(vec!["x=http://s"]); // non-numeric id
        err(vec!["9=http://s"]); // unknown shard id
    }

    #[test]
    fn persisted_state_round_trips() {
        let mut map = ShardMap::from_urls(["http://s0"]).unwrap();
        map.add_shard(1, "http://s1", vec![]).unwrap();
        let json = serde_json::to_vec(&Persisted { next_id: 2, map }).unwrap();
        let back: Persisted = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.next_id, 2);
        assert_eq!(back.map.version(), 1);
        assert_eq!(back.map.len(), 2);
    }
}
