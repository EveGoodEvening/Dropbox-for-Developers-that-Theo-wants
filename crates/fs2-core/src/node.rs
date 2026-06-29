//! Node and revision domain types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::id::{DeviceId, NodeId, RevisionId, WorkspaceId};

/// Kind of a node in the workspace tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
}

impl NodeKind {
    /// Returns the string tag used in storage layers.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }

    /// Parses the string tag produced by [`as_str`].
    ///
    /// # Errors
    /// Returns an error if the tag is unknown.
    pub fn from_str_err(s: &str) -> Result<Self, NodeKindParseError> {
        match s {
            "directory" => Ok(Self::Directory),
            "file" => Ok(Self::File),
            "symlink" => Ok(Self::Symlink),
            other => Err(NodeKindParseError(other.to_owned())),
        }
    }
}

/// Error returned by [`NodeKind::from_str_err`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown node kind: {0}")]
pub struct NodeKindParseError(pub String);

/// A node is a file, directory, or symlink in a workspace tree.
///
/// `node_id` is stable across renames and moves. The path is a derived cached
/// field, not a primary key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Stable node identifier.
    pub node_id: NodeId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Parent node, or `None` for the workspace root.
    pub parent_id: Option<NodeId>,
    /// Display name (exact, not normalized).
    pub name: String,
    /// Kind of the node.
    pub kind: NodeKind,
    /// Current revision of this node.
    pub current_rev: RevisionId,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
    /// Tombstone time, set when the node is deleted.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Tombstone version (cursor at which the node was deleted), if any.
    pub tombstone_version: Option<i64>,
}

/// Content identity for a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RevisionContent {
    /// A directory has no content payload.
    Directory,
    /// A regular file references one or more encrypted blobs.
    File {
        /// Content-addressed blob id for a whole-file blob.
        blob_id: String,
        /// Chunk blob ids when the file is split into chunks.
        chunk_ids: Vec<String>,
        /// Plaintext content hash for verification.
        content_hash: String,
        /// Optional encryption header (nonce/tag metadata).
        encryption_header: Option<String>,
    },
    /// A symlink stores its target as a string.
    Symlink {
        /// Target path of the symlink.
        target: String,
    },
}

/// A revision captures metadata and content identity for a node at a point in
/// time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRevision {
    /// Revision identifier.
    pub revision_id: RevisionId,
    /// Node this revision belongs to.
    pub node_id: NodeId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Device that authored this revision.
    pub device_id: DeviceId,
    /// Revision this one is based on, or `None` for the initial revision.
    pub base_revision_id: Option<RevisionId>,
    /// Content identity.
    pub content: RevisionContent,
    /// POSIX mode bits.
    pub posix_mode: u32,
    /// Modification time.
    pub mtime: DateTime<Utc>,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Whether the file is executable.
    pub executable: bool,
    /// Creation time of this revision.
    pub created_at: DateTime<Utc>,
}

impl NodeRevision {
    /// Construct an initial directory revision.
    #[must_use]
    pub fn initial_directory(
        node_id: NodeId,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        mtime: DateTime<Utc>,
    ) -> Self {
        Self {
            revision_id: RevisionId::new(),
            node_id,
            workspace_id,
            device_id,
            base_revision_id: None,
            content: RevisionContent::Directory,
            posix_mode: 0o755,
            mtime,
            size: 0,
            executable: false,
            created_at: mtime,
        }
    }

    /// Construct an initial symlink revision.
    #[must_use]
    pub fn initial_symlink(
        node_id: NodeId,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        target: String,
        mtime: DateTime<Utc>,
    ) -> Self {
        Self {
            revision_id: RevisionId::new(),
            node_id,
            workspace_id,
            device_id,
            base_revision_id: None,
            content: RevisionContent::Symlink { target },
            posix_mode: 0o777,
            mtime,
            size: 0,
            executable: false,
            created_at: mtime,
        }
    }
}

/// Helper to build a [`Node`] for the workspace root.
#[must_use]
pub fn root_node(workspace_id: WorkspaceId, root_revision: RevisionId, now: DateTime<Utc>) -> Node {
    Node {
        node_id: NodeId::from_uuid(Uuid::nil()),
        workspace_id,
        parent_id: None,
        name: String::new(),
        kind: NodeKind::Directory,
        current_rev: root_revision,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        tombstone_version: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_kind_roundtrip() {
        for k in [NodeKind::Directory, NodeKind::File, NodeKind::Symlink] {
            let s = k.as_str();
            assert_eq!(NodeKind::from_str_err(s).unwrap(), k);
        }
        assert!(NodeKind::from_str_err("bogus").is_err());
    }

    #[test]
    fn node_json_roundtrip() {
        let now = Utc::now();
        let n = Node {
            node_id: NodeId::new(),
            workspace_id: WorkspaceId::new(),
            parent_id: None,
            name: "root".to_owned(),
            kind: NodeKind::Directory,
            current_rev: RevisionId::new(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
            tombstone_version: None,
        };
        let json = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn revision_content_file_tag() {
        let c = RevisionContent::File {
            blob_id: "sha256:abc".to_owned(),
            chunk_ids: vec![],
            content_hash: "sha256:def".to_owned(),
            encryption_header: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"kind\":\"file\""));
        let back: RevisionContent = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn revision_content_symlink_tag() {
        let c = RevisionContent::Symlink {
            target: "../x".to_owned(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"kind\":\"symlink\""));
        let back: RevisionContent = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }
}
