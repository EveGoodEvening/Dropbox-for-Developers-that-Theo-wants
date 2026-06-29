//! Structured error model with stable codes.
//!
//! Errors are machine-readable: every error carries a stable [`Fs2ErrorCode`]
//! so clients and tests can branch on the code rather than parsing messages.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable, machine-readable error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fs2ErrorCode {
    /// Authentication failed or token missing.
    Unauthorized,
    /// The device has been revoked.
    DeviceRevoked,
    /// Workspace not found.
    WorkspaceNotFound,
    /// Node not found.
    NodeNotFound,
    /// Path collision with an existing live sibling.
    PathCollision,
    /// File revision conflict (stale base revision).
    RevisionConflict,
    /// Referenced blob is missing from the store.
    BlobMissing,
    /// Operation shape or state is invalid.
    InvalidOperation,
    /// Workspace or blob quota exceeded.
    QuotaExceeded,
    /// Client is being rate limited.
    RateLimited,
    /// Client is offline and cannot reach the backend.
    Offline,
    /// File bytes are not hydrated and cannot be fetched.
    NotHydrated,
    /// Secret value is unavailable (key missing or decryption failed).
    SecretUnavailable,
}

impl Fs2ErrorCode {
    /// Suggested HTTP status for this code.
    #[must_use]
    pub fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::DeviceRevoked | Self::SecretUnavailable => 403,
            Self::WorkspaceNotFound | Self::NodeNotFound | Self::BlobMissing => 404,
            Self::PathCollision | Self::RevisionConflict | Self::InvalidOperation => 409,
            Self::QuotaExceeded | Self::RateLimited => 429,
            Self::Offline | Self::NotHydrated => 503,
        }
    }

    /// Stable string code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::DeviceRevoked => "device_revoked",
            Self::WorkspaceNotFound => "workspace_not_found",
            Self::NodeNotFound => "node_not_found",
            Self::PathCollision => "path_collision",
            Self::RevisionConflict => "revision_conflict",
            Self::BlobMissing => "blob_missing",
            Self::InvalidOperation => "invalid_operation",
            Self::QuotaExceeded => "quota_exceeded",
            Self::RateLimited => "rate_limited",
            Self::Offline => "offline",
            Self::NotHydrated => "not_hydrated",
            Self::SecretUnavailable => "secret_unavailable",
        }
    }
}

impl fmt::Display for Fs2ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured error returned by fs2 operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fs2Error {
    /// Stable code.
    pub code: Fs2ErrorCode,
    /// Human-readable message (never contains secrets).
    pub message: String,
    /// Optional structured details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl Fs2Error {
    /// Create a new error with a code and message.
    #[must_use]
    pub fn new(code: Fs2ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attach structured details.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Stable code string.
    #[must_use]
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
}

impl fmt::Display for Fs2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Fs2Error {}

impl From<Fs2Error> for Fs2ErrorCode {
    fn from(e: Fs2Error) -> Self {
        e.code
    }
}

/// Convenience alias.
pub type Fs2Result<T> = Result<T, Fs2Error>;

/// Wire representation of an error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fs2ErrorResponse {
    /// The error payload under the `error` key.
    pub error: Fs2Error,
}

impl From<Fs2Error> for Fs2ErrorResponse {
    fn from(e: Fs2Error) -> Self {
        Self { error: e }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_http_status() {
        assert_eq!(Fs2ErrorCode::Unauthorized.http_status(), 401);
        assert_eq!(Fs2ErrorCode::NodeNotFound.http_status(), 404);
        assert_eq!(Fs2ErrorCode::RevisionConflict.http_status(), 409);
        assert_eq!(Fs2ErrorCode::Offline.http_status(), 503);
    }

    #[test]
    fn error_json_roundtrip() {
        let e = Fs2Error::new(Fs2ErrorCode::RevisionConflict, "stale base revision").with_details(
            serde_json::json!({
                "node_id": "abc",
                "client_base_revision": "r1",
                "server_current_revision": "r2",
            }),
        );
        let body = Fs2ErrorResponse::from(e.clone());
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"code\":\"revision_conflict\""));
        assert!(json.contains("\"error\""));
        let back: Fs2ErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.error, e);
    }

    #[test]
    fn error_display() {
        let e = Fs2Error::new(Fs2ErrorCode::PathCollision, "x");
        assert_eq!(format!("{e}"), "path_collision: x");
    }
}
