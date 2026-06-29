//! In-memory metadata store for development and testing.
//!
//! The MVP uses Postgres for authoritative metadata. This in-memory store
//! implements the same operations so the full API surface can be tested
//! without external dependencies. A Postgres adapter will be added later.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Utc;
use fs2_core::ids::UserId;
use fs2_core::node::{Node, NodeKind, NodeRevision, RevisionContent};
use fs2_core::{Cursor, DeviceId, NodeId, Operation, OperationKind, RevisionId, WorkspaceId};

/// In-memory metadata store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<MemoryStoreInner>,
}

#[derive(Debug, Default)]
struct MemoryStoreInner {
    users: HashMap<UserId, UserRecord>,
    devices: HashMap<DeviceId, DeviceRecord>,
    workspaces: HashMap<WorkspaceId, WorkspaceRecord>,
    nodes: HashMap<NodeId, Node>,
    revisions: HashMap<RevisionId, NodeRevision>,
    operations: HashMap<WorkspaceId, Vec<OperationRow>>,
    /// Dedup index: (`workspace_id`, `op_id`) -> cursor.
    op_dedup: HashMap<(WorkspaceId, uuid::Uuid), Cursor>,
}

#[derive(Debug, Clone)]
struct UserRecord {
    #[allow(dead_code)]
    email: String,
}

#[derive(Debug, Clone)]
struct DeviceRecord {
    user_id: UserId,
    name: String,
    #[allow(dead_code)]
    public_key: Vec<u8>,
    platform: serde_json::Value,
    revoked_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct WorkspaceRecord {
    user_id: UserId,
    name: String,
    root_node_id: NodeId,
    current_cursor: Cursor,
}

#[derive(Debug, Clone)]
struct OperationRow {
    cursor: Cursor,
    op: Operation,
}

/// Error from store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Entity not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Invalid operation.
    #[error("invalid operation: {0}")]
    Invalid(String),
    /// Path collision.
    #[error("path collision: {0}")]
    Collision(String),
    /// Revision conflict.
    #[error("revision conflict: {0}")]
    Conflict(String),
}

impl StoreError {
    /// Convert to an [`fs2_core::Fs2Error`].
    #[must_use]
    pub fn to_fs2_error(&self) -> fs2_core::Fs2Error {
        match self {
            Self::NotFound(msg) => match msg.as_str() {
                s if s.starts_with("workspace") => {
                    fs2_core::Fs2Error::WorkspaceNotFound(msg.clone())
                }
                s if s.starts_with("node") => fs2_core::Fs2Error::NodeNotFound(msg.clone()),
                _ => fs2_core::Fs2Error::NodeNotFound(msg.clone()),
            },
            Self::Invalid(msg) => fs2_core::Fs2Error::InvalidOperation(msg.clone()),
            Self::Collision(msg) => fs2_core::Fs2Error::PathCollision(msg.clone()),
            Self::Conflict(msg) => fs2_core::Fs2Error::RevisionConflict(msg.clone()),
        }
    }
}

impl MemoryStore {
    /// Create a new empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap in an [`Arc`] for sharing across handlers.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Whether this is the in-memory store (always true for `MemoryStore`).
    #[must_use]
    pub fn is_memory(&self) -> bool {
        true
    }

    // -- Users ---------------------------------------------------------------

    /// Create a dev-only user.
    pub fn create_user(&self, email: &str) -> Result<UserId, StoreError> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        let id = UserId::new();
        inner.users.insert(
            id,
            UserRecord {
                email: email.to_owned(),
            },
        );
        Ok(id)
    }

    /// Check if a user exists.
    pub fn user_exists(&self, user_id: &UserId) -> bool {
        self.inner
            .lock()
            .expect("mutex poisoned")
            .users
            .contains_key(user_id)
    }

    // -- Devices -------------------------------------------------------------

    /// Register a device.
    pub fn register_device(
        &self,
        user_id: UserId,
        name: &str,
        public_key: Vec<u8>,
        platform: serde_json::Value,
    ) -> Result<DeviceId, StoreError> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        if !inner.users.contains_key(&user_id) {
            return Err(StoreError::NotFound(format!("user {user_id}")));
        }
        let id = DeviceId::new();
        inner.devices.insert(
            id,
            DeviceRecord {
                user_id,
                name: name.to_owned(),
                public_key,
                platform,
                revoked_at: None,
            },
        );
        Ok(id)
    }

    /// List devices for a user.
    pub fn list_devices(&self, user_id: UserId) -> Vec<DeviceInfo> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner
            .devices
            .iter()
            .filter(|(_, d)| d.user_id == user_id)
            .map(|(id, d)| DeviceInfo {
                device_id: *id,
                name: d.name.clone(),
                platform: d.platform.clone(),
                revoked_at: d.revoked_at,
            })
            .collect()
    }

    /// Revoke a device.
    pub fn revoke_device(&self, device_id: DeviceId) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        let dev = inner
            .devices
            .get_mut(&device_id)
            .ok_or_else(|| StoreError::NotFound(format!("device {device_id}")))?;
        dev.revoked_at = Some(Utc::now());
        Ok(())
    }

    /// Check if a device is revoked.
    pub fn is_device_revoked(&self, device_id: DeviceId) -> bool {
        self.inner
            .lock()
            .expect("mutex poisoned")
            .devices
            .get(&device_id)
            .is_some_and(|d| d.revoked_at.is_some())
    }

    /// Get the user id for a device.
    pub fn device_user(&self, device_id: DeviceId) -> Option<UserId> {
        self.inner
            .lock()
            .expect("mutex poisoned")
            .devices
            .get(&device_id)
            .map(|d| d.user_id)
    }

    // -- Workspaces ----------------------------------------------------------

    /// Create a workspace with a root directory node.
    pub fn create_workspace(
        &self,
        user_id: UserId,
        name: &str,
        device_id: DeviceId,
    ) -> Result<(WorkspaceId, NodeId), StoreError> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        if !inner.users.contains_key(&user_id) {
            return Err(StoreError::NotFound(format!("user {user_id}")));
        }
        if !inner.devices.contains_key(&device_id) {
            return Err(StoreError::NotFound(format!("device {device_id}")));
        }

        let ws_id = WorkspaceId::new();
        let root_node_id = NodeId::new();
        let root_rev_id = RevisionId::new();
        let now = Utc::now();

        // Create root node.
        let root_node = Node {
            node_id: root_node_id,
            workspace_id: ws_id,
            parent_id: None,
            name: String::new(),
            kind: NodeKind::Directory,
            current_rev: root_rev_id,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            tombstone_version: None,
        };
        let root_rev = NodeRevision {
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
        };
        inner.nodes.insert(root_node_id, root_node);
        inner.revisions.insert(root_rev_id, root_rev);

        inner.workspaces.insert(
            ws_id,
            WorkspaceRecord {
                user_id,
                name: name.to_owned(),
                root_node_id,
                current_cursor: Cursor::ZERO,
            },
        );
        inner.operations.insert(ws_id, Vec::new());
        Ok((ws_id, root_node_id))
    }

    /// Get workspace root node id.
    pub fn workspace_root(&self, ws_id: WorkspaceId) -> Result<NodeId, StoreError> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner
            .workspaces
            .get(&ws_id)
            .map(|w| w.root_node_id)
            .ok_or_else(|| StoreError::NotFound(format!("workspace {ws_id}")))
    }

    /// List workspaces for a user.
    pub fn list_workspaces(&self, user_id: UserId) -> Vec<WorkspaceInfo> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner
            .workspaces
            .iter()
            .filter(|(_, w)| w.user_id == user_id)
            .map(|(id, w)| WorkspaceInfo {
                workspace_id: *id,
                name: w.name.clone(),
                root_node_id: w.root_node_id,
                current_cursor: w.current_cursor,
            })
            .collect()
    }

    // -- Operations ----------------------------------------------------------

    /// Commit an operation idempotently.
    ///
    /// If the `op_id` was already committed, returns the original cursor
    /// without re-applying. Otherwise validates, assigns a cursor, applies,
    /// and stores the operation.
    pub fn commit_operation(&self, op: Operation) -> Result<Cursor, StoreError> {
        let mut inner = self.inner.lock().expect("mutex poisoned");

        // Idempotency check.
        if let Some(&cursor) = inner.op_dedup.get(&(op.workspace_id, *op.op_id.as_raw())) {
            return Ok(cursor);
        }

        // Validate workspace exists.
        let ws = inner
            .workspaces
            .get(&op.workspace_id)
            .ok_or_else(|| StoreError::NotFound(format!("workspace {}", op.workspace_id)))?
            .clone();

        // Validate device belongs to workspace owner and is not revoked.
        let dev = inner
            .devices
            .get(&op.device_id)
            .ok_or_else(|| StoreError::NotFound(format!("device {}", op.device_id)))?;
        if dev.user_id != ws.user_id {
            return Err(StoreError::Invalid(format!(
                "device {} does not belong to workspace {} owner",
                op.device_id, op.workspace_id
            )));
        }
        if dev.revoked_at.is_some() {
            return Err(StoreError::Invalid(format!(
                "device {} is revoked",
                op.device_id
            )));
        }

        // Validate operation shape.
        fs2_core::op::validate_shape(&op.kind).map_err(|e| StoreError::Invalid(e.to_owned()))?;

        // Apply operation to nodes/revisions.
        Self::apply_operation_inner(&mut inner, &op)?;

        // Assign cursor and store.
        let cursor = ws.current_cursor.next();
        inner
            .workspaces
            .get_mut(&op.workspace_id)
            .expect("workspace exists")
            .current_cursor = cursor;
        inner
            .op_dedup
            .insert((op.workspace_id, *op.op_id.as_raw()), cursor);
        inner
            .operations
            .entry(op.workspace_id)
            .or_default()
            .push(OperationRow { cursor, op });
        Ok(cursor)
    }

    fn apply_operation_inner(
        inner: &mut MemoryStoreInner,
        op: &Operation,
    ) -> Result<(), StoreError> {
        let now = Utc::now();
        match &op.kind {
            OperationKind::CreateNode {
                parent_id,
                name,
                kind,
                initial_revision,
            } => {
                // Validate parent exists and is a live directory.
                let parent = inner
                    .nodes
                    .get(parent_id)
                    .ok_or_else(|| StoreError::NotFound(format!("node {parent_id}")))?;
                if parent.deleted_at.is_some() {
                    return Err(StoreError::Invalid(format!(
                        "parent {parent_id} is deleted"
                    )));
                }
                if parent.kind != NodeKind::Directory {
                    return Err(StoreError::Invalid(format!(
                        "parent {parent_id} is not a directory"
                    )));
                }
                // Check for live sibling collision.
                let collision = inner.nodes.values().any(|n| {
                    n.workspace_id == op.workspace_id
                        && n.parent_id == Some(*parent_id)
                        && n.name == *name
                        && n.deleted_at.is_none()
                });
                if collision {
                    return Err(StoreError::Collision(format!(
                        "sibling '{name}' already exists under {parent_id}"
                    )));
                }

                let node_id = NodeId::new();
                let rev_id = initial_revision
                    .as_ref()
                    .map_or_else(RevisionId::new, |r| r.revision_id);

                let node = Node {
                    node_id,
                    workspace_id: op.workspace_id,
                    parent_id: Some(*parent_id),
                    name: name.clone(),
                    kind: *kind,
                    current_rev: rev_id,
                    created_at: now,
                    updated_at: now,
                    deleted_at: None,
                    tombstone_version: None,
                };
                inner.nodes.insert(node_id, node);

                if let Some(rev) = initial_revision {
                    inner.revisions.insert(rev.revision_id, rev.clone());
                } else {
                    // Create a default empty revision.
                    let rev = NodeRevision {
                        revision_id: rev_id,
                        node_id,
                        workspace_id: op.workspace_id,
                        device_id: op.device_id,
                        base_revision_id: None,
                        content: match kind {
                            NodeKind::Directory => RevisionContent::Directory,
                            NodeKind::Symlink => RevisionContent::Symlink {
                                target: String::new(),
                            },
                            NodeKind::File => RevisionContent::File {
                                blob_id: fs2_core::BlobId::new("sha256:empty".to_owned()),
                                chunk_ids: vec![],
                                content_hash: String::new(),
                                encryption_header: None,
                            },
                        },
                        posix_mode: 0o644,
                        mtime: now,
                        size: 0,
                        executable: false,
                        created_at: now,
                    };
                    inner.revisions.insert(rev_id, rev);
                }
            }
            OperationKind::PutFileRevision {
                node_id,
                base_revision_id,
                revision,
            } => {
                let node = inner
                    .nodes
                    .get_mut(node_id)
                    .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
                if node.deleted_at.is_some() {
                    return Err(StoreError::Invalid(format!("node {node_id} is deleted")));
                }
                // Check base revision.
                if let Some(base) = base_revision_id {
                    if node.current_rev != *base {
                        return Err(StoreError::Conflict(format!(
                            "base revision mismatch: expected {base}, got {}",
                            node.current_rev
                        )));
                    }
                }
                node.current_rev = revision.revision_id;
                node.updated_at = now;
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
                let node = inner
                    .nodes
                    .get(node_id)
                    .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
                if node.deleted_at.is_some() {
                    return Err(StoreError::Invalid(format!("node {node_id} is deleted")));
                }
                // Check collision in new parent.
                let collision = inner.nodes.values().any(|n| {
                    n.workspace_id == op.workspace_id
                        && n.parent_id == Some(*new_parent_id)
                        && n.name == *new_name
                        && n.node_id != *node_id
                        && n.deleted_at.is_none()
                });
                if collision {
                    return Err(StoreError::Collision(format!(
                        "sibling '{new_name}' already exists under {new_parent_id}"
                    )));
                }
                let node = inner.nodes.get_mut(node_id).unwrap();
                node.parent_id = Some(*new_parent_id);
                node.name.clone_from(new_name);
                node.updated_at = now;
            }
            OperationKind::DeleteNode { node_id, recursive } => {
                let has_live_children = inner.nodes.values().any(|n| {
                    n.workspace_id == op.workspace_id
                        && n.parent_id == Some(*node_id)
                        && n.deleted_at.is_none()
                });
                if has_live_children && !recursive {
                    return Err(StoreError::Invalid(format!(
                        "node {node_id} has children and recursive=false"
                    )));
                }
                // Tombstone the node and all descendants if recursive.
                let to_tombstone: Vec<NodeId> = if *recursive {
                    Self::collect_descendants(inner, op.workspace_id, *node_id)
                } else {
                    vec![*node_id]
                };
                for nid in to_tombstone {
                    if let Some(n) = inner.nodes.get_mut(&nid) {
                        n.deleted_at = Some(now);
                        n.updated_at = now;
                    }
                }
            }
            OperationKind::RestoreNode {
                node_id,
                parent_id,
                name,
            } => {
                let node = inner
                    .nodes
                    .get(node_id)
                    .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
                if node.deleted_at.is_none() {
                    return Err(StoreError::Invalid(format!(
                        "node {node_id} is not deleted"
                    )));
                }
                // Check collision.
                let collision = inner.nodes.values().any(|n| {
                    n.workspace_id == op.workspace_id
                        && n.parent_id == Some(*parent_id)
                        && n.name == *name
                        && n.node_id != *node_id
                        && n.deleted_at.is_none()
                });
                if collision {
                    return Err(StoreError::Collision(format!(
                        "sibling '{name}' already exists under {parent_id}"
                    )));
                }
                let node = inner.nodes.get_mut(node_id).unwrap();
                node.deleted_at = None;
                node.parent_id = Some(*parent_id);
                node.name.clone_from(name);
                node.updated_at = now;
            }
            OperationKind::SetRule { .. }
            | OperationKind::SetEnvVar { .. }
            | OperationKind::DeleteEnvVar { .. } => {
                // Rule and env ops don't modify nodes; they're stored in the
                // operation log for clients to replay. Full rule/env storage
                // lands in a later phase.
            }
        }
        Ok(())
    }

    fn collect_descendants(
        inner: &MemoryStoreInner,
        ws_id: WorkspaceId,
        node_id: NodeId,
    ) -> Vec<NodeId> {
        let mut result = vec![node_id];
        let mut stack = vec![node_id];
        while let Some(current) = stack.pop() {
            for n in inner.nodes.values() {
                if n.workspace_id == ws_id && n.parent_id == Some(current) {
                    result.push(n.node_id);
                    stack.push(n.node_id);
                }
            }
        }
        result
    }

    /// Fetch operations since a cursor, up to `limit`.
    pub fn fetch_operations(
        &self,
        ws_id: WorkspaceId,
        since: Cursor,
        limit: usize,
    ) -> Result<(Vec<Operation>, bool), StoreError> {
        let inner = self.inner.lock().expect("mutex poisoned");
        if !inner.workspaces.contains_key(&ws_id) {
            return Err(StoreError::NotFound(format!("workspace {ws_id}")));
        }
        let ops = inner.operations.get(&ws_id);
        let matching: Vec<&OperationRow> = ops
            .map(|v| v.iter().filter(|r| r.cursor > since).collect())
            .unwrap_or_default();
        let has_more = matching.len() > limit;
        let result: Vec<Operation> = matching
            .into_iter()
            .take(limit)
            .map(|r| r.op.clone())
            .collect();
        Ok((result, has_more))
    }

    /// Get a node by id.
    pub fn get_node(&self, node_id: NodeId) -> Result<Node, StoreError> {
        self.inner
            .lock()
            .expect("mutex poisoned")
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))
    }

    /// List live children of a directory node.
    pub fn list_children(&self, parent_id: NodeId) -> Vec<Node> {
        self.inner
            .lock()
            .expect("mutex poisoned")
            .nodes
            .values()
            .filter(|n| n.parent_id == Some(parent_id) && n.deleted_at.is_none())
            .cloned()
            .collect()
    }

    /// Get a revision by id.
    pub fn get_revision(&self, rev_id: RevisionId) -> Result<NodeRevision, StoreError> {
        self.inner
            .lock()
            .expect("mutex poisoned")
            .revisions
            .get(&rev_id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("revision {rev_id}")))
    }
}

/// Info about a device, returned by `list_devices`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    /// Device ID.
    pub device_id: DeviceId,
    /// Device name.
    pub name: String,
    /// Platform metadata.
    pub platform: serde_json::Value,
    /// Revocation time, if revoked.
    pub revoked_at: Option<chrono::DateTime<Utc>>,
}

/// Info about a workspace, returned by `list_workspaces`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceInfo {
    /// Workspace ID.
    pub workspace_id: WorkspaceId,
    /// Workspace name.
    pub name: String,
    /// Root node ID.
    pub root_node_id: NodeId,
    /// Current cursor.
    pub current_cursor: Cursor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn setup_store() -> (Arc<MemoryStore>, UserId, DeviceId, WorkspaceId, NodeId) {
        let store = MemoryStore::shared();
        let user = store.create_user("test@example.com").unwrap();
        let device = store
            .register_device(
                user,
                "test-device",
                vec![1, 2, 3],
                serde_json::json!({"os": "linux"}),
            )
            .unwrap();
        let (ws, root) = store.create_workspace(user, "test-ws", device).unwrap();
        (store, user, device, ws, root)
    }

    fn make_op(ws: WorkspaceId, dev: DeviceId, kind: OperationKind) -> Operation {
        Operation {
            op_id: fs2_core::OpId::new(),
            workspace_id: ws,
            device_id: dev,
            base_cursor: Cursor::ZERO,
            kind,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn workspace_has_one_root() {
        let (store, _, _, _, root) = setup_store();
        // Each workspace has exactly one root.
        let node = store.get_node(root).unwrap();
        assert_eq!(node.kind, NodeKind::Directory);
        assert!(node.parent_id.is_none());
    }

    #[test]
    fn create_node_works() {
        let (store, _, dev, ws, root) = setup_store();
        let op = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        let cursor = store.commit_operation(op.clone()).unwrap();
        assert_eq!(cursor, Cursor(1));

        let children = store.list_children(root);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "apps");
    }

    #[test]
    fn duplicate_op_id_is_idempotent() {
        let (store, _, dev, ws, root) = setup_store();
        let op = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        let cursor1 = store.commit_operation(op.clone()).unwrap();
        let cursor2 = store.commit_operation(op).unwrap();
        assert_eq!(cursor1, cursor2);
        let children = store.list_children(root);
        assert_eq!(children.len(), 1); // not duplicated
    }

    #[test]
    fn collision_detected() {
        let (store, _, dev, ws, root) = setup_store();
        let op1 = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        store.commit_operation(op1).unwrap();

        let op2 = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        let err = store.commit_operation(op2).unwrap_err();
        assert!(matches!(err, StoreError::Collision(_)));
    }

    #[test]
    fn delete_and_restore() {
        let (store, _, dev, ws, root) = setup_store();
        let create = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "temp".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        store.commit_operation(create).unwrap();
        let node = store.list_children(root).into_iter().next().unwrap();

        let delete = make_op(
            ws,
            dev,
            OperationKind::DeleteNode {
                node_id: node.node_id,
                recursive: false,
            },
        );
        store.commit_operation(delete).unwrap();
        assert!(store.list_children(root).is_empty());

        let restore = make_op(
            ws,
            dev,
            OperationKind::RestoreNode {
                node_id: node.node_id,
                parent_id: root,
                name: "temp".to_owned(),
            },
        );
        store.commit_operation(restore).unwrap();
        assert_eq!(store.list_children(root).len(), 1);
    }

    #[test]
    fn delete_non_recursive_with_children_fails() {
        let (store, _, dev, ws, root) = setup_store();
        let create_dir = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "parent".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        store.commit_operation(create_dir).unwrap();
        let parent = store.list_children(root).into_iter().next().unwrap();

        let create_child = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: parent.node_id,
                name: "child".to_owned(),
                kind: NodeKind::File,
                initial_revision: None,
            },
        );
        store.commit_operation(create_child).unwrap();

        let delete = make_op(
            ws,
            dev,
            OperationKind::DeleteNode {
                node_id: parent.node_id,
                recursive: false,
            },
        );
        assert!(store.commit_operation(delete).is_err());

        // recursive works
        let delete_rec = make_op(
            ws,
            dev,
            OperationKind::DeleteNode {
                node_id: parent.node_id,
                recursive: true,
            },
        );
        store.commit_operation(delete_rec).unwrap();
    }

    #[test]
    fn revision_conflict_detected() {
        let (store, _, dev, ws, root) = setup_store();
        let create = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "file.txt".to_owned(),
                kind: NodeKind::File,
                initial_revision: None,
            },
        );
        store.commit_operation(create).unwrap();
        let node = store.list_children(root).into_iter().next().unwrap();
        let current_rev = store.get_node(node.node_id).unwrap().current_rev;

        let rev = NodeRevision {
            revision_id: RevisionId::new(),
            node_id: node.node_id,
            workspace_id: ws,
            device_id: dev,
            base_revision_id: Some(current_rev),
            content: RevisionContent::File {
                blob_id: fs2_core::BlobId::new("sha256:abc".to_owned()),
                chunk_ids: vec![],
                content_hash: "sha256:plain".to_owned(),
                encryption_header: None,
            },
            posix_mode: 0o644,
            mtime: Utc::now(),
            size: 10,
            executable: false,
            created_at: Utc::now(),
        };

        // First update with correct base succeeds.
        let put1 = make_op(
            ws,
            dev,
            OperationKind::PutFileRevision {
                node_id: node.node_id,
                base_revision_id: Some(current_rev),
                revision: rev.clone(),
            },
        );
        store.commit_operation(put1).unwrap();

        // Second update with stale base fails.
        let rev2 = NodeRevision {
            revision_id: RevisionId::new(),
            base_revision_id: Some(current_rev),
            ..rev.clone()
        };
        let put2 = make_op(
            ws,
            dev,
            OperationKind::PutFileRevision {
                node_id: node.node_id,
                base_revision_id: Some(current_rev), // stale
                revision: rev2,
            },
        );
        let err = store.commit_operation(put2).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
    }

    #[test]
    fn fetch_operations_paginates() {
        let (store, _, dev, ws, root) = setup_store();
        for i in 0..5 {
            let op = make_op(
                ws,
                dev,
                OperationKind::CreateNode {
                    parent_id: root,
                    name: format!("d{i}"),
                    kind: NodeKind::Directory,
                    initial_revision: None,
                },
            );
            store.commit_operation(op).unwrap();
        }

        let (page1, has_more) = store.fetch_operations(ws, Cursor::ZERO, 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert!(has_more);

        let (page2, has_more) = store.fetch_operations(ws, Cursor(2), 2).unwrap();
        assert_eq!(page2.len(), 2);
        assert!(has_more);

        let (page3, has_more) = store.fetch_operations(ws, Cursor(4), 2).unwrap();
        assert_eq!(page3.len(), 1);
        assert!(!has_more);
    }

    #[test]
    fn revoked_device_rejected() {
        let (store, user, _, ws, _) = setup_store();
        let dev2 = store
            .register_device(user, "dev2", vec![4, 5, 6], serde_json::json!({}))
            .unwrap();
        store.revoke_device(dev2).unwrap();
        assert!(store.is_device_revoked(dev2));

        let op = make_op(
            ws,
            dev2,
            OperationKind::CreateNode {
                parent_id: store.workspace_root(ws).unwrap(),
                name: "x".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        let err = store.commit_operation(op).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));
    }

    #[test]
    fn list_devices_works() {
        let (store, user, _, _, _) = setup_store();
        let dev2 = store
            .register_device(user, "dev2", vec![], serde_json::json!({}))
            .unwrap();
        let devices = store.list_devices(user);
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().any(|d| d.device_id == dev2));
    }

    #[test]
    fn move_node_works() {
        let (store, _, dev, ws, root) = setup_store();
        let create = make_op(
            ws,
            dev,
            OperationKind::CreateNode {
                parent_id: root,
                name: "old".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        );
        store.commit_operation(create).unwrap();
        let node = store.list_children(root).into_iter().next().unwrap();

        let mv = make_op(
            ws,
            dev,
            OperationKind::MoveNode {
                node_id: node.node_id,
                old_parent_id: root,
                old_name: "old".to_owned(),
                new_parent_id: root,
                new_name: "new".to_owned(),
            },
        );
        store.commit_operation(mv).unwrap();
        let children = store.list_children(root);
        assert_eq!(children[0].name, "new");
        assert_eq!(children[0].node_id, node.node_id);
    }

    #[test]
    fn _ensure_uuid_imported() {
        let _ = Uuid::nil();
    }
}
