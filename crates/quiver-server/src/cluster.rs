// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in cluster router (ADR-0065, increment 1): when `QUIVER_CLUSTER_SHARDS` is
//! set, the server runs as a stateless **router** in front of N shard servers.
//! Single-shard ops (upsert/get/delete) route by the point id's owning shard;
//! searches **scatter-gather** to every shard and merge the global top-`k`;
//! collection ops broadcast. Each shard is an ordinary `quiver serve`.
//!
//! The shard map is held behind an [`ArcSwap`] so a later increment can refresh it
//! (dynamic, elastic membership — ADR-0065) without restarting the router; here it
//! is seeded once from the operator-declared shard URLs. Scatter is sequential for
//! now — correct and simple; concurrent fan-out is a perf follow-up.
//! `ponytail`: sequential scatter, parallelise when shard count / latency matters.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use quiver_cluster::{ShardMap, merge_top_k};

/// How often a router polls the coordinator for a newer shard map (ADR-0066).
/// Fixed: membership changes are rare and reads self-correct, so a few seconds of
/// staleness is fine.
// ponytail: fixed interval; make it configurable if an operator ever needs to tune it.
pub(crate) const MAP_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
use quiver_embed::{
    DistanceMetric, Filter, FilterableField, IndexKind, IndexSpec, VectorEncryption,
};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::error::Error;
use crate::{CollectionInfo, MatchOut, PointIn, PointOut};

/// The router's view of the cluster: the (refreshable) shard map, an HTTP client,
/// an optional API key presented to shards, and a small cache of each collection's
/// score ordering (for the scatter-gather merge).
pub(crate) struct Cluster {
    map: ArcSwap<ShardMap>,
    http: reqwest::Client,
    shard_key: Option<String>,
    // collection -> higher_is_better (cosine/dot = true, L2 = false), learned on
    // create or by describing a shard, so a search knows how to merge.
    ordering: RwLock<HashMap<String, bool>>,
    // Monotonic counter that round-robins each shard's reads across {primary} ∪
    // replicas (ADR-0065 increment 2). Relaxed: an exact sequence is not required,
    // only an even spread.
    read_rr: AtomicUsize,
}

impl Cluster {
    /// Build the router from operator-declared shard primary URLs, optional
    /// per-shard read replicas (each `"<shard_id>=<replica_url>"`, e.g.
    /// `QUIVER_CLUSTER_REPLICAS`; the shard id is its 0-based position in
    /// `QUIVER_CLUSTER_SHARDS`), and an optional key presented to the shards (a
    /// cluster runs over a trusted network, like replication — ADR-0030).
    pub(crate) fn new(
        shard_urls: Vec<String>,
        replica_specs: Vec<String>,
        shard_key: Option<String>,
    ) -> Result<Self, Error> {
        let mut map = ShardMap::from_urls(shard_urls).map_err(|e| Error::Config(e.to_string()))?;
        for spec in &replica_specs {
            let (index, url) = spec.split_once('=').ok_or_else(|| {
                Error::Config(format!(
                    "QUIVER_CLUSTER_REPLICAS entry {spec:?} must be \"<shard_index>=<url>\""
                ))
            })?;
            let index: u64 = index.trim().parse().map_err(|_| {
                Error::Config(format!("replica entry {spec:?} has a non-numeric shard id"))
            })?;
            map.add_replica(index, url)
                .map_err(|e| Error::Config(e.to_string()))?;
        }
        Ok(Self {
            map: ArcSwap::from_pointee(map),
            http: reqwest::Client::new(),
            shard_key,
            ordering: RwLock::new(HashMap::new()),
            read_rr: AtomicUsize::new(0),
        })
    }

    /// Number of shards (for `/healthz` / diagnostics).
    pub(crate) fn shard_count(&self) -> usize {
        self.map.load().len()
    }

    /// A snapshot of the router's currently adopted shard map (version + shards), for
    /// the read-only `GET /cluster/map` ops endpoint — so an operator (or a test) can
    /// see which map version a router has refreshed to.
    pub(crate) fn current_map(&self) -> ShardMap {
        ShardMap::clone(&self.map.load())
    }

    /// Refresh the shard map from the coordinator (ADR-0066): fetch
    /// `GET {coordinator}/cluster/map` and adopt it **only if its version is newer**
    /// than the one held, swapping it into the `ArcSwap` with no restart. Returns
    /// `true` if a newer map was adopted. A stale or equal version is ignored so an
    /// out-of-order or duplicate response can never move the map backwards.
    pub(crate) async fn refresh_from(&self, coordinator_url: &str) -> Result<bool, Error> {
        let url = format!("{}/cluster/map", coordinator_url.trim_end_matches('/'));
        let body = self.send(reqwest::Method::GET, url, None).await?;
        let new_map: ShardMap = serde_json::from_value(body)
            .map_err(|e| Error::Internal(format!("coordinator map: {e}")))?;
        if new_map.version() > self.map.load().version() {
            self.map.store(std::sync::Arc::new(new_map));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // --- HTTP plumbing -----------------------------------------------------

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.shard_key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }

    // Send a request to one shard and return its JSON body, mapping a transport
    // failure or a non-2xx response to a server error (the shard's message is
    // surfaced so a misconfiguration is legible).
    async fn send(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, Error> {
        let mut rb = self.http.request(method, &url);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = self
            .auth(rb)
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
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| Error::Internal(format!("shard {url} bad response: {e}")))
    }

    // Query one shard, trying its read targets ({primary} ∪ replicas) in the
    // round-robin order for `nth`. The first reachable target answers; if a target
    // is down (a stopped/stale replica, or — reads only — a stopped primary) the
    // next is tried. The shard's slice is unavailable, and the error surfaced, only
    // if every target is down.
    async fn shard_query(
        &self,
        shard: &quiver_cluster::Shard,
        nth: usize,
        collection: &str,
        body: &Value,
    ) -> Result<Value, Error> {
        let targets = shard.read_order(nth);
        let mut last_err = None;
        for (i, target) in targets.iter().enumerate() {
            let url = format!("{target}/v1/collections/{collection}/query");
            match self
                .send(reqwest::Method::POST, url, Some(body.clone()))
                .await
            {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if i + 1 < targets.len() {
                        tracing::warn!(target, error = %e, "shard read target failed; trying next");
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Internal("shard has no read targets".into())))
    }

    // Send the same request to every shard (collection broadcast), returning the
    // first body. Any shard failure fails the whole op so the cluster never ends up
    // with a collection on only some shards.
    async fn broadcast(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, Error> {
        let map = self.map.load();
        let mut last = Value::Null;
        for shard in map.shards() {
            last = self
                .send(
                    method.clone(),
                    format!("{}{path}", shard.primary_url),
                    body.clone(),
                )
                .await?;
        }
        Ok(last)
    }

    // --- Collection ops (broadcast) ----------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_collection(
        &self,
        name: String,
        dim: u32,
        metric: DistanceMetric,
        index: IndexSpec,
        filterable: Vec<FilterableField>,
        multivector: bool,
        vector_encryption: VectorEncryption,
    ) -> Result<CollectionInfo, Error> {
        let mut body = json!({
            "name": name,
            "dim": dim,
            "metric": metric_wire(metric),
            "index": index_wire(index.kind),
            "multivector": multivector,
            "vector_encryption": encryption_wire(vector_encryption),
        });
        if let Some(pq) = index.pq_subspaces {
            body["pq_subspaces"] = json!(pq);
        }
        if !filterable.is_empty() {
            body["filterable"] = json!(
                filterable
                    .iter()
                    .map(|f| json!({ "path": f.path, "type": field_type_wire(f.field_type) }))
                    .collect::<Vec<_>>()
            );
        }
        self.broadcast(reqwest::Method::POST, "/v1/collections", Some(body))
            .await?;
        // Remember the score ordering so a later search can merge correctly.
        self.ordering
            .write()
            .await
            .insert(name.clone(), higher_is_better(metric));
        Ok(CollectionInfo {
            name,
            dim,
            metric,
            count: 0,
            index,
            filterable,
            multivector,
            vector_encryption,
        })
    }

    pub(crate) async fn drop_collection(&self, name: &str) -> Result<bool, Error> {
        self.broadcast(
            reqwest::Method::DELETE,
            &format!("/v1/collections/{name}"),
            None,
        )
        .await?;
        self.ordering.write().await.remove(name);
        Ok(true)
    }

    // --- Writes (route by point id) ----------------------------------------

    pub(crate) async fn upsert(
        &self,
        collection: &str,
        points: Vec<PointIn>,
    ) -> Result<u64, Error> {
        self.upsert_to(collection, points, "points").await
    }

    pub(crate) async fn upsert_bulk(
        &self,
        collection: &str,
        points: Vec<PointIn>,
    ) -> Result<u64, Error> {
        self.upsert_to(collection, points, "points:bulk").await
    }

    async fn upsert_to(
        &self,
        collection: &str,
        points: Vec<PointIn>,
        endpoint: &str,
    ) -> Result<u64, Error> {
        let map = self.map.load();
        // Each point goes to its HRW owner; the returned count is the logical upsert.
        let mut total = 0u64;
        for (shard, group) in map.partition(&points, |p| p.id.as_str()) {
            total += self
                .post_points(collection, endpoint, &shard.primary_url, &group)
                .await?;
        }
        // Migration dual-write (ADR-0066 increment 3c): a point whose owner is a
        // *joining* shard is also written to the donor that still serves the slice, so
        // a write during migration is on both and the flip loses nothing. The donor
        // copy is not counted (it is the same logical point). Empty when no migration
        // is in flight, so a steady-state cluster does no extra work.
        for (donor, group) in map.partition_to_donors(&points, |p| p.id.as_str()) {
            self.post_points(collection, endpoint, &donor.primary_url, &group)
                .await?;
        }
        Ok(total)
    }

    // POST a group of points to one shard, returning its `upserted` count.
    async fn post_points(
        &self,
        collection: &str,
        endpoint: &str,
        primary_url: &str,
        group: &[&PointIn],
    ) -> Result<u64, Error> {
        let dtos: Vec<Value> = group
            .iter()
            .map(|p| json!({ "id": p.id, "vector": p.vector, "payload": p.payload }))
            .collect();
        let url = format!("{primary_url}/v1/collections/{collection}/{endpoint}");
        let resp = self
            .send(reqwest::Method::POST, url, Some(json!({ "points": dtos })))
            .await?;
        Ok(resp.get("upserted").and_then(Value::as_u64).unwrap_or(0))
    }

    pub(crate) async fn delete_points(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> Result<u64, Error> {
        let map = self.map.load();
        let mut total = 0u64;
        for (shard, group) in map.partition(&ids, |id| id.as_str()) {
            total += self
                .delete_group(collection, &shard.primary_url, &group)
                .await?;
        }
        // Migration dual-delete (ADR-0066 increment 3c): also remove from the donor so
        // a delete during a join is not resurrected by the slice copy. Not counted.
        for (donor, group) in map.partition_to_donors(&ids, |id| id.as_str()) {
            self.delete_group(collection, &donor.primary_url, &group)
                .await?;
        }
        Ok(total)
    }

    // DELETE a group of ids from one shard, returning its `deleted` count.
    async fn delete_group(
        &self,
        collection: &str,
        primary_url: &str,
        group: &[&String],
    ) -> Result<u64, Error> {
        let url = format!("{primary_url}/v1/collections/{collection}/points");
        let resp = self
            .send(reqwest::Method::DELETE, url, Some(json!({ "ids": group })))
            .await?;
        Ok(resp.get("deleted").and_then(Value::as_u64).unwrap_or(0))
    }

    // --- Reads -------------------------------------------------------------

    pub(crate) async fn get_points(
        &self,
        collection: &str,
        ids: Vec<String>,
        with_vector: bool,
    ) -> Result<Vec<PointOut>, Error> {
        let map = self.map.load();
        let mut out = Vec::new();
        for id in &ids {
            // Gets go to the authoritative primary: the donor while the owner is a
            // joining shard mid-migration (ADR-0066), else the HRW owner. Replicas are
            // eventually consistent — they serve searches, not point lookups.
            let shard = map.donor_for(id).unwrap_or_else(|| map.shard_for(id));
            let url = format!(
                "{}/v1/collections/{collection}/points/{id}",
                shard.primary_url
            );
            let resp = match self.send(reqwest::Method::GET, url, None).await {
                Ok(v) => v,
                // A 404 (point not found) surfaces as an error from `send`; treat a
                // missing point as "skip", any other failure as fatal.
                Err(Error::Internal(msg)) if msg.contains("404") => continue,
                Err(e) => return Err(e),
            };
            if let Some(p) = point_from_json(&resp, with_vector) {
                out.push(p);
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        k: usize,
        filter: Option<Filter>,
        ef_search: usize,
        with_payload: bool,
        with_vector: bool,
    ) -> Result<Vec<MatchOut>, Error> {
        let higher = self.higher_is_better(collection).await?;
        let mut body = json!({
            "vector": vector,
            "k": k,
            "ef_search": ef_search,
            "with_payload": with_payload,
            "with_vector": with_vector,
        });
        if let Some(f) = &filter {
            body["filter"] =
                serde_json::to_value(f).map_err(|e| Error::BadRequest(e.to_string()))?;
        }
        // Scatter: ask each shard for its local top-k, round-robining reads across
        // {primary} ∪ replicas (ADR-0065 increment 2) to spread read load. The base
        // advances once per search and is offset by the shard index so every shard
        // sweeps its own targets (rather than aliasing to one). A search is
        // eventually consistent — a replica may lag its primary (ADR-0030).
        let map = self.map.load();
        let base = self.read_rr.fetch_add(1, Ordering::Relaxed);
        // Scatter only to *active* shards: a joining shard is excluded because its
        // donor still holds the authoritative slice (ADR-0066 increment 3c) — querying
        // both would double-count. With no migration in flight this is every shard.
        let active = map.active_shards();
        let mut per_shard: Vec<Vec<MatchOut>> = Vec::with_capacity(active.len());
        for shard in &active {
            let resp = self
                .shard_query(
                    shard,
                    base.wrapping_add(shard.id as usize),
                    collection,
                    &body,
                )
                .await?;
            per_shard.push(matches_from_json(&resp, with_vector));
        }
        // Gather: dedup by id, then merge to the exact global top-k by score. The
        // dedup absorbs the brief post-flip window where a just-promoted shard and its
        // donor both still hold a slice point, so it is never double-counted.
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<MatchOut> = per_shard
            .into_iter()
            .flatten()
            .filter(|m| seen.insert(m.id.clone()))
            .collect();
        Ok(merge_top_k(vec![deduped], k, |m| m.score, higher))
    }

    // The score ordering for `collection` (cached on create; learned by describing
    // a shard on a cold router).
    async fn higher_is_better(&self, collection: &str) -> Result<bool, Error> {
        if let Some(h) = self.ordering.read().await.get(collection).copied() {
            return Ok(h);
        }
        let map = self.map.load();
        let shard = map
            .shards()
            .first()
            .ok_or_else(|| Error::Internal("no shards".into()))?;
        let url = format!("{}/v1/collections/{collection}", shard.primary_url);
        let info = self.send(reqwest::Method::GET, url, None).await?;
        let metric = info.get("metric").and_then(Value::as_str).unwrap_or("l2");
        let higher = matches!(metric, "cosine" | "dot");
        self.ordering
            .write()
            .await
            .insert(collection.to_owned(), higher);
        Ok(higher)
    }
}

// --- wire encodings (match the shard REST API) -----------------------------

fn metric_wire(m: DistanceMetric) -> &'static str {
    match m {
        DistanceMetric::L2 => "l2",
        DistanceMetric::Cosine => "cosine",
        DistanceMetric::Dot => "dot",
    }
}

fn higher_is_better(m: DistanceMetric) -> bool {
    !matches!(m, DistanceMetric::L2)
}

fn index_wire(k: IndexKind) -> &'static str {
    match k {
        IndexKind::Hnsw => "hnsw",
        IndexKind::Vamana => "vamana",
        IndexKind::DiskVamana => "disk_vamana",
        IndexKind::Ivf => "ivf",
        IndexKind::Colbert => "colbert",
        // `IndexKind` is `#[non_exhaustive]`; a new variant needs a wire mapping
        // here. Default to the most common kind until one is added.
        _ => "hnsw",
    }
}

fn encryption_wire(e: VectorEncryption) -> &'static str {
    match e {
        VectorEncryption::None => "none",
        VectorEncryption::Dcpe => "dcpe",
        VectorEncryption::ClientSide => "client_side",
    }
}

fn field_type_wire(t: quiver_embed::FieldType) -> &'static str {
    match t {
        quiver_embed::FieldType::Keyword => "keyword",
        quiver_embed::FieldType::Numeric => "numeric",
        // `FieldType` is `#[non_exhaustive]`; default to keyword for a future kind.
        _ => "keyword",
    }
}

// Parse one shard `{matches: [...]}` query response into `MatchOut`s.
fn matches_from_json(resp: &Value, with_vector: bool) -> Vec<MatchOut> {
    resp.get("matches")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(MatchOut {
                        id: m.get("id")?.as_str()?.to_owned(),
                        score: m.get("score")?.as_f64()? as f32,
                        payload: m.get("payload").cloned(),
                        vector: if with_vector {
                            m.get("vector").and_then(Value::as_array).map(|v| {
                                v.iter()
                                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                                    .collect()
                            })
                        } else {
                            None
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// Parse one shard point-fetch response into a `PointOut`.
fn point_from_json(resp: &Value, with_vector: bool) -> Option<PointOut> {
    Some(PointOut {
        id: resp.get("id")?.as_str()?.to_owned(),
        payload: resp.get("payload").cloned().unwrap_or(Value::Null),
        vector: if with_vector {
            resp.get("vector").and_then(Value::as_array).map(|v| {
                v.iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect()
            })
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards() -> Vec<String> {
        vec!["http://s0:6333".into(), "http://s1:6333".into()]
    }

    #[test]
    fn new_accepts_well_formed_replicas() {
        let c = Cluster::new(
            shards(),
            vec!["0=http://s0b:6333".into(), "1=http://s1b:6333".into()],
            None,
        )
        .unwrap();
        assert_eq!(c.shard_count(), 2);
        let map = c.map.load();
        assert_eq!(map.shards()[0].replica_urls, ["http://s0b:6333"]);
        assert_eq!(map.shards()[1].replica_urls, ["http://s1b:6333"]);
        // No replicas configured is fine too (primary-only shards).
        assert!(Cluster::new(shards(), vec![], None).is_ok());
    }

    #[test]
    fn new_rejects_malformed_replica_specs() {
        // `Cluster` is not `Debug`, so assert on the error arm without unwrapping Ok.
        let config_err = |spec: &str| match Cluster::new(shards(), vec![spec.into()], None) {
            Err(Error::Config(_)) => {}
            Err(e) => panic!("expected a Config error, got {e:?}"),
            Ok(_) => panic!("expected a Config error for {spec:?}, built a router"),
        };
        config_err("http://nope"); // missing the `index=` prefix
        config_err("x=http://nope"); // non-numeric shard index
        config_err("9=http://nope"); // index past the shard set (add_replica rejects)
    }
}
