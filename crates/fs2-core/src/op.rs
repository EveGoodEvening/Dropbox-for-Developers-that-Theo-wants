//! Operation log model — the canonical sync primitive.
//!
//! Clients submit operations; the backend validates them, assigns an ordering
//! cursor, and broadcasts them to other devices. Every operation carries a
//! stable `op_id` so submission is idempotent under retry.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{Cursor, DeviceId, NodeId, OpId, RevisionId, WorkspaceId};
use crate::node::{NodeKind, NodeRevision};

/// A single operation submitted to the workspace operation log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Stable id used for idempotent deduplication by the backend.
    pub op_id: OpId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Device that authored this operation.
    pub device_id: DeviceId,
    /// Cursor the submitting device had observed when forming this op.
    ///
    /// Used for diagnostics; the backend assigns the real ordering cursor.
    pub base_cursor: Cursor,
    /// Kind-specific payload.
    pub kind: OperationKind,
    /// When the client created this operation.
    pub created_at: DateTime<Utc>,
}

/// Kind-specific payload of an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    /// Create a new node under an existing parent.
    CreateNode {
        /// Parent directory node. Must exist and be a live directory.
        parent_id: NodeId,
        /// Displayed name of the new node.
        name: String,
        /// Kind of the new node.
        kind: NodeKind,
        /// Optional initial revision (e.g. file content for a new file).
        initial_revision: Option<NodeRevision>,
    },
    /// Replace a file's current revision with a new one.
    PutFileRevision {
        /// Target file node.
        node_id: NodeId,
        /// Revision the submitting device observed before editing.
        ///
        /// `None` is only valid for the initial revision of a freshly created
        /// file.
        base_revision_id: Option<RevisionId>,
        /// The new revision to apply.
        revision: NodeRevision,
    },
    /// Move and/or rename a node.
    MoveNode {
        /// Node being moved.
        node_id: NodeId,
        /// Current parent (for validation).
        old_parent_id: NodeId,
        /// Current name (for validation).
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
        /// Whether to delete non-empty directories recursively.
        recursive: bool,
    },
    /// Restore a tombstoned node.
    RestoreNode {
        /// Tombstoned node to restore.
        node_id: NodeId,
        /// Parent to restore under.
        parent_id: NodeId,
        /// Name to restore with.
        name: String,
    },
    /// Set or update a path rule.
    SetRule {
        /// Glob pattern this rule applies to.
        path_pattern: String,
        /// Serialized rule payload.
        rule: serde_json::Value,
    },
    /// Set or update an encrypted env var.
    SetEnvVar {
        /// Stable id for this env var.
        env_var_id: uuid::Uuid,
        /// Encrypted payload (server cannot decrypt).
        encrypted_payload: String,
        /// Non-secret metadata visible to the server.
        metadata: serde_json::Value,
    },
    /// Delete an env var.
    DeleteEnvVar {
        /// Stable id of the env var to delete.
        env_var_id: uuid::Uuid,
    },
}

impl OperationKind {
    /// Stable string tag used for DB columns and dispatch.
    #[must_use]
    pub fn type_tag(&self) -> &'static str {
        match self {
            Self::CreateNode { .. } => "create_node",
            Self::PutFileRevision { .. } => "put_file_revision",
            Self::MoveNode { .. } => "move_node",
            Self::DeleteNode { .. } => "delete_node",
            Self::RestoreNode { .. } => "restore_node",
            Self::SetRule { .. } => "set_rule",
            Self::SetEnvVar { .. } => "set_env_var",
            Self::DeleteEnvVar { .. } => "delete_env_var",
        }
    }

    /// Returns the node id this operation primarily targets, if any.
    ///
    /// Useful for conflict attribution and validation dispatch.
    #[must_use]
    pub fn target_node(&self) -> Option<NodeId> {
        match self {
            Self::PutFileRevision { node_id, .. }
            | Self::MoveNode { node_id, .. }
            | Self::DeleteNode { node_id, .. }
            | Self::RestoreNode { node_id, .. } => Some(*node_id),
            Self::CreateNode { .. }
            | Self::SetRule { .. }
            | Self::SetEnvVar { .. }
            | Self::DeleteEnvVar { .. } => None,
        }
    }
}

/// Lightweight shape validation for an operation.
///
/// This checks invariants that can be verified without external state (e.g.
/// "a `PutFileRevision` must target a file-shaped revision"). Stateful checks
/// (parent exists, no collision, base revision matches) live in the backend
/// and local store layers.
///
/// # Errors
/// Returns a static error description if the operation is structurally
/// invalid (empty/null names, mismatched revision content, etc.).
pub fn validate_shape(op: &OperationKind) -> Result<(), &'static str> {
    match op {
        OperationKind::CreateNode { name, kind, .. } => {
            if name.is_empty() || name.contains('\0') {
                return Err("create_node: invalid name");
            }
            if matches!(kind, NodeKind::File) {
                // initial_revision is optional; a file may be created empty and
                // receive its first revision via PutFileRevision.
            }
            Ok(())
        }
        OperationKind::PutFileRevision {
            node_id,
            revision,
            base_revision_id,
            ..
        } => {
            if !revision.content.is_file() {
                return Err("put_file_revision: revision content is not a file");
            }
            if &revision.node_id != node_id {
                return Err("put_file_revision: revision node_id mismatch");
            }
            if base_revision_id.is_none() && revision.base_revision_id.is_some() {
                // base on the op says "initial" but the revision claims a base
                return Err("put_file_revision: inconsistent base revision");
            }
            Ok(())
        }
        OperationKind::MoveNode {
            old_name, new_name, ..
        } => {
            if old_name.is_empty() || new_name.is_empty() {
                return Err("move_node: empty name");
            }
            if old_name.contains('\0') || new_name.contains('\0') {
                return Err("move_node: null byte in name");
            }
            Ok(())
        }
        OperationKind::DeleteNode { .. } | OperationKind::DeleteEnvVar { .. } => Ok(()),
        OperationKind::RestoreNode { name, .. } => {
            if name.is_empty() || name.contains('\0') {
                return Err("restore_node: invalid name");
            }
            Ok(())
        }
        OperationKind::SetRule { path_pattern, .. } => {
            if path_pattern.is_empty() {
                return Err("set_rule: empty pattern");
            }
            Ok(())
        }
        OperationKind::SetEnvVar {
            encrypted_payload, ..
        } => {
            if encrypted_payload.is_empty() {
                return Err("set_env_var: empty encrypted payload");
            }
            Ok(())
        }
    }
}

// Re-export the validate_shape helper under a more discoverable path on the
// OperationKind type surface for callers that prefer a method-like call.
impl OperationKind {
    /// Convenience wrapper around [`validate_shape`].
    ///
    /// # Errors
    /// Returns a static error description if the operation is structurally
    /// invalid.
    pub fn validate_shape(&self) -> Result<(), &'static str> {
        validate_shape(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::node::RevisionContent;
    use chrono::Utc;
    use insta::assert_json_snapshot;
    use uuid::Uuid;

    fn sample_op(kind: OperationKind) -> Operation {
        Operation {
            op_id: OpId::from_raw(Uuid::nil()),
            workspace_id: WorkspaceId::from_raw(Uuid::nil()),
            device_id: DeviceId::from_raw(Uuid::nil()),
            base_cursor: Cursor::ZERO,
            kind,
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        }
    }

    fn file_revision(node_id: NodeId) -> NodeRevision {
        NodeRevision {
            revision_id: RevisionId::from_raw(Uuid::nil()),
            node_id,
            workspace_id: WorkspaceId::from_raw(Uuid::nil()),
            device_id: DeviceId::from_raw(Uuid::nil()),
            base_revision_id: None,
            content: RevisionContent::File {
                blob_id: BlobId::new("sha256:abcd".to_owned()),
                chunk_ids: vec![],
                content_hash: "sha256:plain".to_owned(),
                encryption_header: Some("v1:nonce".to_owned()),
            },
            posix_mode: 0o644,
            mtime: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            size: 4,
            executable: false,
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        }
    }

    #[test]
    fn create_node_snapshot_and_roundtrip() {
        let op = sample_op(OperationKind::CreateNode {
            parent_id: NodeId::from_raw(Uuid::nil()),
            name: "app".to_owned(),
            kind: NodeKind::Directory,
            initial_revision: None,
        });
        let json = serde_json::to_string(&op).unwrap();
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
        assert_json_snapshot!(json, {
            ".op_id" => "[op_id]",
            ".workspace_id" => "[ws_id]",
            ".device_id" => "[dev_id]",
            ".kind.parent_id" => "[node_id]",
        });
    }

    #[test]
    fn put_file_revision_roundtrip() {
        let node = NodeId::new();
        let op = sample_op(OperationKind::PutFileRevision {
            node_id: node,
            base_revision_id: None,
            revision: file_revision(node),
        });
        let json = serde_json::to_string(&op).unwrap();
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn move_and_delete_roundtrip() {
        let node = NodeId::new();
        let parent = NodeId::new();
        let mv = OperationKind::MoveNode {
            node_id: node,
            old_parent_id: parent,
            old_name: "a".to_owned(),
            new_parent_id: parent,
            new_name: "b".to_owned(),
        };
        let del = OperationKind::DeleteNode {
            node_id: node,
            recursive: false,
        };
        for k in [mv, del] {
            let op = sample_op(k);
            let json = serde_json::to_string(&op).unwrap();
            let back: Operation = serde_json::from_str(&json).unwrap();
            assert_eq!(op, back);
        }
    }

    #[test]
    fn env_ops_roundtrip() {
        let id = Uuid::nil();
        let set = OperationKind::SetEnvVar {
            env_var_id: id,
            encrypted_payload: "envelope".to_owned(),
            metadata: serde_json::json!({"name": "FOO", "scope": "workspace"}),
        };
        let del = OperationKind::DeleteEnvVar { env_var_id: id };
        for k in [set, del] {
            let op = sample_op(k);
            let json = serde_json::to_string(&op).unwrap();
            let back: Operation = serde_json::from_str(&json).unwrap();
            assert_eq!(op, back);
        }
    }

    #[test]
    fn type_tags_are_stable() {
        let node = NodeId::new();
        let cases = [
            (
                OperationKind::CreateNode {
                    parent_id: node,
                    name: "x".to_owned(),
                    kind: NodeKind::File,
                    initial_revision: None,
                },
                "create_node",
            ),
            (
                OperationKind::PutFileRevision {
                    node_id: node,
                    base_revision_id: None,
                    revision: file_revision(node),
                },
                "put_file_revision",
            ),
            (
                OperationKind::MoveNode {
                    node_id: node,
                    old_parent_id: node,
                    old_name: "a".to_owned(),
                    new_parent_id: node,
                    new_name: "b".to_owned(),
                },
                "move_node",
            ),
            (
                OperationKind::DeleteNode {
                    node_id: node,
                    recursive: true,
                },
                "delete_node",
            ),
            (
                OperationKind::RestoreNode {
                    node_id: node,
                    parent_id: node,
                    name: "x".to_owned(),
                },
                "restore_node",
            ),
            (
                OperationKind::SetRule {
                    path_pattern: "node_modules/**".to_owned(),
                    rule: serde_json::json!({"action": "generated"}),
                },
                "set_rule",
            ),
            (
                OperationKind::SetEnvVar {
                    env_var_id: Uuid::nil(),
                    encrypted_payload: "e".to_owned(),
                    metadata: serde_json::json!({}),
                },
                "set_env_var",
            ),
            (
                OperationKind::DeleteEnvVar {
                    env_var_id: Uuid::nil(),
                },
                "delete_env_var",
            ),
        ];
        for (k, tag) in cases {
            assert_eq!(k.type_tag(), tag);
        }
    }

    #[test]
    fn validate_shape_catches_obvious_errors() {
        let node = NodeId::new();

        // empty name
        let bad = OperationKind::CreateNode {
            parent_id: node,
            name: String::new(),
            kind: NodeKind::Directory,
            initial_revision: None,
        };
        assert!(bad.validate_shape().is_err());

        // null byte in name
        let bad = OperationKind::CreateNode {
            parent_id: node,
            name: "a\0b".to_owned(),
            kind: NodeKind::Directory,
            initial_revision: None,
        };
        assert!(bad.validate_shape().is_err());

        // put_file_revision with directory content
        let mut rev = file_revision(node);
        rev.content = RevisionContent::Directory;
        let bad = OperationKind::PutFileRevision {
            node_id: node,
            base_revision_id: None,
            revision: rev,
        };
        assert!(bad.validate_shape().is_err());

        // put_file_revision with mismatched node_id
        let mut rev = file_revision(node);
        rev.node_id = NodeId::new();
        let bad = OperationKind::PutFileRevision {
            node_id: node,
            base_revision_id: None,
            revision: rev,
        };
        assert!(bad.validate_shape().is_err());

        // valid file put
        let good = OperationKind::PutFileRevision {
            node_id: node,
            base_revision_id: None,
            revision: file_revision(node),
        };
        assert!(good.validate_shape().is_ok());

        // empty set_env_var payload
        let bad = OperationKind::SetEnvVar {
            env_var_id: Uuid::nil(),
            encrypted_payload: String::new(),
            metadata: serde_json::json!({}),
        };
        assert!(bad.validate_shape().is_err());
    }

    #[test]
    fn target_node_extraction() {
        let node = NodeId::new();
        let put = OperationKind::PutFileRevision {
            node_id: node,
            base_revision_id: None,
            revision: file_revision(node),
        };
        assert_eq!(put.target_node(), Some(node));

        let create = OperationKind::CreateNode {
            parent_id: node,
            name: "x".to_owned(),
            kind: NodeKind::File,
            initial_revision: None,
        };
        assert_eq!(create.target_node(), None);
    }
}
