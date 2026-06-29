#![allow(
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::struct_field_names
)]

//! Shared FS2 domain model and wire contracts.
mod path;

pub use path::{
    names_collide, try_normalized_name, CasePolicy, NodeName, NormalizedName, ParsePathError,
    WorkspacePath,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{fmt, str::FromStr};
use uuid::Uuid;

/// Returns the crate name for smoke tests and early workspace validation.
pub const fn crate_name() -> &'static str {
    "fs2-core"
}

/// Parse error for FS2 identifier wrappers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFs2IdError {
    message: String,
}

impl ParseFs2IdError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseFs2IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseFs2IdError {}

macro_rules! uuid_id {
    ($name:ident) => {
        #[doc = concat!("Typed UUID wrapper for ", stringify!($name), ".")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates a wrapper from a boundary UUID value.
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the wrapped UUID for storage/transport boundaries.
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }

            /// Generates a random identifier.
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_id!(UserId);
uuid_id!(WorkspaceId);
uuid_id!(DeviceId);
uuid_id!(NodeId);
uuid_id!(RevisionId);
uuid_id!(OpId);
uuid_id!(EnvVarId);

/// Content-addressed blob identifier, for example `sha256:<hex>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct BlobId(String);

impl BlobId {
    /// Creates a blob ID after checking it is non-empty.
    pub fn new(value: impl Into<String>) -> Result<Self, ParseFs2IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ParseFs2IdError::new("blob id cannot be empty"));
        }
        Ok(Self(value))
    }

    /// Returns the canonical blob ID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns the canonical blob ID string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for BlobId {
    type Err = ParseFs2IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for BlobId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Monotonically increasing workspace cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Cursor(i64);

impl Cursor {
    /// Creates a cursor. Negative values are rejected because cursors start at zero.
    pub fn new(value: i64) -> Result<Self, ParseFs2IdError> {
        if value < 0 {
            return Err(ParseFs2IdError::new("cursor cannot be negative"));
        }
        Ok(Self(value))
    }

    /// Returns the raw cursor value.
    pub const fn value(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Cursor {
    type Err = ParseFs2IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = value
            .parse::<i64>()
            .map_err(|error| ParseFs2IdError::new(error.to_string()))?;
        Self::new(parsed)
    }
}

impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = i64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Node kind visible in the workspace tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Directory node.
    Directory,
    /// Regular file node.
    File,
    /// Symlink node.
    Symlink,
}

/// Workspace tree node metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub parent_id: Option<NodeId>,
    pub name: String,
    pub kind: NodeKind,
    pub current_rev: Option<RevisionId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub tombstone_version: Option<i64>,
}

/// Revision content identity for a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RevisionContent {
    /// Directory revision.
    Directory,
    /// File revision with blob/chunk identity.
    File {
        blob_id: BlobId,
        chunk_ids: Vec<BlobId>,
        content_hash: String,
        encryption_header: Option<String>,
    },
    /// Symlink revision preserving the target string.
    Symlink { target: String },
}

impl RevisionContent {
    const fn node_kind(&self) -> NodeKind {
        match self {
            Self::Directory => NodeKind::Directory,
            Self::File { .. } => NodeKind::File,
            Self::Symlink { .. } => NodeKind::Symlink,
        }
    }
}

/// Portable revision metadata and content identity for a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRevision {
    pub revision_id: RevisionId,
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub base_revision_id: Option<RevisionId>,
    pub content: RevisionContent,
    pub posix_mode: u32,
    pub mtime: DateTime<Utc>,
    pub size: u64,
    pub executable: bool,
    pub created_at: DateTime<Utc>,
}

/// Rule action strings supported by `.fs2ignore` and `.fs2/config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleAction {
    Ignore,
    LocalOnly,
    Generated,
    Lazy,
    Pin,
    Normal,
    Secret,
    DependencyCache,
}

/// Minimal shared rule payload used in operation logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsRule {
    pub action: RuleAction,
    pub manager: Option<String>,
    pub scope: Option<String>,
}

/// Secret classification for env values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    Secret,
    PlainConfig,
}

/// Env record scope used in shared operation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvScope {
    Workspace,
    Project {
        project_path: String,
    },
    Machine {
        device_id: DeviceId,
    },
    ProjectMachine {
        project_path: String,
        device_id: DeviceId,
    },
}

/// Minimal env metadata carried by `SetEnvVar` operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVarMetadata {
    pub env_name: String,
    pub environment: String,
    pub scope: EnvScope,
    pub secret_kind: SecretKind,
}

/// Client-generated operation submitted to the sync log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub op_id: OpId,
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub base_cursor: Cursor,
    pub kind: OperationKind,
    pub created_at: DateTime<Utc>,
}

impl Operation {
    /// Validates only self-contained operation shape invariants.
    pub fn validate_shape(&self) -> Result<(), ErrorEnvelope> {
        require(
            self.base_cursor.value() >= 0,
            "base_cursor",
            "must not be negative",
        )
        .and_then(|()| self.kind.validate_shape(self.workspace_id, self.device_id))
        .map_err(invalid_operation)
    }
}

/// Operation variants in the canonical operation log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    CreateNode {
        node_id: NodeId,
        parent_id: NodeId,
        name: String,
        kind: NodeKind,
        initial_revision: Option<NodeRevision>,
    },
    PutFileRevision {
        node_id: NodeId,
        base_revision_id: Option<RevisionId>,
        revision: NodeRevision,
    },
    MoveNode {
        node_id: NodeId,
        old_parent_id: NodeId,
        old_name: String,
        new_parent_id: NodeId,
        new_name: String,
    },
    DeleteNode {
        node_id: NodeId,
        recursive: bool,
    },
    RestoreNode {
        node_id: NodeId,
        parent_id: NodeId,
        name: String,
    },
    SetRule {
        path_pattern: String,
        rule: FsRule,
    },
    SetEnvVar {
        env_var_id: EnvVarId,
        encrypted_payload: String,
        metadata: EnvVarMetadata,
    },
    DeleteEnvVar {
        env_var_id: EnvVarId,
    },
}

impl OperationKind {
    fn validate_shape(
        &self,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
    ) -> Result<(), ValidationError> {
        match self {
            Self::CreateNode {
                node_id,
                name,
                kind,
                initial_revision,
                ..
            } => {
                validate_node_name(name, "name")?;
                if let Some(revision) = initial_revision {
                    validate_revision(revision, workspace_id, device_id, *kind)?;
                    require(
                        revision.node_id == *node_id,
                        "initial_revision.node_id",
                        "must match created node_id",
                    )?;
                }
            }
            Self::PutFileRevision {
                node_id,
                base_revision_id,
                revision,
            } => {
                validate_revision(revision, workspace_id, device_id, NodeKind::File)?;
                require(
                    revision.node_id == *node_id,
                    "revision.node_id",
                    "must match operation node_id",
                )?;
                require(
                    revision.base_revision_id == *base_revision_id,
                    "base_revision_id",
                    "must match revision.base_revision_id",
                )?;
            }
            Self::MoveNode {
                old_name, new_name, ..
            } => {
                validate_node_name(old_name, "old_name")?;
                validate_node_name(new_name, "new_name")?;
            }
            Self::DeleteNode { .. } | Self::DeleteEnvVar { .. } => {}
            Self::RestoreNode { name, .. } => validate_node_name(name, "name")?,
            Self::SetRule { path_pattern, rule } => {
                require_non_empty(path_pattern, "path_pattern")?;
                validate_rule(rule)?;
            }
            Self::SetEnvVar {
                encrypted_payload,
                metadata,
                ..
            } => {
                require_non_empty(encrypted_payload, "encrypted_payload")?;
                validate_env_metadata(metadata)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationError {
    field: &'static str,
    reason: &'static str,
}

fn invalid_operation(error: ValidationError) -> ErrorEnvelope {
    let mut details = Map::new();
    details.insert("field".to_owned(), Value::String(error.field.to_owned()));
    details.insert("reason".to_owned(), Value::String(error.reason.to_owned()));
    Fs2Error::InvalidOperation.to_error_envelope(details)
}

fn validate_revision(
    revision: &NodeRevision,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    expected_kind: NodeKind,
) -> Result<(), ValidationError> {
    require(
        revision.workspace_id == workspace_id,
        "revision.workspace_id",
        "must match operation workspace_id",
    )?;
    require(
        revision.device_id == device_id,
        "revision.device_id",
        "must match operation device_id",
    )?;
    require(
        revision.content.node_kind() == expected_kind,
        "revision.content",
        "must match node kind",
    )?;
    match &revision.content {
        RevisionContent::Directory => {}
        RevisionContent::File {
            blob_id,
            chunk_ids,
            content_hash,
            encryption_header,
        } => {
            require_non_empty(blob_id.as_str(), "revision.content.blob_id")?;
            for chunk_id in chunk_ids {
                require_non_empty(chunk_id.as_str(), "revision.content.chunk_ids")?;
            }
            require_non_empty(content_hash, "revision.content.content_hash")?;
            if let Some(header) = encryption_header {
                require_non_empty(header, "revision.content.encryption_header")?;
            }
        }
        RevisionContent::Symlink { target } => {
            require_non_empty(target, "revision.content.target")?;
        }
    }
    Ok(())
}

fn validate_rule(rule: &FsRule) -> Result<(), ValidationError> {
    if let Some(manager) = &rule.manager {
        require_non_empty(manager, "rule.manager")?;
        require(
            rule.action == RuleAction::DependencyCache,
            "rule.manager",
            "is only valid for dependency-cache rules",
        )?;
    }
    if let Some(scope) = &rule.scope {
        require_non_empty(scope, "rule.scope")?;
        require(
            rule.action == RuleAction::Secret,
            "rule.scope",
            "is only valid for secret rules",
        )?;
    }
    Ok(())
}

fn validate_env_metadata(metadata: &EnvVarMetadata) -> Result<(), ValidationError> {
    require(
        is_valid_env_var_name(&metadata.env_name),
        "metadata.env_name",
        "must be shell-portable ASCII identifier",
    )?;
    require(
        is_valid_environment_name(&metadata.environment),
        "metadata.environment",
        "must be lowercase ASCII environment name",
    )?;
    match &metadata.scope {
        EnvScope::Workspace | EnvScope::Machine { .. } => {}
        EnvScope::Project { project_path } | EnvScope::ProjectMachine { project_path, .. } => {
            require(
                WorkspacePath::parse(project_path).is_ok(),
                "metadata.scope.project_path",
                "must be a valid workspace-relative path",
            )?;
        }
    }
    Ok(())
}

fn is_valid_env_var_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_valid_environment_name(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && chars.all(|ch| ch == '-' || ch == '_' || ch.is_ascii_lowercase() || ch.is_ascii_digit())
}

fn validate_node_name(name: &str, field: &'static str) -> Result<(), ValidationError> {
    require(
        NodeName::parse(name).is_ok(),
        field,
        "must be a valid node name",
    )?;
    Ok(())
}

const fn require_non_empty(value: &str, field: &'static str) -> Result<(), ValidationError> {
    require(!value.is_empty(), field, "must not be empty")
}

const fn require(
    condition: bool,
    field: &'static str,
    reason: &'static str,
) -> Result<(), ValidationError> {
    if condition {
        Ok(())
    } else {
        Err(ValidationError { field, reason })
    }
}

/// Stable FS2 error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fs2Error {
    Unauthorized,
    DeviceRevoked,
    WorkspaceNotFound,
    NodeNotFound,
    PathCollision,
    RevisionConflict,
    BlobMissing,
    InvalidOperation,
    QuotaExceeded,
    RateLimited,
    Offline,
    NotHydrated,
    SecretUnavailable,
}

impl Fs2Error {
    /// Stable `snake_case` error code.
    pub const fn code(self) -> &'static str {
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

    /// Secret-safe default message for users and machines.
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::Unauthorized => "Authentication is required.",
            Self::DeviceRevoked => "This device has been revoked.",
            Self::WorkspaceNotFound => "Workspace was not found.",
            Self::NodeNotFound => "Node was not found.",
            Self::PathCollision => "Path collides with an existing workspace entry.",
            Self::RevisionConflict => "Node revision changed before operation was applied.",
            Self::BlobMissing => "Blob was not found.",
            Self::InvalidOperation => "Operation is invalid.",
            Self::QuotaExceeded => "Storage quota was exceeded.",
            Self::RateLimited => "Request was rate limited.",
            Self::Offline => "Backend is unreachable.",
            Self::NotHydrated => "File content is not hydrated locally.",
            Self::SecretUnavailable => "Secret material is unavailable.",
        }
    }

    /// HTTP-like status code without depending on a web framework.
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::DeviceRevoked => 403,
            Self::WorkspaceNotFound | Self::NodeNotFound | Self::BlobMissing => 404,
            Self::PathCollision | Self::RevisionConflict | Self::NotHydrated => 409,
            Self::InvalidOperation => 400,
            Self::QuotaExceeded => 507,
            Self::RateLimited => 429,
            Self::Offline | Self::SecretUnavailable => 503,
        }
    }

    /// Builds a structured error envelope.
    pub fn to_error_envelope(self, details: Map<String, Value>) -> ErrorEnvelope {
        ErrorEnvelope {
            error: ErrorBody {
                code: self,
                message: self.default_message().to_owned(),
                details,
            },
        }
    }

    /// Builds a framework-neutral HTTP error view.
    pub fn to_http_error(self, details: Map<String, Value>) -> HttpError {
        HttpError {
            status: self.http_status(),
            body: self.to_error_envelope(details),
        }
    }

    /// Builds a CLI-friendly message view.
    pub const fn to_cli_message(self) -> CliErrorMessage {
        CliErrorMessage {
            code: self.code(),
            message: self.default_message(),
            hint: self.cli_hint(),
        }
    }

    const fn cli_hint(self) -> &'static str {
        match self {
            Self::Unauthorized => "Run `fs2 login` and try again.",
            Self::DeviceRevoked => "Re-enroll this device from a trusted device.",
            Self::WorkspaceNotFound => "Run `fs2 workspace list` to inspect available workspaces.",
            Self::NodeNotFound => "Check the path and run `fs2 status`.",
            Self::PathCollision => "Rename one of the colliding paths.",
            Self::RevisionConflict => "Resolve the conflict before retrying the write.",
            Self::BlobMissing => "Retry hydration or check backend blob storage.",
            Self::InvalidOperation => "Inspect operation details and retry with a valid request.",
            Self::QuotaExceeded => "Free space or increase quota before retrying.",
            Self::RateLimited => "Wait before retrying.",
            Self::Offline => "Check network connectivity and backend status.",
            Self::NotHydrated => "Hydrate the file before using it offline.",
            Self::SecretUnavailable => "Unlock local key storage or re-enroll this device.",
        }
    }
}

/// Error envelope matching the backend API design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// Structured error body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: Fs2Error,
    pub message: String,
    pub details: Map<String, Value>,
}

/// Framework-neutral HTTP error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpError {
    pub status: u16,
    pub body: ErrorEnvelope,
}

/// Plain CLI error message parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliErrorMessage {
    pub code: &'static str,
    pub message: &'static str,
    pub hint: &'static str,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn uuid_from(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    macro_rules! id_round_trip_test {
        ($test_name:ident, $id_type:ident) => {
            proptest! {
                #[test]
                fn $test_name(value: u128) {
                    let id = $id_type::from_uuid(uuid_from(value));
                    let rendered = id.to_string();
                    prop_assert_eq!(rendered.parse::<$id_type>(), Ok(id));
                    let value = serde_json::to_value(id)
                        .map_err(|error| TestCaseError::fail(error.to_string()))?;
                    prop_assert_eq!(value, Value::String(rendered));
                }
            }
        };
    }

    id_round_trip_test!(user_id_round_trips, UserId);
    id_round_trip_test!(workspace_id_round_trips, WorkspaceId);
    id_round_trip_test!(device_id_round_trips, DeviceId);
    id_round_trip_test!(node_id_round_trips, NodeId);
    id_round_trip_test!(revision_id_round_trips, RevisionId);
    id_round_trip_test!(op_id_round_trips, OpId);
    id_round_trip_test!(env_var_id_round_trips, EnvVarId);

    proptest! {
        #[test]
        fn blob_id_property_round_trips(value in "[A-Za-z0-9:._/-]{1,64}") {
            let id: BlobId = value.parse()
                .map_err(|error: ParseFs2IdError| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(id.to_string(), value.as_str());
            let json_value = serde_json::to_value(&id)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(json_value, Value::String(value));
        }

        #[test]
        fn cursor_property_round_trips(value in 0i64..i64::MAX) {
            let cursor = Cursor::new(value)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(cursor.to_string(), value.to_string());
            let parsed = cursor.to_string().parse::<Cursor>()
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(parsed, cursor);
            let json_value = serde_json::to_value(cursor)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(json_value, json!(value));
        }
    }

    #[test]
    fn blob_id_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let id: BlobId = "sha256:abc".parse()?;
        assert_eq!(id.to_string(), "sha256:abc");
        assert_eq!(serde_json::to_value(&id)?, json!("sha256:abc"));
        assert!("".parse::<BlobId>().is_err());
        Ok(())
    }

    #[test]
    fn cursor_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let cursor: Cursor = "42".parse()?;
        assert_eq!(cursor.value(), 42);
        assert_eq!(serde_json::to_value(cursor)?, json!(42));
        assert!(Cursor::new(-1).is_err());
        assert!(serde_json::from_value::<Cursor>(json!(-1)).is_err());
        assert!(serde_json::from_value::<BlobId>(json!("")).is_err());
        Ok(())
    }

    #[test]
    fn node_tree_serializes_with_identical_values() -> Result<(), Box<dyn std::error::Error>> {
        let now = sample_time()?;
        let node = Node {
            node_id: NodeId::from_uuid(uuid_from(1)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            parent_id: Some(NodeId::from_uuid(uuid_from(3))),
            name: "src".to_owned(),
            kind: NodeKind::Directory,
            current_rev: Some(RevisionId::from_uuid(uuid_from(4))),
            created_at: now,
            updated_at: now,
            deleted_at: None,
            tombstone_version: None,
        };
        let expected = json!({
            "node_id":NodeId::from_uuid(uuid_from(1)).to_string(),
            "workspace_id":WorkspaceId::from_uuid(uuid_from(2)).to_string(),
            "parent_id":NodeId::from_uuid(uuid_from(3)).to_string(),
            "name":"src",
            "kind":"directory",
            "current_rev":RevisionId::from_uuid(uuid_from(4)).to_string(),
            "created_at":"2026-06-28T13:04:11Z",
            "updated_at":"2026-06-28T13:04:11Z",
            "deleted_at":null,
            "tombstone_version":null
        });
        let value = serde_json::to_value(&node)?;
        assert_eq!(value, expected);
        let parsed: Node = serde_json::from_value(value)?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn node_revision_snapshot_is_stable() -> Result<(), Box<dyn std::error::Error>> {
        let revision = sample_revision(NodeKind::File)?;
        let expected = json!({
            "revision_id":RevisionId::from_uuid(uuid_from(30)).to_string(),
            "node_id":NodeId::from_uuid(uuid_from(4)).to_string(),
            "workspace_id":WorkspaceId::from_uuid(uuid_from(2)).to_string(),
            "device_id":DeviceId::from_uuid(uuid_from(3)).to_string(),
            "base_revision_id":RevisionId::from_uuid(uuid_from(5)).to_string(),
            "content":{
                "type":"file",
                "blob_id":"sha256:ciphertext",
                "chunk_ids":[],
                "content_hash":"plain:hash",
                "encryption_header":"v1:header"
            },
            "posix_mode":420,
            "mtime":"2026-06-28T13:04:11Z",
            "size":12,
            "executable":false,
            "created_at":"2026-06-28T13:04:11Z"
        });
        let value = serde_json::to_value(&revision)?;
        assert_eq!(value, expected);
        let parsed: NodeRevision = serde_json::from_value(value)?;
        assert_eq!(parsed, revision);
        Ok(())
    }

    #[test]
    fn revision_content_snapshots_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let file = RevisionContent::File {
            blob_id: "sha256:ciphertext".parse()?,
            chunk_ids: vec!["sha256:chunk".parse()?],
            content_hash: "plain:hash".to_owned(),
            encryption_header: Some("v1:header".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(RevisionContent::Directory)?,
            json!({"type":"directory"})
        );
        assert_eq!(
            serde_json::to_value(file)?,
            json!({
                "type":"file",
                "blob_id":"sha256:ciphertext",
                "chunk_ids":["sha256:chunk"],
                "content_hash":"plain:hash",
                "encryption_header":"v1:header"
            })
        );
        assert_eq!(
            serde_json::to_value(RevisionContent::Symlink {
                target: "../README.md".to_owned()
            })?,
            json!({"type":"symlink","target":"../README.md"})
        );
        Ok(())
    }

    #[test]
    fn operation_kind_snapshots_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let revision = sample_revision(NodeKind::File)?;
        let revision_json = serde_json::to_value(&revision)?;
        let env_var_id = EnvVarId::from_uuid(uuid_from(16));
        let cases = vec![
            (
                OperationKind::CreateNode {
                    node_id: NodeId::from_uuid(uuid_from(10)),
                    parent_id: NodeId::from_uuid(uuid_from(11)),
                    name: "file.txt".to_owned(),
                    kind: NodeKind::File,
                    initial_revision: Some(revision.clone()),
                },
                json!({
                    "type":"create_node",
                    "node_id":NodeId::from_uuid(uuid_from(10)).to_string(),
                    "parent_id":NodeId::from_uuid(uuid_from(11)).to_string(),
                    "name":"file.txt",
                    "kind":"file",
                    "initial_revision":revision_json
                }),
            ),
            (
                OperationKind::PutFileRevision {
                    node_id: revision.node_id,
                    base_revision_id: revision.base_revision_id,
                    revision: revision.clone(),
                },
                json!({
                    "type":"put_file_revision",
                    "node_id":revision.node_id.to_string(),
                    "base_revision_id":revision.base_revision_id.map(|id| id.to_string()),
                    "revision":revision_json
                }),
            ),
            (
                OperationKind::MoveNode {
                    node_id: NodeId::from_uuid(uuid_from(12)),
                    old_parent_id: NodeId::from_uuid(uuid_from(11)),
                    old_name: "old.txt".to_owned(),
                    new_parent_id: NodeId::from_uuid(uuid_from(13)),
                    new_name: "new.txt".to_owned(),
                },
                json!({
                    "type":"move_node",
                    "node_id":NodeId::from_uuid(uuid_from(12)).to_string(),
                    "old_parent_id":NodeId::from_uuid(uuid_from(11)).to_string(),
                    "old_name":"old.txt",
                    "new_parent_id":NodeId::from_uuid(uuid_from(13)).to_string(),
                    "new_name":"new.txt"
                }),
            ),
            (
                OperationKind::DeleteNode {
                    node_id: NodeId::from_uuid(uuid_from(14)),
                    recursive: true,
                },
                json!({
                    "type":"delete_node",
                    "node_id":NodeId::from_uuid(uuid_from(14)).to_string(),
                    "recursive":true
                }),
            ),
            (
                OperationKind::RestoreNode {
                    node_id: NodeId::from_uuid(uuid_from(15)),
                    parent_id: NodeId::from_uuid(uuid_from(11)),
                    name: "restored.txt".to_owned(),
                },
                json!({
                    "type":"restore_node",
                    "node_id":NodeId::from_uuid(uuid_from(15)).to_string(),
                    "parent_id":NodeId::from_uuid(uuid_from(11)).to_string(),
                    "name":"restored.txt"
                }),
            ),
            (
                OperationKind::SetRule {
                    path_pattern: "node_modules/**".to_owned(),
                    rule: FsRule {
                        action: RuleAction::DependencyCache,
                        manager: Some("node".to_owned()),
                        scope: None,
                    },
                },
                json!({
                    "type":"set_rule",
                    "path_pattern":"node_modules/**",
                    "rule":{"action":"dependency-cache","manager":"node","scope":null}
                }),
            ),
            (
                OperationKind::SetEnvVar {
                    env_var_id,
                    encrypted_payload: "encrypted-envelope".to_owned(),
                    metadata: EnvVarMetadata {
                        env_name: "STRIPE_SECRET_KEY".to_owned(),
                        environment: "dev".to_owned(),
                        scope: EnvScope::Project {
                            project_path: "apps/web".to_owned(),
                        },
                        secret_kind: SecretKind::Secret,
                    },
                },
                json!({
                    "type":"set_env_var",
                    "env_var_id":env_var_id.to_string(),
                    "encrypted_payload":"encrypted-envelope",
                    "metadata":{
                        "env_name":"STRIPE_SECRET_KEY",
                        "environment":"dev",
                        "scope":{"type":"project","project_path":"apps/web"},
                        "secret_kind":"secret"
                    }
                }),
            ),
            (
                OperationKind::DeleteEnvVar { env_var_id },
                json!({"type":"delete_env_var","env_var_id":env_var_id.to_string()}),
            ),
        ];

        for (case, expected) in cases {
            let value = serde_json::to_value(&case)?;
            assert_eq!(value, expected);
            let parsed: OperationKind = serde_json::from_value(value)?;
            assert_eq!(parsed, case);
        }
        Ok(())
    }

    #[test]
    fn operation_validates_self_contained_shape() -> Result<(), Box<dyn std::error::Error>> {
        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(20)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::PutFileRevision {
                node_id: NodeId::from_uuid(uuid_from(4)),
                base_revision_id: Some(RevisionId::from_uuid(uuid_from(5))),
                revision: sample_revision(NodeKind::File)?,
            },
            created_at: sample_time()?,
        };
        assert!(operation.validate_shape().is_ok());
        Ok(())
    }

    #[test]
    fn operation_validation_rejects_bad_shapes() -> Result<(), Box<dyn std::error::Error>> {
        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(20)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::MoveNode {
                node_id: NodeId::from_uuid(uuid_from(4)),
                old_parent_id: NodeId::from_uuid(uuid_from(5)),
                old_name: "bad/name".to_owned(),
                new_parent_id: NodeId::from_uuid(uuid_from(6)),
                new_name: "new".to_owned(),
            },
            created_at: sample_time()?,
        };
        let error = operation.validate_shape().err();
        assert!(matches!(
            error,
            Some(ErrorEnvelope {
                error: ErrorBody {
                    code: Fs2Error::InvalidOperation,
                    ..
                }
            })
        ));
        let mut revision = sample_revision(NodeKind::File)?;
        revision.base_revision_id = None;
        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(21)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::PutFileRevision {
                node_id: NodeId::from_uuid(uuid_from(4)),
                base_revision_id: Some(RevisionId::from_uuid(uuid_from(5))),
                revision,
            },
            created_at: sample_time()?,
        };
        assert!(operation.validate_shape().is_err());
        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(22)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::RestoreNode {
                node_id: NodeId::from_uuid(uuid_from(4)),
                parent_id: NodeId::from_uuid(uuid_from(5)),
                name: "..".to_owned(),
            },
            created_at: sample_time()?,
        };
        assert!(operation.validate_shape().is_err());

        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(23)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::SetEnvVar {
                env_var_id: EnvVarId::from_uuid(uuid_from(16)),
                encrypted_payload: "encrypted-envelope".to_owned(),
                metadata: EnvVarMetadata {
                    env_name: "STRIPE_SECRET_KEY".to_owned(),
                    environment: "dev".to_owned(),
                    scope: EnvScope::Project {
                        project_path: "../x".to_owned(),
                    },
                    secret_kind: SecretKind::Secret,
                },
            },
            created_at: sample_time()?,
        };
        assert!(operation.validate_shape().is_err());

        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(24)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::SetEnvVar {
                env_var_id: EnvVarId::from_uuid(uuid_from(16)),
                encrypted_payload: "encrypted-envelope".to_owned(),
                metadata: EnvVarMetadata {
                    env_name: "2BAD".to_owned(),
                    environment: "dev".to_owned(),
                    scope: EnvScope::Workspace,
                    secret_kind: SecretKind::Secret,
                },
            },
            created_at: sample_time()?,
        };
        assert!(operation.validate_shape().is_err());

        let operation = Operation {
            op_id: OpId::from_uuid(uuid_from(25)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::SetEnvVar {
                env_var_id: EnvVarId::from_uuid(uuid_from(16)),
                encrypted_payload: "encrypted-envelope".to_owned(),
                metadata: EnvVarMetadata {
                    env_name: "STRIPE_SECRET_KEY".to_owned(),
                    environment: "Dev".to_owned(),
                    scope: EnvScope::Workspace,
                    secret_kind: SecretKind::Secret,
                },
            },
            created_at: sample_time()?,
        };
        assert!(operation.validate_shape().is_err());
        Ok(())
    }

    #[test]
    fn error_contracts_are_stable() -> Result<(), Box<dyn std::error::Error>> {
        let codes = [
            (Fs2Error::Unauthorized, 401),
            (Fs2Error::DeviceRevoked, 403),
            (Fs2Error::WorkspaceNotFound, 404),
            (Fs2Error::NodeNotFound, 404),
            (Fs2Error::PathCollision, 409),
            (Fs2Error::RevisionConflict, 409),
            (Fs2Error::BlobMissing, 404),
            (Fs2Error::InvalidOperation, 400),
            (Fs2Error::QuotaExceeded, 507),
            (Fs2Error::RateLimited, 429),
            (Fs2Error::Offline, 503),
            (Fs2Error::NotHydrated, 409),
            (Fs2Error::SecretUnavailable, 503),
        ];
        for (code, status) in codes {
            assert_eq!(code.http_status(), status);
            assert_eq!(serde_json::to_value(code)?, json!(code.code()));
            let http_error = code.to_http_error(Map::new());
            assert_eq!(http_error.status, status);
            assert_eq!(http_error.body.error.code, code);
            assert_eq!(code.to_cli_message().code, code.code());
        }
        assert_eq!(
            serde_json::to_value(Fs2Error::RevisionConflict.to_error_envelope(Map::new()))?,
            json!({
                "error": {
                    "code": "revision_conflict",
                    "message": "Node revision changed before operation was applied.",
                    "details": {}
                }
            })
        );
        Ok(())
    }

    fn sample_time() -> Result<DateTime<Utc>, Box<dyn std::error::Error>> {
        Ok(DateTime::parse_from_rfc3339("2026-06-28T13:04:11Z")?.with_timezone(&Utc))
    }

    fn sample_revision(kind: NodeKind) -> Result<NodeRevision, Box<dyn std::error::Error>> {
        let content = match kind {
            NodeKind::Directory => RevisionContent::Directory,
            NodeKind::File => RevisionContent::File {
                blob_id: "sha256:ciphertext".parse()?,
                chunk_ids: Vec::new(),
                content_hash: "plain:hash".to_owned(),
                encryption_header: Some("v1:header".to_owned()),
            },
            NodeKind::Symlink => RevisionContent::Symlink {
                target: "../README.md".to_owned(),
            },
        };
        Ok(NodeRevision {
            revision_id: RevisionId::from_uuid(uuid_from(30)),
            node_id: NodeId::from_uuid(uuid_from(4)),
            workspace_id: WorkspaceId::from_uuid(uuid_from(2)),
            device_id: DeviceId::from_uuid(uuid_from(3)),
            base_revision_id: Some(RevisionId::from_uuid(uuid_from(5))),
            content,
            posix_mode: 0o644,
            mtime: sample_time()?,
            size: 12,
            executable: false,
            created_at: sample_time()?,
        })
    }
}
