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
        // The client only ever sees the category message; log the full detail
        // here (operator-side) so upstream failures — tapd/litd errors, gRPC
        // decode issues — are diagnosable. Upstream (502) is the interesting one.
        let message = self.to_string();
        if status == StatusCode::BAD_GATEWAY {
            log::error!("gateway {} {}", status.as_u16(), message);
        } else {
            log::warn!("gateway {} {}", status.as_u16(), message);
        }
        let body = ErrorBody { error: message };
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
        use crate::registry::RegistryError as R;
        let msg = e.to_string();
        match e {
            // The caller lacks the balance to act — a client-side condition.
            R::InsufficientBalance { .. } => GatewayError::Forbidden(msg),
            R::BatchClaimed(_) => GatewayError::BadRequest(msg),
            R::Db(_) => GatewayError::Upstream(msg),
        }
    }
}
