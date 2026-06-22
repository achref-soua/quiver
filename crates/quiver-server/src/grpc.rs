// SPDX-License-Identifier: AGPL-3.0-only
//! The gRPC surface (tonic): implements the `quiver.v1` service over the shared
//! engine state. Auth is checked per call from the `authorization` metadata.

use serde_json::Value;
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use quiver_embed::{
    DEFAULT_RRF_K0, DistanceMetric, FieldType, FilterableField, IndexKind, IndexSpec,
    VectorEncryption, WalOp,
};
use quiver_proto::v1::{
    self,
    quiver_server::{Quiver, QuiverServer},
};
use quiver_query::Filter;

use crate::auth::Principal;
use crate::{AppState, CollectionInfo, DocumentIn, DocumentMatchOut, MatchOut, PointIn, PointOut};

/// Build the gRPC service over the shared state.
pub(crate) fn service(state: AppState) -> QuiverServer<QuiverService> {
    QuiverServer::new(QuiverService { state })
}

pub(crate) struct QuiverService {
    state: AppState,
}

impl QuiverService {
    fn authenticate<T>(&self, request: &Request<T>) -> Result<Principal, Status> {
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
        self.state
            .authenticate(presented)
            .ok_or_else(|| Status::unauthenticated("missing or invalid API key"))
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
        Ok(v1::IndexKind::Colbert) => IndexKind::Colbert,
        _ => IndexKind::Hnsw,
    };
    IndexSpec { kind, pq_subspaces }
}

fn index_kind_to_proto(kind: IndexKind) -> i32 {
    let value = match kind {
        IndexKind::Vamana => v1::IndexKind::Vamana,
        IndexKind::DiskVamana => v1::IndexKind::DiskVamana,
        IndexKind::Ivf => v1::IndexKind::Ivf,
        IndexKind::Colbert => v1::IndexKind::Colbert,
        _ => v1::IndexKind::Hnsw,
    };
    value as i32
}

fn field_type_from_proto(value: i32) -> FieldType {
    match v1::FieldType::try_from(value) {
        Ok(v1::FieldType::Numeric) => FieldType::Numeric,
        // UNSPECIFIED and KEYWORD both map to keyword.
        _ => FieldType::Keyword,
    }
}

fn vector_encryption_from_proto(value: i32) -> VectorEncryption {
    match v1::VectorEncryption::try_from(value) {
        Ok(v1::VectorEncryption::Dcpe) => VectorEncryption::Dcpe,
        Ok(v1::VectorEncryption::ClientSide) => VectorEncryption::ClientSide,
        // NONE and any unknown value map to plaintext.
        _ => VectorEncryption::None,
    }
}

fn vector_encryption_to_proto(encryption: VectorEncryption) -> i32 {
    let value = match encryption {
        VectorEncryption::None => v1::VectorEncryption::None,
        VectorEncryption::Dcpe => v1::VectorEncryption::Dcpe,
        VectorEncryption::ClientSide => v1::VectorEncryption::ClientSide,
    };
    value as i32
}

fn field_type_to_proto(field_type: FieldType) -> i32 {
    let value = match field_type {
        FieldType::Numeric => v1::FieldType::Numeric,
        _ => v1::FieldType::Keyword,
    };
    value as i32
}

fn filterable_from_proto(fields: Vec<v1::FilterableField>) -> Vec<FilterableField> {
    fields
        .into_iter()
        .map(|f| FilterableField {
            path: f.path,
            field_type: field_type_from_proto(f.field_type),
        })
        .collect()
}

fn filterable_to_proto(fields: Vec<FilterableField>) -> Vec<v1::FilterableField> {
    fields
        .into_iter()
        .map(|f| v1::FilterableField {
            path: f.path,
            field_type: field_type_to_proto(f.field_type),
        })
        .collect()
}

fn collection_to_proto(info: CollectionInfo) -> v1::Collection {
    v1::Collection {
        name: info.name,
        dim: info.dim,
        metric: metric_to_proto(info.metric),
        count: info.count,
        index: index_kind_to_proto(info.index.kind),
        pq_subspaces: info.index.pq_subspaces,
        filterable: filterable_to_proto(info.filterable),
        multivector: info.multivector,
        vector_encryption: vector_encryption_to_proto(info.vector_encryption),
    }
}

fn vectors_from_proto(vectors: Vec<v1::Vector>) -> Vec<Vec<f32>> {
    vectors.into_iter().map(|v| v.values).collect()
}

fn vectors_to_proto(vectors: Vec<Vec<f32>>) -> Vec<v1::Vector> {
    vectors
        .into_iter()
        .map(|values| v1::Vector { values })
        .collect()
}

fn document_match_to_proto(m: DocumentMatchOut) -> v1::DocumentMatch {
    v1::DocumentMatch {
        id: m.id,
        score: m.score,
        payload: m.payload.as_ref().map(payload_to_bytes).unwrap_or_default(),
        vectors: m.vectors.map(vectors_to_proto).unwrap_or_default(),
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

// Convert a committed op to its replication proto form (ADR-0030). Checkpoint ops
// are a per-node concern and are never streamed (returns `None`).
fn repl_op_to_proto(lsn: u64, op: &WalOp) -> Option<v1::ReplicationOp> {
    use v1::replication_op::Op;
    let inner = match op {
        WalOp::CreateCollection {
            collection_id,
            name,
            descriptor,
        } => Op::CreateCollection(v1::ReplCreateCollection {
            collection_id: collection_id.0,
            name: name.clone(),
            descriptor: descriptor.clone(),
        }),
        WalOp::DropCollection { collection_id } => Op::DropCollection(v1::ReplDropCollection {
            collection_id: collection_id.0,
        }),
        WalOp::Upsert {
            collection_id,
            external_id,
            vector,
            payload,
        } => Op::Upsert(v1::ReplUpsert {
            collection_id: collection_id.0,
            external_id: external_id.clone(),
            vector: vector.clone(),
            payload: payload.clone(),
        }),
        WalOp::Delete {
            collection_id,
            external_id,
        } => Op::Delete(v1::ReplDelete {
            collection_id: collection_id.0,
            external_id: external_id.clone(),
        }),
        WalOp::Checkpoint { .. } => return None,
    };
    Some(v1::ReplicationOp {
        lsn,
        op: Some(inner),
    })
}

#[tonic::async_trait]
impl Quiver for QuiverService {
    type ReplicateStream = ReceiverStream<Result<v1::ReplicationOp, Status>>;

    async fn replicate(
        &self,
        request: Request<v1::ReplicateRequest>,
    ) -> Result<Response<Self::ReplicateStream>, Status> {
        let principal = self.authenticate(&request)?;
        let (snapshot, mut rx) = self
            .state
            .open_replication(&principal)
            .await
            .map_err(|e| e.to_status())?;
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(256);
        tokio::spawn(async move {
            // Bootstrap with the logical snapshot (pre-tail state, no leader LSN).
            for op in snapshot {
                if let Some(proto) = repl_op_to_proto(0, &op)
                    && out_tx.send(Ok(proto)).await.is_err()
                {
                    return;
                }
            }
            // Then the live commit tail, in order.
            loop {
                match rx.recv().await {
                    Ok(entry) => {
                        if let Some(proto) = repl_op_to_proto(entry.lsn.value(), &entry.op)
                            && out_tx.send(Ok(proto)).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let _ = out_tx
                            .send(Err(Status::data_loss(format!(
                                "replication fell {n} ops behind; reconnect to re-bootstrap"
                            ))))
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(out_rx)))
    }

    async fn create_collection(
        &self,
        request: Request<v1::CreateCollectionRequest>,
    ) -> Result<Response<v1::Collection>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let index = index_spec_from_proto(req.index, req.pq_subspaces);
        let filterable = filterable_from_proto(req.filterable);
        let info = self
            .state
            .create_collection(
                &principal,
                req.name,
                req.dim,
                metric_from_proto(req.metric),
                index,
                filterable,
                req.multivector,
                vector_encryption_from_proto(req.vector_encryption),
            )
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(collection_to_proto(info)))
    }

    async fn get_collection(
        &self,
        request: Request<v1::GetCollectionRequest>,
    ) -> Result<Response<v1::Collection>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let info = self
            .state
            .get_collection(&principal, req.name)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(collection_to_proto(info)))
    }

    async fn list_collections(
        &self,
        request: Request<v1::ListCollectionsRequest>,
    ) -> Result<Response<v1::ListCollectionsResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let infos = self
            .state
            .list_collections(&principal)
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
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let existed = self
            .state
            .delete_collection(&principal, req.name)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::DeleteCollectionResponse { existed }))
    }

    async fn upsert(
        &self,
        request: Request<v1::UpsertRequest>,
    ) -> Result<Response<v1::UpsertResponse>, Status> {
        let principal = self.authenticate(&request)?;
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
            .upsert(&principal, req.collection, points)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::UpsertResponse { upserted }))
    }

    async fn delete_points(
        &self,
        request: Request<v1::DeletePointsRequest>,
    ) -> Result<Response<v1::DeletePointsResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let deleted = self
            .state
            .delete_points(&principal, req.collection, req.ids)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::DeletePointsResponse { deleted }))
    }

    async fn get_points(
        &self,
        request: Request<v1::GetPointsRequest>,
    ) -> Result<Response<v1::GetPointsResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let points = self
            .state
            .get_points(&principal, req.collection, req.ids, req.with_vector)
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
        let principal = self.authenticate(&request)?;
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
                &principal,
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

    async fn hybrid_search(
        &self,
        request: Request<v1::HybridSearchRequest>,
    ) -> Result<Response<v1::SearchResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let filter: Option<Filter> = if req.filter.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice(&req.filter)
                    .map_err(|e| Status::invalid_argument(format!("invalid filter json: {e}")))?,
            )
        };
        let dense = (!req.vector.is_empty()).then_some(req.vector);
        let sparse = req.sparse.map(|s| (s.indices, s.values));
        let k = if req.k == 0 { 10 } else { req.k as usize };
        let ef_search = if req.ef_search == 0 {
            64
        } else {
            req.ef_search as usize
        };
        let rrf_k0 = if req.rrf_k0 == 0.0 {
            DEFAULT_RRF_K0
        } else {
            req.rrf_k0
        };
        let matches = self
            .state
            .hybrid_search(
                &principal,
                req.collection,
                dense,
                sparse,
                // `query_text` (full-text BM25) parity comes in the ADR-0046 follow-up.
                None,
                k,
                filter,
                ef_search,
                rrf_k0,
                req.with_payload,
                req.with_vector,
            )
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::SearchResponse {
            matches: matches.into_iter().map(match_to_proto).collect(),
        }))
    }

    async fn fetch(
        &self,
        request: Request<v1::FetchRequest>,
    ) -> Result<Response<v1::FetchResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let filter: Option<Filter> = if req.filter.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice(&req.filter)
                    .map_err(|e| Status::invalid_argument(format!("invalid filter json: {e}")))?,
            )
        };
        let limit = if req.limit == 0 {
            100
        } else {
            req.limit as usize
        };
        let matches = self
            .state
            .fetch(
                &principal,
                req.collection,
                filter,
                limit,
                req.with_payload,
                req.with_vector,
            )
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::FetchResponse {
            matches: matches.into_iter().map(match_to_proto).collect(),
        }))
    }

    async fn upsert_multi_vector(
        &self,
        request: Request<v1::UpsertMultiVectorRequest>,
    ) -> Result<Response<v1::UpsertMultiVectorResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let mut documents = Vec::with_capacity(req.documents.len());
        for doc in req.documents {
            documents.push(DocumentIn {
                id: doc.id,
                vectors: vectors_from_proto(doc.vectors),
                payload: parse_payload(&doc.payload)?,
            });
        }
        let upserted = self
            .state
            .upsert_documents(&principal, req.collection, documents)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::UpsertMultiVectorResponse { upserted }))
    }

    async fn search_multi_vector(
        &self,
        request: Request<v1::SearchMultiVectorRequest>,
    ) -> Result<Response<v1::SearchMultiVectorResponse>, Status> {
        let principal = self.authenticate(&request)?;
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
        let query = vectors_from_proto(req.query);
        let matches = self
            .state
            .search_multi_vector(
                &principal,
                req.collection,
                query,
                k,
                filter,
                ef_search,
                req.with_payload,
                req.with_vector,
            )
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::SearchMultiVectorResponse {
            matches: matches.into_iter().map(document_match_to_proto).collect(),
        }))
    }

    async fn delete_documents(
        &self,
        request: Request<v1::DeleteDocumentsRequest>,
    ) -> Result<Response<v1::DeleteDocumentsResponse>, Status> {
        let principal = self.authenticate(&request)?;
        let req = request.into_inner();
        let deleted = self
            .state
            .delete_documents(&principal, req.collection, req.ids)
            .await
            .map_err(|e| e.to_status())?;
        Ok(Response::new(v1::DeleteDocumentsResponse { deleted }))
    }
}
