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

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Router, response::IntoResponse};
use quiver_cluster::ShardMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use crate::Config;
use crate::error::Error;

// A grace window ≥ the router's map-refresh interval, so every router has adopted the
// new map version (and its dual-write / routing) before the coordinator relies on it
// — once before the copy (dual-write active), once after the flip (slice routed to
// the new shard) before dropping the donors' copies.
const MIGRATION_GRACE: Duration = Duration::from_secs(3);
// Page size for scrolling a donor's points during the copy.
const COPY_PAGE: usize = 1_000;

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
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/cluster/map", get(get_map))
        .route("/cluster/shards", post(add_shard))
        .route("/cluster/shards/grow", post(grow))
        .route("/cluster/shards/joining", post(add_joining_shard))
        .route("/cluster/shards/{id}/promote", post(promote_shard))
        .route("/cluster/shards/{id}", axum::routing::delete(remove_shard))
        .route("/cluster/health", get(health))
        .with_state(state);
    axum::serve(listener, app).await.map_err(Error::Io)
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
    Json(req): Json<AddShardReq>,
) -> Result<Json<ShardMap>, Error> {
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
    Json(req): Json<AddShardReq>,
) -> Result<Json<ShardMap>, Error> {
    let id = st.next_id.fetch_add(1, Ordering::SeqCst);
    let snapshot = {
        let mut map = st.map.write().await;
        map.add_joining_shard(id, &req.primary_url, req.replica_urls.clone())
            .map_err(|e| Error::BadRequest(e.to_string()))?;
        st.persist(&map)?;
        map.clone()
    };
    let bg = st.clone();
    tokio::spawn(async move {
        if let Err(e) = bg.run_migration(id).await {
            tracing::error!(shard = id, error = %e, "cluster migration failed; reverting the join");
            let mut map = bg.map.write().await;
            let _ = map.remove_shard(id);
            let _ = bg.persist(&map);
        }
    });
    Ok(Json(snapshot))
}

// Add a shard in the **joining** state (ADR-0066 increment 3c): it is in the map so
// HRW routes its slice to it, but the donor still serves the slice until the flip.
// Drives the start of an online migration; the data copy + `promote` flip follow.
async fn add_joining_shard(
    State(st): State<Arc<CoordinatorState>>,
    Json(req): Json<AddShardReq>,
) -> Result<Json<ShardMap>, Error> {
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
    Path(id): Path<u64>,
) -> Result<Json<ShardMap>, Error> {
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
    Path(id): Path<u64>,
) -> Result<Json<ShardMap>, Error> {
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
