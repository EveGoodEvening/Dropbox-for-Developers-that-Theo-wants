//! In-memory metadata store for local development.
//!
//! This implements the same interface as the Postgres-backed store would,
//! using in-memory data structures. It allows the backend to run without
//! Postgres for development and testing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use fs2_core::{
    Cursor, DeviceId, NodeId, NodeKind, NodeRevision, Operation, OperationKind, RevisionContent,
    RevisionId, WorkspaceId,
};
use uuid::Uuid;

use crate::error::{BackendError, BackendResult};

/// A registered device.
#[derive(Debug, Clone)]
pub struct DeviceRecord {
    /// Device id.
    pub id: DeviceId,
    /// Owning user id.
    pub user_id: Uuid,
    /// Device name.
    pub name: String,
    /// Device public key (hex-encoded).
    pub public_key: String,
    /// Whether the device has been revoked.
    pub revoked: bool,
}

/// A workspace record.
#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    /// Workspace id.
    pub id: WorkspaceId,
    /// Owning user id.
    pub user_id: Uuid,
    /// Workspace name.
    pub name: String,
    /// Root node id.
    pub root_node_id: NodeId,
    /// Current cursor.
    pub cursor: Cursor,
}

/// A node record.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    /// Node id.
    pub id: NodeId,
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Parent node id (None for root).
    pub parent_id: Option<NodeId>,
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: NodeKind,
    /// Current revision id.
    pub current_revision_id: Option<RevisionId>,
    /// Whether deleted (tombstoned).
    pub deleted: bool,
}

/// In-memory metadata store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<MemoryStoreInner>,
}

#[derive(Debug, Default)]
struct MemoryStoreInner {
    users: HashMap<Uuid, UserEntry>,
    devices: HashMap<DeviceId, DeviceRecord>,
    workspaces: HashMap<WorkspaceId, WorkspaceRecord>,
    nodes: HashMap<NodeId, NodeRecord>,
    revisions: HashMap<RevisionId, NodeRevision>,
    operations: HashMap<WorkspaceId, Vec<CommittedOp>>,
    /// Idempotency index: (`workspace_id`, `op_id`) -> cursor.
    op_index: HashMap<(WorkspaceId, Uuid), Cursor>,
}

#[derive(Debug)]
struct UserEntry {
    #[allow(dead_code)]
    email: String,
}

/// A committed operation with its assigned cursor.
#[derive(Debug, Clone)]
pub struct CommittedOp {
    /// The operation.
    pub op: Operation,
    /// Assigned cursor.
    pub cursor: Cursor,
}

impl MemoryStore {
    /// Create a new empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a shared (Arc) store.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Create a dev user and return its id.
    pub fn create_dev_user(&self, email: &str) -> BackendResult<Uuid> {
        let mut inner = self.inner.lock().unwrap();
        let id = Uuid::new_v4();
        inner.users.insert(
            id,
            UserEntry {
                email: email.to_owned(),
            },
        );
        Ok(id)
    }

    /// Register a device.
    pub fn register_device(
        &self,
        user_id: Uuid,
        name: &str,
        public_key: &str,
    ) -> BackendResult<DeviceRecord> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.users.contains_key(&user_id) {
            return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::Unauthorized,
                "user not found",
            )));
        }
        let id = DeviceId::new();
        let record = DeviceRecord {
            id,
            user_id,
            name: name.to_owned(),
            public_key: public_key.to_owned(),
            revoked: false,
        };
        inner.devices.insert(id, record.clone());
        Ok(record)
    }

    /// List devices for a user.
    pub fn list_devices(&self, user_id: Uuid) -> BackendResult<Vec<DeviceRecord>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .devices
            .values()
            .filter(|d| d.user_id == user_id)
            .cloned()
            .collect())
    }

    /// Revoke a device.
    pub fn revoke_device(&self, device_id: DeviceId) -> BackendResult<()> {
        let mut inner = self.inner.lock().unwrap();
        let device = inner.devices.get_mut(&device_id).ok_or_else(|| {
            BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::Unauthorized,
                "device not found",
            ))
        })?;
        device.revoked = true;
        Ok(())
    }

    /// Check if a device is valid (exists, not revoked).
    pub fn device_valid(&self, device_id: DeviceId) -> BackendResult<DeviceRecord> {
        let inner = self.inner.lock().unwrap();
        inner
            .devices
            .get(&device_id)
            .cloned()
            .ok_or_else(|| {
                BackendError::Domain(fs2_core::Fs2Error::new(
                    fs2_core::Fs2ErrorCode::Unauthorized,
                    "device not found",
                ))
            })
            .and_then(|d| {
                if d.revoked {
                    Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::DeviceRevoked,
                        "device has been revoked",
                    )))
                } else {
                    Ok(d)
                }
            })
    }

    /// Create a workspace with a root directory node.
    pub fn create_workspace(
        &self,
        user_id: Uuid,
        name: &str,
        device_id: DeviceId,
    ) -> BackendResult<WorkspaceRecord> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.users.contains_key(&user_id) {
            return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::Unauthorized,
                "user not found",
            )));
        }
        let ws_id = WorkspaceId::new();
        let root_node_id = NodeId::new();
        let root_rev_id = RevisionId::new();
        let now = Utc::now();

        // Create root node.
        inner.nodes.insert(
            root_node_id,
            NodeRecord {
                id: root_node_id,
                workspace_id: ws_id,
                parent_id: None,
                name: String::new(),
                kind: NodeKind::Directory,
                current_revision_id: Some(root_rev_id),
                deleted: false,
            },
        );

        // Create initial revision.
        inner.revisions.insert(
            root_rev_id,
            NodeRevision {
                revision_id: root_rev_id,
                node_id: root_node_id,
                workspace_id: ws_id,
                device_id,
                base_revision_id: None,
                content: RevisionContent::Directory,
                posix_mode: 0o755,
                mtime: now,
                size: 0,
                executable: false,
                created_at: now,
            },
        );

        let record = WorkspaceRecord {
            id: ws_id,
            user_id,
            name: name.to_owned(),
            root_node_id,
            cursor: Cursor::zero(),
        };
        inner.workspaces.insert(ws_id, record.clone());
        inner.operations.insert(ws_id, Vec::new());
        Ok(record)
    }

    /// List workspaces for a user.
    pub fn list_workspaces(&self, user_id: Uuid) -> BackendResult<Vec<WorkspaceRecord>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .workspaces
            .values()
            .filter(|w| w.user_id == user_id)
            .cloned()
            .collect())
    }

    /// Get a workspace by id.
    pub fn get_workspace(&self, workspace_id: WorkspaceId) -> BackendResult<WorkspaceRecord> {
        let inner = self.inner.lock().unwrap();
        inner.workspaces.get(&workspace_id).cloned().ok_or_else(|| {
            BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::WorkspaceNotFound,
                "workspace not found",
            ))
        })
    }

    /// Commit an operation. Assigns a cursor, applies the operation, and
    /// returns the committed operation with its cursor.
    ///
    /// Idempotent: if the `op_id` has already been committed for this workspace,
    /// returns the original cursor without re-applying.
    pub fn commit_operation(&self, op: Operation) -> BackendResult<CommittedOp> {
        let mut inner = self.inner.lock().unwrap();
        let ws_id = op.workspace_id;

        // Check workspace exists.
        if !inner.workspaces.contains_key(&ws_id) {
            return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::WorkspaceNotFound,
                "workspace not found",
            )));
        }

        // Idempotency check.
        let op_id_uuid = op.op_id.as_uuid();
        if let Some(&cursor) = inner.op_index.get(&(ws_id, op_id_uuid)) {
            return Ok(CommittedOp { op, cursor });
        }

        // Compute the next cursor but only assign it after successful apply.
        let current_cursor = inner.workspaces.get(&ws_id).unwrap().cursor;
        let new_cursor = current_cursor.next();

        // Apply operation to nodes/revisions.
        Self::apply_operation_inner(&mut inner, &op)?;

        // Only now commit the cursor.
        inner.workspaces.get_mut(&ws_id).unwrap().cursor = new_cursor;

        // Record the operation.
        inner
            .operations
            .entry(ws_id)
            .or_default()
            .push(CommittedOp {
                op: op.clone(),
                cursor: new_cursor,
            });
        inner.op_index.insert((ws_id, op_id_uuid), new_cursor);

        Ok(CommittedOp {
            op,
            cursor: new_cursor,
        })
    }

    fn apply_operation_inner(inner: &mut MemoryStoreInner, op: &Operation) -> BackendResult<()> {
        let ws_id = op.workspace_id;
        match &op.kind {
            OperationKind::CreateNode {
                parent_id,
                name,
                kind,
                initial_revision,
            } => {
                // Validate parent exists and is a directory.
                let parent = inner.nodes.get(parent_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "parent node not found",
                    ))
                })?;
                if parent.workspace_id != ws_id || parent.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "parent node not found in this workspace",
                    )));
                }
                if parent.kind != NodeKind::Directory {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::InvalidOperation,
                        "parent is not a directory",
                    )));
                }
                // Check for sibling collision.
                let collision = inner.nodes.values().any(|n| {
                    n.workspace_id == ws_id
                        && n.parent_id == Some(*parent_id)
                        && n.name == *name
                        && !n.deleted
                });
                if collision {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::PathCollision,
                        format!("a live sibling named `{name}` already exists"),
                    )));
                }
                let node_id = NodeId::new();
                let rev_id = initial_revision
                    .as_ref()
                    .map_or_else(RevisionId::new, |r| r.revision_id);
                inner.nodes.insert(
                    node_id,
                    NodeRecord {
                        id: node_id,
                        workspace_id: ws_id,
                        parent_id: Some(*parent_id),
                        name: name.clone(),
                        kind: *kind,
                        current_revision_id: initial_revision.as_ref().map(|_| rev_id),
                        deleted: false,
                    },
                );
                if let Some(rev) = initial_revision {
                    let mut rev = rev.clone();
                    rev.node_id = node_id;
                    inner.revisions.insert(rev.revision_id, rev);
                }
            }
            OperationKind::PutFileRevision {
                node_id,
                base_revision_id,
                revision,
            } => {
                let node = inner.nodes.get_mut(node_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found",
                    ))
                })?;
                if node.workspace_id != ws_id || node.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found in this workspace",
                    )));
                }
                if node.kind != NodeKind::File {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::InvalidOperation,
                        "node is not a file",
                    )));
                }
                // Check base revision.
                if let Some(base) = base_revision_id {
                    if node.current_revision_id != Some(*base) {
                        return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                            fs2_core::Fs2ErrorCode::RevisionConflict,
                            "base revision does not match current revision",
                        )));
                    }
                }
                node.current_revision_id = Some(revision.revision_id);
                inner
                    .revisions
                    .insert(revision.revision_id, revision.clone());
            }
            OperationKind::MoveNode {
                node_id,
                new_parent_id,
                new_name,
                ..
            } => {
                // Validate node exists and belongs to workspace (immutable read).
                let node = inner.nodes.get(node_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found",
                    ))
                })?;
                if node.workspace_id != ws_id || node.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found in this workspace",
                    )));
                }
                // Validate new parent.
                let parent = inner.nodes.get(new_parent_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "new parent not found",
                    ))
                })?;
                if parent.workspace_id != ws_id || parent.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "new parent not found in this workspace",
                    )));
                }
                if parent.kind != NodeKind::Directory {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::InvalidOperation,
                        "new parent is not a directory",
                    )));
                }
                // Check for cycle: walk up the ancestor chain of new_parent_id
                // and reject if node_id is encountered.
                let mut ancestor = Some(*new_parent_id);
                while let Some(aid) = ancestor {
                    if aid == *node_id {
                        return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                            fs2_core::Fs2ErrorCode::InvalidOperation,
                            "cannot move a node into its own descendant",
                        )));
                    }
                    ancestor = inner.nodes.get(&aid).and_then(|n| n.parent_id);
                }
                // Check collision.
                let collision = inner.nodes.values().any(|n| {
                    n.workspace_id == ws_id
                        && n.parent_id == Some(*new_parent_id)
                        && n.name == *new_name
                        && n.id != *node_id
                        && !n.deleted
                });
                if collision {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::PathCollision,
                        format!("a live sibling named `{new_name}` already exists"),
                    )));
                }
                // All validation passed; apply the mutation.
                let node = inner.nodes.get_mut(node_id).unwrap();
                node.parent_id = Some(*new_parent_id);
                node.name.clone_from(new_name);
            }
            OperationKind::DeleteNode { node_id, recursive } => {
                // Validate node exists (immutable read).
                let node = inner.nodes.get(node_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found",
                    ))
                })?;
                if node.workspace_id != ws_id || node.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found in this workspace",
                    )));
                }
                // Check for children if not recursive.
                let has_children = inner
                    .nodes
                    .values()
                    .any(|n| n.parent_id == Some(*node_id) && !n.deleted);
                if has_children && !recursive {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::InvalidOperation,
                        "directory is not empty (use recursive delete)",
                    )));
                }
                // Collect descendant ids for recursive delete.
                let descendants: Vec<NodeId> = if *recursive {
                    let mut all = Vec::new();
                    let mut queue = vec![*node_id];
                    while let Some(current) = queue.pop() {
                        let children: Vec<NodeId> = inner
                            .nodes
                            .values()
                            .filter(|n| n.parent_id == Some(current) && !n.deleted)
                            .map(|n| n.id)
                            .collect();
                        for child in children {
                            all.push(child);
                            queue.push(child);
                        }
                    }
                    all
                } else {
                    Vec::new()
                };
                // Apply mutations.
                inner.nodes.get_mut(node_id).unwrap().deleted = true;
                for desc in descendants {
                    if let Some(n) = inner.nodes.get_mut(&desc) {
                        n.deleted = true;
                    }
                }
            }
            OperationKind::RestoreNode {
                node_id,
                parent_id,
                name,
            } => {
                let node = inner.nodes.get(node_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found",
                    ))
                })?;
                if node.workspace_id != ws_id {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "node not found in this workspace",
                    )));
                }
                if !node.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::InvalidOperation,
                        "node is not deleted",
                    )));
                }
                // Validate parent.
                let parent = inner.nodes.get(parent_id).ok_or_else(|| {
                    BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "parent not found",
                    ))
                })?;
                if parent.workspace_id != ws_id || parent.deleted {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::NodeNotFound,
                        "parent not found in this workspace",
                    )));
                }
                // Check collision.
                let collision = inner.nodes.values().any(|n| {
                    n.workspace_id == ws_id
                        && n.parent_id == Some(*parent_id)
                        && n.name == *name
                        && n.id != *node_id
                        && !n.deleted
                });
                if collision {
                    return Err(BackendError::Domain(fs2_core::Fs2Error::new(
                        fs2_core::Fs2ErrorCode::PathCollision,
                        format!("a live sibling named `{name}` already exists"),
                    )));
                }
                // Apply mutation.
                let node = inner.nodes.get_mut(node_id).unwrap();
                node.deleted = false;
                node.parent_id = Some(*parent_id);
                node.name.clone_from(name);
            }
            OperationKind::SetRule { .. }
            | OperationKind::SetEnvVar { .. }
            | OperationKind::DeleteEnvVar { .. } => {
                // Rule and env operations are stored in the operation log
                // but do not modify the node tree. Full env/rule storage is
                // implemented in later phases.
            }
        }
        Ok(())
    }

    /// Fetch operations since a cursor, up to a limit.
    pub fn fetch_operations(
        &self,
        workspace_id: WorkspaceId,
        since: Cursor,
        limit: usize,
    ) -> BackendResult<(Vec<CommittedOp>, bool, Cursor)> {
        let inner = self.inner.lock().unwrap();
        let ops = inner.operations.get(&workspace_id).ok_or_else(|| {
            BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::WorkspaceNotFound,
                "workspace not found",
            ))
        })?;
        let filtered: Vec<&CommittedOp> = ops
            .iter()
            .filter(|op| op.cursor > since)
            .take(limit + 1)
            .collect();
        let has_more = filtered.len() > limit;
        let page: Vec<CommittedOp> = filtered.into_iter().take(limit).cloned().collect();
        let next_cursor = page.last().map_or(since, |o| o.cursor);
        Ok((page, has_more, next_cursor))
    }

    /// Get a node by id.
    pub fn get_node(&self, node_id: NodeId) -> BackendResult<NodeRecord> {
        let inner = self.inner.lock().unwrap();
        inner.nodes.get(&node_id).cloned().ok_or_else(|| {
            BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::NodeNotFound,
                "node not found",
            ))
        })
    }

    /// List children of a directory node.
    pub fn list_children(&self, parent_id: NodeId) -> BackendResult<Vec<NodeRecord>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .nodes
            .values()
            .filter(|n| n.parent_id == Some(parent_id) && !n.deleted)
            .cloned()
            .collect())
    }

    /// Get a revision by id.
    pub fn get_revision(&self, revision_id: RevisionId) -> BackendResult<NodeRevision> {
        let inner = self.inner.lock().unwrap();
        inner.revisions.get(&revision_id).cloned().ok_or_else(|| {
            BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::NodeNotFound,
                "revision not found",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_workspace_has_root_node() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        assert_eq!(ws.cursor, Cursor::zero());
        let root = store.get_node(ws.root_node_id).unwrap();
        assert_eq!(root.kind, NodeKind::Directory);
        assert!(root.parent_id.is_none());
        assert!(!root.deleted);
    }

    #[test]
    fn commit_operation_increments_cursor() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        let op = Operation::new(
            ws.id,
            device.id,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: ws.root_node_id,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        let committed = store.commit_operation(op.clone()).unwrap();
        assert_eq!(committed.cursor, Cursor::from(1));
        let ws_after = store.get_workspace(ws.id).unwrap();
        assert_eq!(ws_after.cursor, Cursor::from(1));
    }

    #[test]
    fn duplicate_op_is_idempotent() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        let op = Operation::new(
            ws.id,
            device.id,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: ws.root_node_id,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        let first = store.commit_operation(op.clone()).unwrap();
        let second = store.commit_operation(op).unwrap();
        assert_eq!(first.cursor, second.cursor);
        // Only one node should exist.
        let children = store.list_children(ws.root_node_id).unwrap();
        assert_eq!(children.len(), 1);
    }

    #[test]
    fn path_collision_rejected() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        let op1 = Operation::new(
            ws.id,
            device.id,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: ws.root_node_id,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        store.commit_operation(op1).unwrap();
        let op2 = Operation::new(
            ws.id,
            device.id,
            Cursor::from(1),
            OperationKind::CreateNode {
                parent_id: ws.root_node_id,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        let err = store.commit_operation(op2).unwrap_err();
        assert!(err.to_string().contains("path_collision"));
    }

    #[test]
    fn revision_conflict_rejected() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        // Create a file node.
        let rev1 = RevisionId::new();
        let create_op = Operation::new(
            ws.id,
            device.id,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: ws.root_node_id,
                name: "file.txt".to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(NodeRevision {
                    revision_id: rev1,
                    node_id: NodeId::new(), // store will assign its own
                    workspace_id: ws.id,
                    device_id: device.id,
                    base_revision_id: None,
                    content: RevisionContent::File {
                        blob_id: "sha256:abc".to_owned(),
                        chunk_ids: vec![],
                        content_hash: "sha256:def".to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: Utc::now(),
                    size: 10,
                    executable: false,
                    created_at: Utc::now(),
                }),
            },
            Utc::now(),
        );
        store.commit_operation(create_op).unwrap();
        // Look up the actual node id and current revision.
        let children = store.list_children(ws.root_node_id).unwrap();
        let file_node = &children[0];
        let actual_node_id = file_node.id;
        let actual_rev_id = file_node.current_revision_id.unwrap();
        // Try to put a revision with wrong base.
        let rev2 = RevisionId::new();
        let wrong_base = RevisionId::new(); // different from actual_rev_id
        let put_op = Operation::new(
            ws.id,
            device.id,
            Cursor::from(1),
            OperationKind::PutFileRevision {
                node_id: actual_node_id,
                base_revision_id: Some(wrong_base),
                revision: NodeRevision {
                    revision_id: rev2,
                    node_id: actual_node_id,
                    workspace_id: ws.id,
                    device_id: device.id,
                    base_revision_id: Some(wrong_base),
                    content: RevisionContent::File {
                        blob_id: "sha256:xyz".to_owned(),
                        chunk_ids: vec![],
                        content_hash: "sha256:ghi".to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: Utc::now(),
                    size: 20,
                    executable: false,
                    created_at: Utc::now(),
                },
            },
            Utc::now(),
        );
        let err = store.commit_operation(put_op).unwrap_err();
        assert!(
            err.to_string().contains("revision_conflict"),
            "expected revision_conflict, got: {err}"
        );
        // Verify that using the correct base succeeds.
        let rev3 = RevisionId::new();
        let put_ok = Operation::new(
            ws.id,
            device.id,
            Cursor::from(1),
            OperationKind::PutFileRevision {
                node_id: actual_node_id,
                base_revision_id: Some(actual_rev_id),
                revision: NodeRevision {
                    revision_id: rev3,
                    node_id: actual_node_id,
                    workspace_id: ws.id,
                    device_id: device.id,
                    base_revision_id: Some(actual_rev_id),
                    content: RevisionContent::File {
                        blob_id: "sha256:xyz".to_owned(),
                        chunk_ids: vec![],
                        content_hash: "sha256:ghi".to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: Utc::now(),
                    size: 20,
                    executable: false,
                    created_at: Utc::now(),
                },
            },
            Utc::now(),
        );
        store.commit_operation(put_ok).unwrap();
    }

    #[test]
    fn fetch_operations_pagination() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        // Create 3 directories.
        for i in 0..3 {
            let op = Operation::new(
                ws.id,
                device.id,
                Cursor::from(i),
                OperationKind::CreateNode {
                    parent_id: ws.root_node_id,
                    name: format!("dir{i}"),
                    kind: NodeKind::Directory,
                    initial_revision: None,
                },
                Utc::now(),
            );
            store.commit_operation(op).unwrap();
        }
        // Fetch with limit 2.
        let (page1, has_more, next) = store.fetch_operations(ws.id, Cursor::zero(), 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert!(has_more);
        assert_eq!(next, Cursor::from(2));
        let (page2, has_more2, _) = store.fetch_operations(ws.id, next, 2).unwrap();
        assert_eq!(page2.len(), 1);
        assert!(!has_more2);
    }

    #[test]
    fn revoked_device_rejected() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        store.revoke_device(device.id).unwrap();
        let err = store.device_valid(device.id).unwrap_err();
        assert!(err.to_string().contains("device_revoked"));
    }

    #[test]
    fn delete_non_empty_dir_requires_recursive() {
        let store = MemoryStore::new();
        let user_id = store.create_dev_user("test@example.com").unwrap();
        let device = store
            .register_device(user_id, "test-device", "fake-key")
            .unwrap();
        let ws = store
            .create_workspace(user_id, "test-ws", device.id)
            .unwrap();
        // Create parent dir.
        let parent_op = Operation::new(
            ws.id,
            device.id,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: ws.root_node_id,
                name: "parent".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        let committed = store.commit_operation(parent_op).unwrap();
        // Find the parent node id.
        let children = store.list_children(ws.root_node_id).unwrap();
        let parent_id = children[0].id;
        // Create a child.
        let child_op = Operation::new(
            ws.id,
            device.id,
            committed.cursor,
            OperationKind::CreateNode {
                parent_id,
                name: "child".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        store.commit_operation(child_op).unwrap();
        // Try to delete parent without recursive.
        let del_op = Operation::new(
            ws.id,
            device.id,
            Cursor::from(2),
            OperationKind::DeleteNode {
                node_id: parent_id,
                recursive: false,
            },
            Utc::now(),
        );
        let err = store.commit_operation(del_op).unwrap_err();
        assert!(err.to_string().contains("not empty"));
        // Delete with recursive.
        let del_op2 = Operation::new(
            ws.id,
            device.id,
            Cursor::from(2),
            OperationKind::DeleteNode {
                node_id: parent_id,
                recursive: true,
            },
            Utc::now(),
        );
        store.commit_operation(del_op2).unwrap();
        let node = store.get_node(parent_id).unwrap();
        assert!(node.deleted);
    }
}
