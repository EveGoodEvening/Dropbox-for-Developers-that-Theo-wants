//! Node and revision types — the immutable metadata describing the workspace tree.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{BlobId, DeviceId, NodeId, RevisionId, WorkspaceId};

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
    /// Stable string tag used in DB columns and JSON `kind` fields.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Directory => "directory",
            NodeKind::File => "file",
            NodeKind::Symlink => "symlink",
        }
    }

    /// Inverse of [`as_str`].
    ///
    /// Returns `None` on unknown tags so callers can produce a structured
    /// `invalid_operation` error instead of panicking on persisted data.
    #[must_use]
    pub fn parse_tag(s: &str) -> Option<Self> {
        match s {
            "directory" => Some(Self::Directory),
            "file" => Some(Self::File),
            "symlink" => Some(Self::Symlink),
            _ => None,
        }
    }
}

/// A node is a file, directory, or symlink in a workspace tree.
///
/// `node_id` is stable across renames and moves. The path is derived from
/// `parent_id + name`, not stored as authoritative identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Stable identifier for this node.
    pub node_id: NodeId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Parent node, or `None` for the workspace root.
    pub parent_id: Option<NodeId>,
    /// Displayed filename. A separate normalized key handles collision checks.
    pub name: String,
    /// Kind of the node.
    pub kind: NodeKind,
    /// Current revision of this node's metadata/content.
    pub current_rev: RevisionId,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
    /// Tombstone time. `Some` means the node is logically deleted.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Version of the tombstone, used to converge offline deletes.
    pub tombstone_version: Option<i64>,
}

/// Content/metadata snapshot of a node at a point in time.
///
/// Revisions are immutable once committed.
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
    /// Revision this one is based on. `None` for the initial revision.
    pub base_revision_id: Option<RevisionId>,
    /// Content payload, depending on node kind.
    pub content: RevisionContent,
    /// Portable POSIX mode bits (e.g. `0o644`).
    pub posix_mode: u32,
    /// Modification time preserved across machines.
    pub mtime: DateTime<Utc>,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Whether the executable bit is set.
    pub executable: bool,
    /// When this revision was created.
    pub created_at: DateTime<Utc>,
}

/// Kind-specific content of a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RevisionContent {
    /// A directory has no content payload.
    Directory,
    /// A regular file points at one or more encrypted blobs.
    File {
        /// Content-addressed blob holding the (encrypted) file bytes.
        blob_id: BlobId,
        /// Optional chunk IDs for chunked large files.
        chunk_ids: Vec<BlobId>,
        /// Hash of the plaintext content, for verification.
        content_hash: String,
        /// Versioned encryption header (nonce + algorithm tag).
        encryption_header: Option<String>,
    },
    /// A symlink stores its target string.
    Symlink {
        /// Target path string. May be relative or absolute.
        target: String,
    },
}

impl RevisionContent {
    /// Returns `true` if this content describes a directory.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Returns `true` if this content describes a regular file.
    #[must_use]
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }

    /// Returns `true` if this content describes a symlink.
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        matches!(self, Self::Symlink { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use chrono::Utc;
    use insta::assert_json_snapshot;
    use uuid::Uuid;

    fn sample_node() -> Node {
        Node {
            node_id: NodeId::from_raw(Uuid::nil()),
            workspace_id: WorkspaceId::from_raw(Uuid::nil()),
            parent_id: None,
            name: "root".to_owned(),
            kind: NodeKind::Directory,
            current_rev: RevisionId::from_raw(Uuid::nil()),
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            updated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            deleted_at: None,
            tombstone_version: None,
        }
    }

    fn sample_file_revision() -> NodeRevision {
        NodeRevision {
            revision_id: RevisionId::from_raw(Uuid::nil()),
            node_id: NodeId::from_raw(Uuid::nil()),
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
            size: 42,
            executable: false,
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        }
    }

    #[test]
    fn node_kind_roundtrip() {
        for k in [NodeKind::Directory, NodeKind::File, NodeKind::Symlink] {
            assert_eq!(NodeKind::parse_tag(k.as_str()), Some(k));
        }
        assert_eq!(NodeKind::parse_tag("nope"), None);
    }

    #[test]
    fn node_serializes_with_expected_shape() {
        let node = sample_node();
        let json = serde_json::to_string(&node).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(node, back);
        assert_json_snapshot!(json, {
            ".node_id" => "[node_id]",
            ".workspace_id" => "[ws_id]",
            ".current_rev" => "[rev_id]",
        });
    }

    #[test]
    fn file_revision_roundtrip() {
        let rev = sample_file_revision();
        let json = serde_json::to_string(&rev).unwrap();
        let back: NodeRevision = serde_json::from_str(&json).unwrap();
        assert_eq!(rev, back);
    }

    #[test]
    fn symlink_revision_roundtrip() {
        let rev = NodeRevision {
            content: RevisionContent::Symlink {
                target: "../other".to_owned(),
            },
            ..sample_file_revision()
        };
        let json = serde_json::to_string(&rev).unwrap();
        let back: NodeRevision = serde_json::from_str(&json).unwrap();
        assert_eq!(rev, back);
        assert!(back.content.is_symlink());
    }

    #[test]
    fn revision_content_tagged_serialization() {
        let dir = NodeRevision {
            content: RevisionContent::Directory,
            ..sample_file_revision()
        };
        let json = serde_json::to_string(&dir.content).unwrap();
        assert!(json.contains("\"kind\":\"directory\""));
    }
}
