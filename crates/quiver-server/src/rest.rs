// SPDX-License-Identifier: AGPL-3.0-only
//! The REST surface (axum): the JSON mirror of the gRPC contract
//! (`docs/api/rest-grpc.md`).

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use quiver_embed::{
    DistanceMetric, FieldType, FilterableField, IndexKind, IndexSpec, VectorEncryption,
};
use quiver_query::Filter;

use crate::auth::Principal;
use crate::{
    AppState, CollectionInfo, DocumentIn, DocumentMatchOut, Error, MatchOut, PointIn, PointOut,
    RateDecision, RateLimitSnapshot, TextPointIn,
};

/// Build the REST router: open `/healthz`, `/readyz`, `/metrics`; the `/v1` API
/// behind API-key auth.
pub(crate) fn router(state: AppState) -> Router {
    // Reject an oversized request body before it is buffered or parsed (ADR-0040).
    let max_body = state.limits.max_request_body_bytes;
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
        .route("/v1/collections/{name}/points:bulk", post(upsert_bulk))
        .route("/v1/collections/{name}/points:text", post(upsert_text))
        .route("/v1/collections/{name}/points/{id}", get(get_point))
        .route("/v1/collections/{name}/query", post(search))
        .route("/v1/collections/{name}/query/hybrid", post(hybrid_search))
        .route("/v1/collections/{name}/query/text", post(search_text))
        .route("/v1/collections/{name}/fetch", post(fetch))
        .route(
            "/v1/collections/{name}/documents",
            post(upsert_documents).delete(delete_documents),
        )
        .route(
            "/v1/collections/{name}/documents/query",
            post(search_multi_vector),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(max_body))
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
            // Per-key rate limit (ADR-0049): consume a token before the handler
            // runs; 429 if the key is over its rate, else carry the snapshot to the
            // response headers.
            let snapshot = if state.rate_limit_enabled() {
                match state.rate_limit(principal.actor()) {
                    RateDecision::Limited {
                        retry_after_secs,
                        limit,
                    } => return rate_limited_response(retry_after_secs, limit),
                    RateDecision::Allowed(s) => Some(s),
                }
            } else {
                None
            };
            request.extensions_mut().insert(principal);
            let mut response = next.run(request).await;
            if let Some(s) = snapshot {
                set_rate_limit_headers(response.headers_mut(), s);
            }
            response
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

// Attach the standard `RateLimit-*` headers (ADR-0049) to a response.
fn set_rate_limit_headers(headers: &mut axum::http::HeaderMap, s: RateLimitSnapshot) {
    use axum::http::HeaderValue;
    headers.insert("RateLimit-Limit", HeaderValue::from(s.limit));
    headers.insert("RateLimit-Remaining", HeaderValue::from(s.remaining));
    headers.insert("RateLimit-Reset", HeaderValue::from(s.reset_secs));
}

// A 429 with `Retry-After` and the `RateLimit-*` headers for a key over its rate.
fn rate_limited_response(retry_after_secs: u64, limit: u32) -> Response {
    let body = json!({
        "type": "about:blank",
        "title": "Too Many Requests",
        "status": 429,
        "detail": "rate limit exceeded for this API key; retry after the indicated delay",
    });
    let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    set_rate_limit_headers(
        response.headers_mut(),
        RateLimitSnapshot {
            limit,
            remaining: 0,
            reset_secs: retry_after_secs,
        },
    );
    use axum::http::HeaderValue;
    response
        .headers_mut()
        .insert("Retry-After", HeaderValue::from(retry_after_secs));
    response
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
    Colbert,
}

impl From<IndexKindDto> for IndexKind {
    fn from(k: IndexKindDto) -> Self {
        match k {
            IndexKindDto::Hnsw => IndexKind::Hnsw,
            IndexKindDto::Vamana => IndexKind::Vamana,
            IndexKindDto::DiskVamana => IndexKind::DiskVamana,
            IndexKindDto::Ivf => IndexKind::Ivf,
            IndexKindDto::Colbert => IndexKind::Colbert,
        }
    }
}

impl From<IndexKind> for IndexKindDto {
    fn from(k: IndexKind) -> Self {
        match k {
            IndexKind::Vamana => IndexKindDto::Vamana,
            IndexKind::DiskVamana => IndexKindDto::DiskVamana,
            IndexKind::Ivf => IndexKindDto::Ivf,
            IndexKind::Colbert => IndexKindDto::Colbert,
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
    #[serde(skip_serializing_if = "is_false")]
    multivector: bool,
    #[serde(skip_serializing_if = "is_none_encryption")]
    vector_encryption: VectorEncryption,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_none_encryption(v: &VectorEncryption) -> bool {
    *v == VectorEncryption::None
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
            multivector: info.multivector,
            vector_encryption: info.vector_encryption,
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
    #[serde(default)]
    multivector: bool,
    #[serde(default)]
    vector_encryption: VectorEncryption,
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
            body.multivector,
            body.vector_encryption,
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

/// Bulk upsert (ADR-0045): same body as `upsert`, but routed to the deferred
/// single-rebuild path with the larger `max_bulk_batch_size` cap. The request is
/// still bounded by `max_request_body_bytes`.
async fn upsert_bulk(
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
    let upserted = state.upsert_bulk(&principal, name, points).await?;
    Ok(Json(UpsertResponse { upserted }))
}

#[derive(Deserialize)]
struct TextPointDto {
    id: String,
    text: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct UpsertTextBody {
    points: Vec<TextPointDto>,
}

/// Embed each point's `text` server-side and upsert it (ADR-0047). Requires an
/// `[embedding.<collection>]` provider; the text is also indexed for BM25.
async fn upsert_text(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<UpsertTextBody>,
) -> Result<Json<UpsertResponse>, Error> {
    let points = body
        .points
        .into_iter()
        .map(|p| TextPointIn {
            id: p.id,
            text: p.text,
            payload: p.payload,
        })
        .collect();
    let upserted = state.upsert_text(&principal, name, points).await?;
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

fn default_rrf_k0() -> f32 {
    quiver_embed::DEFAULT_RRF_K0
}

#[derive(Deserialize)]
struct HybridSearchBody {
    /// Dense query vector (omit for pure-sparse search).
    #[serde(default)]
    vector: Option<Vec<f32>>,
    /// Sparse query vector — parallel `sparse_indices` / `sparse_values` (omit for
    /// pure-dense search). At least one of `vector` / `sparse_indices` is required.
    #[serde(default)]
    sparse_indices: Option<Vec<u32>>,
    #[serde(default)]
    sparse_values: Option<Vec<f32>>,
    /// Full-text query — tokenized server-side and scored by BM25 over the inverted
    /// index (ADR-0046). Omit for non-lexical search.
    #[serde(default)]
    query_text: Option<String>,
    #[serde(default = "default_k")]
    k: usize,
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default = "default_ef")]
    ef_search: usize,
    #[serde(default = "default_rrf_k0")]
    rrf_k0: f32,
    #[serde(default = "default_true")]
    with_payload: bool,
    #[serde(default)]
    with_vector: bool,
}

async fn hybrid_search(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<HybridSearchBody>,
) -> Result<Json<SearchResponse>, Error> {
    let sparse = match (body.sparse_indices, body.sparse_values) {
        (Some(indices), Some(values)) => Some((indices, values)),
        (None, None) => None,
        _ => {
            return Err(Error::BadRequest(
                "sparse_indices and sparse_values must be provided together".to_owned(),
            ));
        }
    };
    let matches = state
        .hybrid_search(
            &principal,
            name,
            body.vector,
            sparse,
            body.query_text,
            body.k,
            body.filter,
            body.ef_search,
            body.rrf_k0,
            body.with_payload,
            body.with_vector,
        )
        .await?;
    Ok(Json(SearchResponse {
        matches: matches.into_iter().map(MatchDto::from).collect(),
    }))
}

#[derive(Deserialize)]
struct SearchTextBody {
    /// The query text — embedded server-side with the collection's provider and
    /// also scored by BM25 over the inverted index (ADR-0046/0047).
    text: String,
    #[serde(default = "default_k")]
    k: usize,
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default = "default_ef")]
    ef_search: usize,
    #[serde(default = "default_rrf_k0")]
    rrf_k0: f32,
    #[serde(default = "default_true")]
    with_payload: bool,
    #[serde(default)]
    with_vector: bool,
    /// Opt-in: rerank the candidate pool with the collection's `[rerank.<name>]`
    /// provider and return the top-`k` reordered (ADR-0047).
    #[serde(default)]
    rerank: bool,
}

/// Text-in search: embed the query, run dense (⊕ BM25) retrieval, optionally
/// rerank — all in one call (ADR-0047). Requires an `[embedding.<collection>]`.
async fn search_text(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<SearchTextBody>,
) -> Result<Json<SearchResponse>, Error> {
    let matches = state
        .search_text(
            &principal,
            name,
            body.text,
            body.k,
            body.filter,
            body.ef_search,
            body.rrf_k0,
            body.with_payload,
            body.with_vector,
            body.rerank,
        )
        .await?;
    Ok(Json(SearchResponse {
        matches: matches.into_iter().map(MatchDto::from).collect(),
    }))
}

#[derive(Deserialize)]
struct FetchBody {
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default = "default_fetch_limit")]
    limit: usize,
    #[serde(default = "default_true")]
    with_payload: bool,
    #[serde(default)]
    with_vector: bool,
}

fn default_fetch_limit() -> usize {
    100
}

#[derive(Serialize)]
struct FetchedPoint {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
}

impl From<MatchOut> for FetchedPoint {
    fn from(m: MatchOut) -> Self {
        Self {
            id: m.id,
            payload: m.payload,
            vector: m.vector,
        }
    }
}

#[derive(Serialize)]
struct FetchResponse {
    points: Vec<FetchedPoint>,
}

async fn fetch(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<FetchBody>,
) -> Result<Json<FetchResponse>, Error> {
    let points = state
        .fetch(
            &principal,
            name,
            body.filter,
            body.limit,
            body.with_payload,
            body.with_vector,
        )
        .await?;
    Ok(Json(FetchResponse {
        points: points.into_iter().map(FetchedPoint::from).collect(),
    }))
}

// ---- Multi-vector (late-interaction / ColBERT) documents (ADR-0028) ----

#[derive(Deserialize)]
struct DocumentDto {
    id: String,
    vectors: Vec<Vec<f32>>,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct UpsertDocumentsBody {
    documents: Vec<DocumentDto>,
}

async fn upsert_documents(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<UpsertDocumentsBody>,
) -> Result<Json<UpsertResponse>, Error> {
    let documents = body
        .documents
        .into_iter()
        .map(|d| DocumentIn {
            id: d.id,
            vectors: d.vectors,
            payload: d.payload,
        })
        .collect();
    let upserted = state.upsert_documents(&principal, name, documents).await?;
    Ok(Json(UpsertResponse { upserted }))
}

async fn delete_documents(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<DeletePointsBody>,
) -> Result<Json<DeletePointsResponse>, Error> {
    let deleted = state.delete_documents(&principal, name, body.ids).await?;
    Ok(Json(DeletePointsResponse { deleted }))
}

#[derive(Deserialize)]
struct SearchMultiVectorBody {
    query: Vec<Vec<f32>>,
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
struct DocumentMatchDto {
    id: String,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vectors: Option<Vec<Vec<f32>>>,
}

impl From<DocumentMatchOut> for DocumentMatchDto {
    fn from(m: DocumentMatchOut) -> Self {
        Self {
            id: m.id,
            score: m.score,
            payload: m.payload,
            vectors: m.vectors,
        }
    }
}

#[derive(Serialize)]
struct SearchMultiVectorResponse {
    matches: Vec<DocumentMatchDto>,
}

async fn search_multi_vector(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<SearchMultiVectorBody>,
) -> Result<Json<SearchMultiVectorResponse>, Error> {
    let matches = state
        .search_multi_vector(
            &principal,
            name,
            body.query,
            body.k,
            body.filter,
            body.ef_search,
            body.with_payload,
            body.with_vector,
        )
        .await?;
    Ok(Json(SearchMultiVectorResponse {
        matches: matches.into_iter().map(DocumentMatchDto::from).collect(),
    }))
}
