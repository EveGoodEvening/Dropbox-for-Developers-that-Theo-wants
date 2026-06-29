//! Backend error model, mapping to fs2-core's structured errors.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use fs2_core::{Fs2Error, Fs2ErrorCode, Fs2ErrorResponse};

/// Backend-specific error type.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// A domain-level fs2 error.
    #[error("{0}")]
    Domain(#[from] Fs2Error),
    /// An internal server error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl BackendError {
    /// Create an internal error.
    #[must_use]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

/// Convenience alias.
pub type BackendResult<T> = Result<T, BackendError>;

impl IntoResponse for BackendError {
    fn into_response(self) -> Response {
        match self {
            Self::Domain(e) => {
                let status = StatusCode::from_u16(e.code.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let body = Fs2ErrorResponse::from(e);
                (status, Json(body)).into_response()
            }
            Self::Internal(msg) => {
                let err = Fs2Error::new(Fs2ErrorCode::InvalidOperation, msg);
                let body = Fs2ErrorResponse::from(err);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn domain_error_maps_to_response() {
        let err = BackendError::Domain(Fs2Error::new(
            Fs2ErrorCode::WorkspaceNotFound,
            "no such workspace",
        ));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn internal_error_maps_to_500() {
        let err = BackendError::internal("something broke");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
