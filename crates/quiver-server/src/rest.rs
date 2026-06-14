// SPDX-License-Identifier: AGPL-3.0-only
//! The REST surface (axum): the JSON mirror of the gRPC contract
//! (`docs/api/rest-grpc.md`).

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use quiver_embed::{DistanceMetric, FieldType, FilterableField, IndexKind, IndexSpec};
use quiver_query::Filter;

use crate::auth::Principal;
use crate::{AppState, CollectionInfo, Error, MatchOut, PointIn, PointOut};

/// Build the REST router: open `/healthz`, `/readyz`, `/metrics`; the `/v1` API
/// behind API-key auth.
pub(crate) fn router(state: AppState) -> Router {
    let api = Router::new()
        .route(
            "/v1/collections",
            post(create_collection).get(list_collections),
        )
        .route(
            "/v1/collections/{name}",
            get(get_collection).delete(delete_collection),
        )
        .route(
            "/v1/collections/{name}/points",
            post(upsert).delete(delete_points),
        )
        .route("/v1/collections/{name}/points/{id}", get(get_point))
        .route("/v1/collections/{name}/query", post(search))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .merge(api)
}

async fn auth(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let presented: Option<String> = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::to_owned);
    match state.authenticate(presented.as_deref()) {
        // The caller's scope rides the request; each op authorizes against it.
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

async fn readyz() -> &'static str {
    "ready"
}

async fn metrics() -> &'static str {
    // A Prometheus exposition endpoint is wired with the observability work;
    // for now this is a stable, scrapable placeholder.
    "# quiver metrics\n"
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum MetricDto {
    #[default]
    L2,
    Cosine,
    Dot,
}

impl From<MetricDto> for DistanceMetric {
    fn from(m: MetricDto) -> Self {
        match m {
            MetricDto::L2 => DistanceMetric::L2,
            MetricDto::Cosine => DistanceMetric::Cosine,
            MetricDto::Dot => DistanceMetric::Dot,
        }
    }
}

impl From<DistanceMetric> for MetricDto {
    fn from(m: DistanceMetric) -> Self {
        match m {
            DistanceMetric::L2 => MetricDto::L2,
            DistanceMetric::Cosine => MetricDto::Cosine,
            DistanceMetric::Dot => MetricDto::Dot,
        }
    }
}

/// The index structure, in REST JSON (`hnsw` | `vamana` | `disk_vamana` | `ivf`).
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum IndexKindDto {
    #[default]
    Hnsw,
    Vamana,
    DiskVamana,
    Ivf,
}

impl From<IndexKindDto> for IndexKind {
    fn from(k: IndexKindDto) -> Self {
        match k {
            IndexKindDto::Hnsw => IndexKind::Hnsw,
            IndexKindDto::Vamana => IndexKind::Vamana,
            IndexKindDto::DiskVamana => IndexKind::DiskVamana,
            IndexKindDto::Ivf => IndexKind::Ivf,
        }
    }
}

impl From<IndexKind> for IndexKindDto {
    fn from(k: IndexKind) -> Self {
        match k {
            IndexKind::Vamana => IndexKindDto::Vamana,
            IndexKind::DiskVamana => IndexKindDto::DiskVamana,
            IndexKind::Ivf => IndexKindDto::Ivf,
            _ => IndexKindDto::Hnsw,
        }
    }
}

/// A filterable field's value type, in REST JSON (`keyword` | `numeric`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FieldTypeDto {
    Keyword,
    Numeric,
}

impl From<FieldTypeDto> for FieldType {
    fn from(t: FieldTypeDto) -> Self {
        match t {
            FieldTypeDto::Keyword => FieldType::Keyword,
            FieldTypeDto::Numeric => FieldType::Numeric,
        }
    }
}

impl From<FieldType> for FieldTypeDto {
    fn from(t: FieldType) -> Self {
        match t {
            FieldType::Numeric => FieldTypeDto::Numeric,
            _ => FieldTypeDto::Keyword,
        }
    }
}

/// A payload field declared filterable at creation, in REST JSON.
#[derive(Serialize, Deserialize, Clone)]
struct FilterableFieldDto {
    path: String,
    field_type: FieldTypeDto,
}

impl From<FilterableFieldDto> for FilterableField {
    fn from(f: FilterableFieldDto) -> Self {
        FilterableField {
            path: f.path,
            field_type: f.field_type.into(),
        }
    }
}

impl From<FilterableField> for FilterableFieldDto {
    fn from(f: FilterableField) -> Self {
        Self {
            path: f.path,
            field_type: f.field_type.into(),
        }
    }
}

#[derive(Serialize)]
struct CollectionDto {
    name: String,
    dim: u32,
    metric: MetricDto,
    count: u64,
    index: IndexKindDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pq_subspaces: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    filterable: Vec<FilterableFieldDto>,
}

impl From<CollectionInfo> for CollectionDto {
    fn from(info: CollectionInfo) -> Self {
        Self {
            name: info.name,
            dim: info.dim,
            metric: info.metric.into(),
            count: info.count,
            index: info.index.kind.into(),
            pq_subspaces: info.index.pq_subspaces,
            filterable: info.filterable.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize)]
struct CreateCollectionBody {
    name: String,
    dim: u32,
    #[serde(default)]
    metric: MetricDto,
    #[serde(default)]
    index: IndexKindDto,
    #[serde(default)]
    pq_subspaces: Option<u32>,
    #[serde(default)]
    filterable: Vec<FilterableFieldDto>,
}

async fn create_collection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<CreateCollectionBody>,
) -> Result<Json<CollectionDto>, Error> {
    let index = IndexSpec {
        kind: body.index.into(),
        pq_subspaces: body.pq_subspaces,
    };
    let filterable = body.filterable.into_iter().map(Into::into).collect();
    let info = state
        .create_collection(
            &principal,
            body.name,
            body.dim,
            body.metric.into(),
            index,
            filterable,
        )
        .await?;
    Ok(Json(info.into()))
}

async fn get_collection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Json<CollectionDto>, Error> {
    let info = state.get_collection(&principal, name).await?;
    Ok(Json(info.into()))
}

async fn list_collections(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Vec<CollectionDto>>, Error> {
    let infos = state.list_collections(&principal).await?;
    Ok(Json(infos.into_iter().map(CollectionDto::from).collect()))
}

#[derive(Serialize)]
struct DeleteCollectionResponse {
    existed: bool,
}

async fn delete_collection(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Json<DeleteCollectionResponse>, Error> {
    let existed = state.delete_collection(&principal, name).await?;
    Ok(Json(DeleteCollectionResponse { existed }))
}

#[derive(Deserialize)]
struct PointDto {
    id: String,
    vector: Vec<f32>,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct UpsertBody {
    points: Vec<PointDto>,
}

#[derive(Serialize)]
struct UpsertResponse {
    upserted: u64,
}

async fn upsert(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<UpsertBody>,
) -> Result<Json<UpsertResponse>, Error> {
    let points = body
        .points
        .into_iter()
        .map(|p| PointIn {
            id: p.id,
            vector: p.vector,
            payload: p.payload,
        })
        .collect();
    let upserted = state.upsert(&principal, name, points).await?;
    Ok(Json(UpsertResponse { upserted }))
}

#[derive(Deserialize)]
struct DeletePointsBody {
    ids: Vec<String>,
}

#[derive(Serialize)]
struct DeletePointsResponse {
    deleted: u64,
}

async fn delete_points(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<DeletePointsBody>,
) -> Result<Json<DeletePointsResponse>, Error> {
    let deleted = state.delete_points(&principal, name, body.ids).await?;
    Ok(Json(DeletePointsResponse { deleted }))
}

#[derive(Serialize)]
struct PointResponse {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
    payload: Value,
}

impl From<PointOut> for PointResponse {
    fn from(p: PointOut) -> Self {
        Self {
            id: p.id,
            vector: p.vector,
            payload: p.payload,
        }
    }
}

async fn get_point(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((name, id)): Path<(String, String)>,
) -> Result<Response, Error> {
    let mut points = state.get_points(&principal, name, vec![id], true).await?;
    match points.pop() {
        Some(point) => Ok(Json(PointResponse::from(point)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

fn default_k() -> usize {
    10
}
fn default_ef() -> usize {
    64
}
fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct SearchBody {
    vector: Vec<f32>,
    #[serde(default = "default_k")]
    k: usize,
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default = "default_ef")]
    ef_search: usize,
    #[serde(default = "default_true")]
    with_payload: bool,
    #[serde(default)]
    with_vector: bool,
}

#[derive(Serialize)]
struct MatchDto {
    id: String,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
}

impl From<MatchOut> for MatchDto {
    fn from(m: MatchOut) -> Self {
        Self {
            id: m.id,
            score: m.score,
            payload: m.payload,
            vector: m.vector,
        }
    }
}

#[derive(Serialize)]
struct SearchResponse {
    matches: Vec<MatchDto>,
}

async fn search(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<SearchBody>,
) -> Result<Json<SearchResponse>, Error> {
    let matches = state
        .search(
            &principal,
            name,
            body.vector,
            body.k,
            body.filter,
            body.ef_search,
            body.with_payload,
            body.with_vector,
        )
        .await?;
    Ok(Json(SearchResponse {
        matches: matches.into_iter().map(MatchDto::from).collect(),
    }))
}
