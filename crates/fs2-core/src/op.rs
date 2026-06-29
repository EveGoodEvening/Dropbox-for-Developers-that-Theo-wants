//! Operation log: the canonical sync primitive.
//!
//! Clients submit operations; the backend validates them, assigns an ordering
//! cursor, and broadcasts them to other devices. Every operation carries a
//! stable client-generated `op_id` for idempotency.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::id::{Cursor, DeviceId, NodeId, OpId, RevisionId, WorkspaceId};
use crate::node::{NodeKind, NodeRevision};

/// An operation submitted by a device and committed by the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Stable client-generated id for idempotency.
    pub op_id: OpId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Device that authored the operation.
    pub device_id: DeviceId,
    /// Cursor the client was at when it generated the operation.
    pub base_cursor: Cursor,
    /// Operation payload.
    pub kind: OperationKind,
    /// When the operation was created on the client.
    pub created_at: DateTime<Utc>,
}

/// The payload of an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    /// Create a new node under an existing parent directory.
    CreateNode {
        /// Parent directory node id.
        parent_id: NodeId,
        /// Name of the new node.
        name: String,
        /// Kind of the new node.
        kind: NodeKind,
        /// Initial revision, if the node is a file or symlink.
        initial_revision: Option<NodeRevision>,
    },
    /// Replace a file's content with a new revision.
    PutFileRevision {
        /// Target node id.
        node_id: NodeId,
        /// Revision the client based this update on.
        base_revision_id: Option<RevisionId>,
        /// New revision to apply.
        revision: NodeRevision,
    },
    /// Move or rename a node.
    MoveNode {
        /// Node being moved.
        node_id: NodeId,
        /// Previous parent.
        old_parent_id: NodeId,
        /// Previous name.
        old_name: String,
        /// New parent.
        new_parent_id: NodeId,
        /// New name.
        new_name: String,
    },
    /// Delete a node (creates a tombstone).
    DeleteNode {
        /// Node to delete.
        node_id: NodeId,
        /// Whether to delete recursively (required for non-empty directories).
        recursive: bool,
    },
    /// Restore a tombstoned node.
    RestoreNode {
        /// Node to restore.
        node_id: NodeId,
        /// Parent to restore under.
        parent_id: NodeId,
        /// Name to restore with.
        name: String,
    },
    /// Set a path rule.
    SetRule {
        /// Glob pattern the rule applies to.
        path_pattern: String,
        /// Serialized rule payload.
        rule: RulePayload,
    },
    /// Set or update an environment variable.
    SetEnvVar {
        /// Env var id.
        env_var_id: Uuid,
        /// Encrypted payload (ciphertext, base64).
        encrypted_payload: String,
        /// Public metadata for the env var.
        metadata: EnvVarMetadata,
    },
    /// Delete an environment variable.
    DeleteEnvVar {
        /// Env var id.
        env_var_id: Uuid,
    },
}

/// Serialized rule payload carried by [`OperationKind::SetRule`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePayload {
    /// Action name (e.g. `ignore`, `generated`).
    pub action: String,
    /// Optional source identifier (which config file/profile this came from).
    pub source: Option<String>,
    /// Optional extra metadata.
    pub metadata: Option<serde_json::Value>,
}

/// Public metadata for an env var. Never contains the plaintext value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVarMetadata {
    /// Variable name, e.g. `STRIPE_SECRET_KEY`.
    pub name: String,
    /// Environment, e.g. `dev`, `test`, `prod`.
    pub environment: String,
    /// Project path within the workspace, or `None` for workspace scope.
    pub project_path: Option<String>,
    /// Scope of the variable.
    pub scope: EnvScope,
    /// Whether the value is a secret or plain config.
    pub secret_kind: SecretKind,
    /// Last updated timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Scope of an environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvScope {
    /// Applies to the whole workspace.
    Workspace,
    /// Applies to a project path.
    Project,
    /// Applies to a specific device.
    Machine {
        /// Device id.
        device_id: DeviceId,
    },
    /// Applies to a project on a specific device.
    ProjectMachine {
        /// Project path.
        project_path: String,
        /// Device id.
        device_id: DeviceId,
    },
}

/// Whether an env var is a secret or plain config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// Encrypted and redacted in all output.
    Secret,
    /// Synced as config but still encrypted at rest.
    PlainConfig,
}

impl Operation {
    /// Construct a new operation with a fresh `op_id` and the given fields.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        base_cursor: Cursor,
        kind: OperationKind,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            op_id: OpId::new(),
            workspace_id,
            device_id,
            base_cursor,
            kind,
            created_at,
        }
    }

    /// Returns whether the operation mutates a node identified by `node_id`.
    #[must_use]
    pub fn target_node(&self) -> Option<NodeId> {
        match &self.kind {
            OperationKind::PutFileRevision { node_id, .. }
            | OperationKind::MoveNode { node_id, .. }
            | OperationKind::DeleteNode { node_id, .. }
            | OperationKind::RestoreNode { node_id, .. } => Some(*node_id),
            OperationKind::CreateNode { .. }
            | OperationKind::SetRule { .. }
            | OperationKind::SetEnvVar { .. }
            | OperationKind::DeleteEnvVar { .. } => None,
        }
    }
}

/// Lightweight shape validation for operations.
///
/// This checks invariants that can be verified without database state: that
/// names are non-empty, that file revisions target file nodes, etc. Full
/// validation against workspace state happens at the backend.
///
/// # Errors
/// Returns [`Fs2Error::InvalidOperation`] when a shape invariant is violated.
pub fn validate_shape(op: &OperationKind) -> Result<(), crate::error::Fs2Error> {
    use crate::error::{Fs2Error, Fs2ErrorCode};
    match op {
        OperationKind::CreateNode { name, .. } => {
            if name.is_empty() {
                return Err(Fs2Error::new(
                    Fs2ErrorCode::InvalidOperation,
                    "create_node name must not be empty",
                ));
            }
            Ok(())
        }
        OperationKind::PutFileRevision { revision, .. } => {
            if !matches!(revision.content, crate::node::RevisionContent::File { .. }) {
                return Err(Fs2Error::new(
                    Fs2ErrorCode::InvalidOperation,
                    "put_file_revision must target a file content",
                ));
            }
            Ok(())
        }
        OperationKind::MoveNode { new_name, .. } => {
            if new_name.is_empty() {
                return Err(Fs2Error::new(
                    Fs2ErrorCode::InvalidOperation,
                    "move_node new_name must not be empty",
                ));
            }
            Ok(())
        }
        OperationKind::RestoreNode { name, .. } => {
            if name.is_empty() {
                return Err(Fs2Error::new(
                    Fs2ErrorCode::InvalidOperation,
                    "restore_node name must not be empty",
                ));
            }
            Ok(())
        }
        OperationKind::DeleteNode { .. }
        | OperationKind::SetRule { .. }
        | OperationKind::SetEnvVar { .. }
        | OperationKind::DeleteEnvVar { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{DeviceId, NodeId, WorkspaceId};
    use crate::node::{NodeKind, RevisionContent};

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn create_node_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: NodeId::new(),
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"create_node\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn put_file_revision_snapshot() {
        let node_id = NodeId::new();
        let rev = NodeRevision {
            revision_id: RevisionId::new(),
            node_id,
            workspace_id: WorkspaceId::new(),
            device_id: DeviceId::new(),
            base_revision_id: None,
            content: RevisionContent::File {
                blob_id: "sha256:abc".to_owned(),
                chunk_ids: vec![],
                content_hash: "sha256:def".to_owned(),
                encryption_header: None,
            },
            posix_mode: 0o644,
            mtime: now(),
            size: 10,
            executable: false,
            created_at: now(),
        };
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::PutFileRevision {
                node_id,
                base_revision_id: None,
                revision: rev,
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"put_file_revision\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn move_node_target() {
        let nid = NodeId::new();
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::MoveNode {
                node_id: nid,
                old_parent_id: NodeId::new(),
                old_name: "a".to_owned(),
                new_parent_id: NodeId::new(),
                new_name: "b".to_owned(),
            },
            now(),
        );
        assert_eq!(op.target_node(), Some(nid));
    }

    #[test]
    fn validate_shape_rejects_empty_name() {
        let op = OperationKind::CreateNode {
            parent_id: NodeId::new(),
            name: String::new(),
            kind: NodeKind::Directory,
            initial_revision: None,
        };
        assert!(validate_shape(&op).is_err());
    }

    #[test]
    fn validate_shape_rejects_non_file_revision() {
        let node_id = NodeId::new();
        let rev =
            NodeRevision::initial_directory(node_id, WorkspaceId::new(), DeviceId::new(), now());
        let op = OperationKind::PutFileRevision {
            node_id,
            base_revision_id: None,
            revision: rev,
        };
        assert!(validate_shape(&op).is_err());
    }

    #[test]
    fn env_scope_roundtrip() {
        let s = EnvScope::ProjectMachine {
            project_path: "apps/web".to_owned(),
            device_id: DeviceId::new(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"kind\":\"project_machine\""));
        let back: EnvScope = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn move_node_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::MoveNode {
                node_id: NodeId::new(),
                old_parent_id: NodeId::new(),
                old_name: "a".to_owned(),
                new_parent_id: NodeId::new(),
                new_name: "b".to_owned(),
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"move_node\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn delete_node_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::DeleteNode {
                node_id: NodeId::new(),
                recursive: true,
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"delete_node\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn restore_node_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::RestoreNode {
                node_id: NodeId::new(),
                parent_id: NodeId::new(),
                name: "restored".to_owned(),
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"restore_node\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn set_rule_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::SetRule {
                path_pattern: "node_modules/**".to_owned(),
                rule: RulePayload {
                    action: "dependency-cache".to_owned(),
                    source: Some("config".to_owned()),
                    metadata: None,
                },
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"set_rule\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn set_env_var_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::SetEnvVar {
                env_var_id: Uuid::new_v4(),
                encrypted_payload: "base64ciphertext".to_owned(),
                metadata: EnvVarMetadata {
                    name: "STRIPE_SECRET_KEY".to_owned(),
                    environment: "dev".to_owned(),
                    project_path: Some("apps/web".to_owned()),
                    scope: EnvScope::Project,
                    secret_kind: SecretKind::Secret,
                    updated_at: now(),
                },
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"set_env_var\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn delete_env_var_snapshot() {
        let op = Operation::new(
            WorkspaceId::new(),
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::DeleteEnvVar {
                env_var_id: Uuid::new_v4(),
            },
            now(),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"delete_env_var\""));
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }
}
