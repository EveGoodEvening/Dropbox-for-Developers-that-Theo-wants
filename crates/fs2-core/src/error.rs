//! Structured error model shared by backend and client.
//!
//! Errors carry a stable [`ErrorCode`] so clients can branch on machine-readable
//! codes instead of parsing messages. The wire shape is:
//!
//! ```json
//! { "error": { "code": "revision_conflict", "message": "...", "details": {...} } }
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable, machine-readable error codes.
///
/// These are the canonical strings that appear in the `error.code` field on the
/// wire and in CLI exit-code mapping. Never rename a variant's serde tag
/// without a wire-version migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Missing or invalid auth token.
    Unauthorized,
    /// Device has been revoked and may no longer mutate.
    DeviceRevoked,
    /// Workspace does not exist or device does not have access.
    WorkspaceNotFound,
    /// Node does not exist or is tombstoned.
    NodeNotFound,
    /// Two live siblings would collide on the target filesystem.
    PathCollision,
    /// `PutFileRevision` base revision does not match current revision.
    RevisionConflict,
    /// Referenced blob is missing from the object store.
    BlobMissing,
    /// Operation is structurally invalid or violates a stateful invariant.
    InvalidOperation,
    /// Workspace or blob quota exceeded.
    QuotaExceeded,
    /// Client is being rate limited.
    RateLimited,
    /// Backend is unreachable or operation cannot complete offline.
    Offline,
    /// File bytes are not hydrated locally and cannot be fetched now.
    NotHydrated,
    /// Secret value or workspace key is unavailable.
    SecretUnavailable,
}

impl ErrorCode {
    /// Stable wire string for this code.
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

    /// Suggested HTTP status for this code.
    ///
    /// Used by the backend when converting an `Fs2Error` to an HTTP response.
    #[must_use]
    pub fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::DeviceRevoked => 403,
            Self::WorkspaceNotFound | Self::NodeNotFound | Self::BlobMissing => 404,
            Self::PathCollision
            | Self::RevisionConflict
            | Self::InvalidOperation
            | Self::NotHydrated
            | Self::SecretUnavailable => 409,
            Self::QuotaExceeded | Self::RateLimited => 429,
            Self::Offline => 503,
        }
    }

    /// Inverse of [`as_str`].
    #[must_use]
    pub fn parse_tag(s: &str) -> Option<Self> {
        Some(match s {
            "unauthorized" => Self::Unauthorized,
            "device_revoked" => Self::DeviceRevoked,
            "workspace_not_found" => Self::WorkspaceNotFound,
            "node_not_found" => Self::NodeNotFound,
            "path_collision" => Self::PathCollision,
            "revision_conflict" => Self::RevisionConflict,
            "blob_missing" => Self::BlobMissing,
            "invalid_operation" => Self::InvalidOperation,
            "quota_exceeded" => Self::QuotaExceeded,
            "rate_limited" => Self::RateLimited,
            "offline" => Self::Offline,
            "not_hydrated" => Self::NotHydrated,
            "secret_unavailable" => Self::SecretUnavailable,
            _ => return None,
        })
    }
}

/// Structured error payload that travels over the wire and is also useful
/// locally for typed error handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    /// Stable error code.
    pub code: ErrorCode,
    /// Human-readable explanation. Never includes secrets or tokens.
    pub message: String,
    /// Optional structured details (node ids, cursors, revisions, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ErrorPayload {
    /// Build a payload with no details.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
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
}

/// Top-level FS2 error type used by library code.
///
/// The [`thiserror`] derivation gives `Display`/`Error` for free; the
/// [`Fs2Error::payload`] method produces the wire shape.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Fs2Error {
    /// `Unauthorized`
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// `DeviceRevoked`
    #[error("device revoked: {0}")]
    DeviceRevoked(String),
    /// `WorkspaceNotFound`
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),
    /// `NodeNotFound`
    #[error("node not found: {0}")]
    NodeNotFound(String),
    /// `PathCollision`
    #[error("path collision: {0}")]
    PathCollision(String),
    /// `RevisionConflict`
    #[error("revision conflict: {0}")]
    RevisionConflict(String),
    /// `BlobMissing`
    #[error("blob missing: {0}")]
    BlobMissing(String),
    /// `InvalidOperation`
    #[error("invalid operation: {0}")]
    InvalidOperation(String),
    /// `QuotaExceeded`
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    /// `RateLimited`
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// `Offline`
    #[error("offline: {0}")]
    Offline(String),
    /// `NotHydrated`
    #[error("not hydrated: {0}")]
    NotHydrated(String),
    /// `SecretUnavailable`
    #[error("secret unavailable: {0}")]
    SecretUnavailable(String),
}

impl Fs2Error {
    /// Stable code for this error.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::DeviceRevoked(_) => ErrorCode::DeviceRevoked,
            Self::WorkspaceNotFound(_) => ErrorCode::WorkspaceNotFound,
            Self::NodeNotFound(_) => ErrorCode::NodeNotFound,
            Self::PathCollision(_) => ErrorCode::PathCollision,
            Self::RevisionConflict(_) => ErrorCode::RevisionConflict,
            Self::BlobMissing(_) => ErrorCode::BlobMissing,
            Self::InvalidOperation(_) => ErrorCode::InvalidOperation,
            Self::QuotaExceeded(_) => ErrorCode::QuotaExceeded,
            Self::RateLimited(_) => ErrorCode::RateLimited,
            Self::Offline(_) => ErrorCode::Offline,
            Self::NotHydrated(_) => ErrorCode::NotHydrated,
            Self::SecretUnavailable(_) => ErrorCode::SecretUnavailable,
        }
    }

    /// Wire payload for this error.
    #[must_use]
    pub fn payload(&self) -> ErrorPayload {
        ErrorPayload::new(self.code(), self.to_string())
    }

    /// Suggested HTTP status code for this error.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        self.code().http_status()
    }

    /// CLI-friendly single-line message.
    ///
    /// Same as `Display` today, but kept as a separate method so future
    /// redaction or localization can happen here without touching `Display`.
    #[must_use]
    pub fn cli_message(&self) -> String {
        self.to_string()
    }
}

impl From<Fs2Error> for ErrorPayload {
    fn from(e: Fs2Error) -> Self {
        e.payload()
    }
}

/// Wire-level error envelope: `{ "error": { ... } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// The error payload.
    pub error: ErrorPayload,
}

impl ErrorEnvelope {
    /// Build an envelope from an [`Fs2Error`].
    #[must_use]
    pub fn from_error(e: &Fs2Error) -> Self {
        Self { error: e.payload() }
    }

    /// Serialize to a JSON string for HTTP response bodies.
    ///
    /// # Errors
    /// Only fails if `serde_json` itself fails, which is unreachable for this
    /// shape.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_json_snapshot;

    #[test]
    fn code_roundtrip() {
        for code in [
            ErrorCode::Unauthorized,
            ErrorCode::DeviceRevoked,
            ErrorCode::WorkspaceNotFound,
            ErrorCode::NodeNotFound,
            ErrorCode::PathCollision,
            ErrorCode::RevisionConflict,
            ErrorCode::BlobMissing,
            ErrorCode::InvalidOperation,
            ErrorCode::QuotaExceeded,
            ErrorCode::RateLimited,
            ErrorCode::Offline,
            ErrorCode::NotHydrated,
            ErrorCode::SecretUnavailable,
        ] {
            assert_eq!(ErrorCode::parse_tag(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::parse_tag("nope"), None);
    }

    #[test]
    fn http_status_mapping() {
        assert_eq!(ErrorCode::Unauthorized.http_status(), 401);
        assert_eq!(ErrorCode::DeviceRevoked.http_status(), 403);
        assert_eq!(ErrorCode::NodeNotFound.http_status(), 404);
        assert_eq!(ErrorCode::RevisionConflict.http_status(), 409);
        assert_eq!(ErrorCode::QuotaExceeded.http_status(), 429);
        assert_eq!(ErrorCode::Offline.http_status(), 503);
    }

    #[test]
    fn payload_and_envelope_shape() {
        let e = Fs2Error::RevisionConflict("base rev stale".to_owned());
        let env = ErrorEnvelope::from_error(&e);
        let json = env.to_json().unwrap();
        assert_json_snapshot!(json);
        assert!(json.contains("\"code\":\"revision_conflict\""));
    }

    #[test]
    fn payload_with_details() {
        let p = ErrorPayload::new(ErrorCode::NodeNotFound, "missing").with_details(
            serde_json::json!({"node_id": "node:00000000-0000-0000-0000-000000000000"}),
        );
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"details\""));
    }

    #[test]
    fn cli_message_matches_display() {
        let e = Fs2Error::Offline("backend unreachable".to_owned());
        assert_eq!(e.cli_message(), e.to_string());
    }
}
