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
    /// Invalid or insecure configuration.
    #[error("configuration error: {0}")]
    Config(String),
    /// A network or filesystem I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// An unexpected internal failure (lock poisoned, task panicked, …).
    #[error("internal error: {0}")]
    Internal(String),
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
        if status.is_server_error() {
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
