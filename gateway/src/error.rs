//! Gateway error type and its mapping to HTTP responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// NIP-98 authentication failed (missing/invalid/expired auth event).
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The caller is authenticated but not the owner of the target asset/mint.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The request was malformed (bad query, bad JSON, bad address, …).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// A downstream dependency (tapd, registry) failed.
    #[error("upstream error: {0}")]
    Upstream(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = match &self {
            GatewayError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            GatewayError::Forbidden(_) => StatusCode::FORBIDDEN,
            GatewayError::BadRequest(_) => StatusCode::BAD_REQUEST,
            GatewayError::Upstream(_) => StatusCode::BAD_GATEWAY,
        };
        // Don't leak internal detail beyond the category message we constructed.
        let body = ErrorBody {
            error: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

pub type GatewayResult<T> = Result<T, GatewayError>;

impl From<crate::auth::AuthError> for GatewayError {
    fn from(e: crate::auth::AuthError) -> Self {
        GatewayError::Unauthorized(e.to_string())
    }
}

impl From<crate::registry::RegistryError> for GatewayError {
    fn from(e: crate::registry::RegistryError) -> Self {
        GatewayError::Upstream(e.to_string())
    }
}
