// SPDX-License-Identifier: AGPL-3.0-only
//! The gRPC surface (tonic): implements the `quiver.v1` service over the shared
//! engine state. Auth is checked per call from the `authorization` metadata.

use serde_json::Value;
use tonic::{Request, Response, Status};

use quiver_embed::{DistanceMetric, IndexKind, IndexSpec};
use quiver_proto::v1::{
    self,
    quiver_server::{Quiver, QuiverServer},
};
use quiver_query::Filter;

use crate::{AppState, CollectionInfo, MatchOut, PointIn, PointOut};

/// Build the gRPC service over the shared state.
pub(crate) fn service(state: AppState) -> QuiverServer<QuiverService> {
    QuiverServer::new(QuiverService { state })
}

pub(crate) struct QuiverService {
    state: AppState,
}

impl QuiverService {
    fn check_auth<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .unwrap_or(value)
            });
        if self.state.authorized(presented) {
            Ok(())
        } else {
            Err(Status::unauthenticated("missing or invalid API key"))
        }
    }
}

fn metric_from_proto(value: i32) -> DistanceMetric {
    match v1::Metric::try_from(value) {
        Ok(v1::Metric::Cosine) => DistanceMetric::Cosine,
        Ok(v1::Metric::Dot) => DistanceMetric::Dot,
        _ => DistanceMetric::L2,
    }
}

fn metric_to_proto(metric: DistanceMetric) -> i32 {
    let value = match metric {
        DistanceMetric::L2 => v1::Metric::L2,
        DistanceMetric::Cosine => v1::Metric::Cosine,
        DistanceMetric::Dot => v1::Metric::Dot,
    };
    value as i32
}

fn index_spec_from_proto(kind: i32, pq_subspaces: Option<u32>) -> IndexSpec {
    let kind = match v1::IndexKind::try_from(kind) {
        Ok(v1::IndexKind::Vamana) => IndexKind::Vamana,
        Ok(v1::IndexKind::DiskVamana) => IndexKind::DiskVamana,
        Ok(v1::IndexKind::Ivf) => IndexKind::Ivf,
        _ => IndexKind::Hnsw,
    };
    IndexSpec { kind, pq_subspaces }
}

fn index_kind_to_proto(kind: IndexKind) -> i32 {
    let value = match kind {
        IndexKind::Vamana => v1::IndexKind::Vamana,
        IndexKind::DiskVamana => v1::IndexKind::DiskVamana,
        IndexKind::Ivf => v1::IndexKind::Ivf,
        _ => v1::IndexKind::Hnsw,
    };
    value as i32
}

fn collection_to_proto(info: CollectionInfo) -> v1::Collection {
    v1::Collection {
        name: info.name,
        dim: info.dim,
        metric: metric_to_proto(info.metric),
        count: info.count,
        index: index_kind_to_proto(info.index.kind),
        pq_subspaces: info.index.pq_subspaces,
    }
}

fn payload_to_bytes(payload: &Value) -> Vec<u8> {
    serde_json::to_vec(payload).unwrap_or_default()
}

fn point_to_proto(point: PointOut) -> v1::Point {
    v1::Point {
        id: point.id,
        vector: point.vector.unwrap_or_default(),
        payload: payload_to_bytes(&point.payload),
    }
}

fn match_to_proto(m: MatchOut) -> v1::Match {
    v1::Match {
        id: m.id,
        score: m.score,
        payload: m.payload.as_ref().map(payload_to_bytes).unwrap_or_default(),
        vector: m.vector.unwrap_or_default(),
    }
}

fn parse_payload(bytes: &[u8]) -> Result<Value, Status> {
    if bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(bytes)
            .map_err(|e| Status::invalid_argument(format!("invalid payload json: {e}")))
    }
}

#[tonic::async_trait]
impl Quiver for QuiverService {
    async fn create_collection(
        &self,
        request: Request<v1::CreateCollectionRequest>,
    ) -> Result<Response<v1::Collection>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let index = index_spec_from_proto(req.index, req.pq_subspaces);
        let info = self
            .state
            .create_collection(req.name, req.dim, metric_from_proto(req.metric), index)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(collection_to_proto(info)))
    }

    async fn get_collection(
        &self,
        request: Request<v1::GetCollectionRequest>,
    ) -> Result<Response<v1::Collection>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let info = self
            .state
            .get_collection(req.name)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(collection_to_proto(info)))
    }

    async fn list_collections(
        &self,
        request: Request<v1::ListCollectionsRequest>,
    ) -> Result<Response<v1::ListCollectionsResponse>, Status> {
        self.check_auth(&request)?;
        let infos = self
            .state
            .list_collections()
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::ListCollectionsResponse {
            collections: infos.into_iter().map(collection_to_proto).collect(),
        }))
    }

    async fn delete_collection(
        &self,
        request: Request<v1::DeleteCollectionRequest>,
    ) -> Result<Response<v1::DeleteCollectionResponse>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let existed = self
            .state
            .delete_collection(req.name)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::DeleteCollectionResponse { existed }))
    }

    async fn upsert(
        &self,
        request: Request<v1::UpsertRequest>,
    ) -> Result<Response<v1::UpsertResponse>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let mut points = Vec::with_capacity(req.points.len());
        for point in req.points {
            points.push(PointIn {
                id: point.id,
                vector: point.vector,
                payload: parse_payload(&point.payload)?,
            });
        }
        let upserted = self
            .state
            .upsert(req.collection, points)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::UpsertResponse { upserted }))
    }

    async fn delete_points(
        &self,
        request: Request<v1::DeletePointsRequest>,
    ) -> Result<Response<v1::DeletePointsResponse>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let deleted = self
            .state
            .delete_points(req.collection, req.ids)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::DeletePointsResponse { deleted }))
    }

    async fn get_points(
        &self,
        request: Request<v1::GetPointsRequest>,
    ) -> Result<Response<v1::GetPointsResponse>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let points = self
            .state
            .get_points(req.collection, req.ids, req.with_vector)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::GetPointsResponse {
            points: points.into_iter().map(point_to_proto).collect(),
        }))
    }

    async fn search(
        &self,
        request: Request<v1::SearchRequest>,
    ) -> Result<Response<v1::SearchResponse>, Status> {
        self.check_auth(&request)?;
        let req = request.into_inner();
        let filter: Option<Filter> = if req.filter.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice(&req.filter)
                    .map_err(|e| Status::invalid_argument(format!("invalid filter json: {e}")))?,
            )
        };
        let k = if req.k == 0 { 10 } else { req.k as usize };
        let ef_search = if req.ef_search == 0 {
            64
        } else {
            req.ef_search as usize
        };
        let matches = self
            .state
            .search(
                req.collection,
                req.vector,
                k,
                filter,
                ef_search,
                req.with_payload,
                req.with_vector,
            )
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::SearchResponse {
            matches: matches.into_iter().map(match_to_proto).collect(),
        }))
    }
}
