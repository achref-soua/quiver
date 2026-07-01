// SPDX-License-Identifier: AGPL-3.0-only
//! The server error type and its mapping to HTTP (RFC-9457) and gRPC statuses
//! (ADR-0017). Client messages are sanitized — internal details are logged, not
//! returned.

use axum::Json;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use quiver_core::CoreError;
use quiver_embed::Error as EngineError;
use serde_json::json;
use thiserror::Error;

/// An error from the server or the engine beneath it.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from the embeddable engine.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// The authenticated caller's API-key scope does not permit the operation
    /// (RBAC, ADR-0011). The message is generic so it leaks no resource names.
    #[error("{0}")]
    Forbidden(String),
    /// The request exceeds a configured cost limit or is otherwise malformed at
    /// the server edge (ADR-0040). The message names the offending field, its
    /// value, and the cap. Returned as HTTP 400 / gRPC `InvalidArgument`.
    #[error("{0}")]
    BadRequest(String),
    /// Invalid or insecure configuration.
    #[error("configuration error: {0}")]
    Config(String),
    /// A network or filesystem I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// An unexpected internal failure (lock poisoned, task panicked, …).
    #[error("internal error: {0}")]
    Internal(String),
    /// A configured upstream provider (server-side embedding or reranking,
    /// ADR-0047) failed or returned a malformed response. Returned as HTTP 502 /
    /// gRPC `Unavailable`. The message carries no secrets (only env-var *names*
    /// and provider transport/parse detail), so it is shown to the client.
    #[error("{0}")]
    Upstream(String),
    /// This node received a write but is not its shard's Raft leader (ADR-0067).
    /// Carries the current leader's base URL when the group knows it, so a cluster
    /// router (or client) redirects the write to the leader — the same
    /// self-correcting data-path pattern as the cluster's "not my range" redirect.
    /// Returned as HTTP 421 Misdirected Request / gRPC `Unavailable` (retry
    /// elsewhere). The detail carries only a URL, so it is client-safe.
    #[error("not the raft leader; leader: {leader:?}")]
    NotLeader {
        /// The current leader's gRPC base URL, if known.
        leader: Option<String>,
    },
    /// The operation is not supported in the current node topology — e.g. hybrid
    /// / text / multi-vector search, fetch, and metadata listing are not yet
    /// routed by the cluster router (ADR-0065), so they must fail honestly rather
    /// than query the router's own empty local engine and return wrong results.
    /// Returned as HTTP 501 Not Implemented / gRPC `Unimplemented`. The message
    /// carries only the operation name, so it is client-safe.
    #[error("{0}")]
    Unsupported(String),
}

impl Error {
    // Map to an HTTP status and the equivalent gRPC code.
    fn category(&self) -> (StatusCode, tonic::Code) {
        match self {
            Error::Engine(EngineError::CollectionNotFound(_))
            | Error::Engine(EngineError::Core(CoreError::NotFound(_))) => {
                (StatusCode::NOT_FOUND, tonic::Code::NotFound)
            }
            Error::Engine(EngineError::Core(CoreError::AlreadyExists(_))) => {
                (StatusCode::CONFLICT, tonic::Code::AlreadyExists)
            }
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, tonic::Code::PermissionDenied),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, tonic::Code::InvalidArgument),
            Error::Upstream(_) => (StatusCode::BAD_GATEWAY, tonic::Code::Unavailable),
            Error::NotLeader { .. } => (StatusCode::MISDIRECTED_REQUEST, tonic::Code::Unavailable),
            Error::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, tonic::Code::Unimplemented),
            Error::Engine(EngineError::Core(CoreError::InvalidArgument(_)))
            | Error::Engine(EngineError::Index(_))
            | Error::Engine(EngineError::Unsupported(_))
            | Error::Engine(EngineError::Json(_)) => {
                (StatusCode::BAD_REQUEST, tonic::Code::InvalidArgument)
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, tonic::Code::Internal),
        }
    }

    // A client-safe message: the detail for 4xx, a generic line for 5xx.
    fn client_message(&self) -> String {
        let (status, _) = self.category();
        // 5xx detail is sanitized, except an upstream-provider failure whose
        // message is client-safe and actionable (no secrets — names only).
        if status.is_server_error() && !matches!(self, Error::Upstream(_) | Error::Unsupported(_)) {
            "internal error".to_owned()
        } else {
            self.to_string()
        }
    }

    /// Convert to a gRPC [`tonic::Status`], logging server-side faults.
    pub(crate) fn to_status(&self) -> tonic::Status {
        let (status, code) = self.category();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        tonic::Status::new(code, self.client_message())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, _) = self.category();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        let body = json!({
            "type": "about:blank",
            "title": status.canonical_reason().unwrap_or("Error"),
            "status": status.as_u16(),
            "detail": self.client_message(),
        });
        let mut response = (status, Json(body)).into_response();
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}
