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
    // An HTTP client for shard health probes.
    http: reqwest::Client,
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
            });
        }
        let map = build_seed_map(config)?;
        let next_id = map.len() as u64; // ids 0..N are taken; the next free id is N
        let state = Self {
            map: RwLock::new(map),
            next_id: AtomicU64::new(next_id),
            path,
            http: reqwest::Client::new(),
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
