//! Test harness: fake/local backend, two-client materialized sync.
//!
//! This crate provides utilities for integration testing the sync engine
//! without FUSE. It spins up a local backend (in-memory store + local blob
//! store), creates two client state directories, and verifies that metadata
//! and blob sync works end-to-end.

use std::sync::Arc;

use anyhow::Result;
use fs2_backend::MemoryStore;
use fs2_core::{
    Cursor, DeviceId, NodeKind, NodeRevision, Operation, OperationKind, RevisionContent,
    RevisionId, WorkspaceId,
};
use fs2_sync::LocalStore;
use tempfile::TempDir;

/// A test harness that sets up a local backend and two clients.
pub struct TestHarness {
    /// Temporary directory for all test state.
    _tmp: TempDir,
    /// Shared in-memory metadata store.
    pub store: Arc<MemoryStore>,
    /// Workspace ID for the test.
    pub workspace_id: WorkspaceId,
    /// Root node ID.
    pub root_node_id: fs2_core::NodeId,
    /// Device A.
    pub device_a: DeviceId,
    /// Device B.
    pub device_b: DeviceId,
    /// User ID.
    pub user_id: uuid::Uuid,
    /// Local store for client A.
    pub local_a: LocalStore,
    /// Local store for client B.
    pub local_b: LocalStore,
}

impl TestHarness {
    /// Create a new test harness with a workspace and two devices.
    ///
    /// # Errors
    /// Returns an error if setup fails.
    pub fn new() -> Result<Self> {
        let tmp = TempDir::new()?;
        let store = MemoryStore::shared();

        // Create a user and two devices.
        let user_id = store.create_dev_user("test@example.com")?;
        let device_a = store.register_device(user_id, "device-a", "fake-key-a")?;
        let device_b = store.register_device(user_id, "device-b", "fake-key-b")?;

        // Create a workspace.
        let ws = store.create_workspace(user_id, "test-ws", device_a.id)?;

        // Create local stores for both clients.
        let local_a = LocalStore::open(&tmp.path().join("client-a.sqlite"))?;
        let local_b = LocalStore::open(&tmp.path().join("client-b.sqlite"))?;

        // Register the workspace in both local stores.
        local_a.upsert_workspace(ws.id, "test-ws", ws.root_node_id)?;
        local_b.upsert_workspace(ws.id, "test-ws", ws.root_node_id)?;

        // Insert root node in both local stores.
        insert_root_node(&local_a, ws.id, ws.root_node_id)?;
        insert_root_node(&local_b, ws.id, ws.root_node_id)?;

        Ok(Self {
            _tmp: tmp,
            store,
            workspace_id: ws.id,
            root_node_id: ws.root_node_id,
            device_a: device_a.id,
            device_b: device_b.id,
            user_id,
            local_a,
            local_b,
        })
    }

    /// Create a `CreateNode` operation for a directory on device A.
    #[must_use]
    pub fn create_dir_op(&self, parent_id: fs2_core::NodeId, name: &str) -> Operation {
        Operation::new(
            self.workspace_id,
            self.device_a,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id,
                name: name.to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        )
    }

    /// Create a `CreateNode` operation for a file with content on device A.
    #[must_use]
    pub fn create_file_op(
        &self,
        parent_id: fs2_core::NodeId,
        name: &str,
        blob_id: &str,
        content: &str,
    ) -> Operation {
        let node_id = fs2_core::NodeId::new();
        let rev_id = RevisionId::new();
        Operation::new(
            self.workspace_id,
            self.device_a,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id,
                name: name.to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(NodeRevision {
                    revision_id: rev_id,
                    node_id,
                    workspace_id: self.workspace_id,
                    device_id: self.device_a,
                    base_revision_id: None,
                    content: RevisionContent::File {
                        blob_id: blob_id.to_owned(),
                        chunk_ids: vec![],
                        content_hash: content.to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: chrono::Utc::now(),
                    size: content.len() as u64,
                    executable: false,
                    created_at: chrono::Utc::now(),
                }),
            },
            chrono::Utc::now(),
        )
    }

    /// Sync operations from the backend to a local store.
    ///
    /// # Errors
    /// Returns an error if sync fails.
    pub fn sync_to(&self, local: &LocalStore) -> Result<()> {
        let mut cursor = local.get_cursor(self.workspace_id)?;
        loop {
            let (ops, has_more, next) =
                self.store
                    .fetch_operations(self.workspace_id, cursor, 100)?;
            for committed_op in &ops {
                local.apply_operation(&committed_op.op, committed_op.cursor)?;
            }
            cursor = next;
            if !has_more {
                break;
            }
        }
        Ok(())
    }
}

/// Insert a root node into a local store.
fn insert_root_node(
    local: &LocalStore,
    _workspace_id: WorkspaceId,
    root_node_id: fs2_core::NodeId,
) -> Result<()> {
    // We need to insert the root node directly via SQL since apply_operation
    // creates new node IDs. The root node ID is assigned by the backend.
    // For the test harness, we use the backend's root_node_id.
    //
    // This is a limitation of the current design where CreateNode doesn't
    // carry the assigned node_id. The test harness works around it by
    // inserting the root node manually.
    //
    // NOTE: This is a known issue (reviewer C3) that will be fixed when
    // CreateNode carries the backend-assigned node_id.
    local.set_hydration_state(root_node_id, "hydrated")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_creates_workspace_and_devices() {
        let harness = TestHarness::new().unwrap();
        assert!(harness.workspace_id.as_uuid().as_u128() > 0);
        // Two devices should be registered.
        let devices = harness.store.list_devices(harness.user_id).unwrap();
        assert_eq!(devices.len(), 2);
    }

    #[test]
    fn metadata_sync_from_backend_to_client() {
        let harness = TestHarness::new().unwrap();
        // Create a directory on the backend via device A.
        let op = harness.create_dir_op(harness.root_node_id, "apps");
        let _committed = harness.store.commit_operation(op).unwrap();

        // Sync to client B.
        let cursor = harness.local_b.get_cursor(harness.workspace_id).unwrap();
        let (ops, has_more, _) = harness
            .store
            .fetch_operations(harness.workspace_id, cursor, 100)
            .unwrap();
        assert!(!ops.is_empty());
        assert!(!has_more || ops.len() == 100);

        // Apply the operation to client B's local store.
        for committed_op in &ops {
            harness
                .local_b
                .apply_operation(&committed_op.op, committed_op.cursor)
                .unwrap();
        }

        // The directory should appear in client B's local store.
        let node = harness
            .local_b
            .get_node_by_path(harness.workspace_id, "apps")
            .unwrap();
        assert!(node.is_some());
        assert_eq!(node.unwrap().name, "apps");
    }

    #[test]
    fn two_clients_see_same_metadata() {
        let harness = TestHarness::new().unwrap();
        // Device A creates two directories.
        let op1 = harness.create_dir_op(harness.root_node_id, "apps");
        let _c1 = harness.store.commit_operation(op1).unwrap();
        let op2 = harness.create_dir_op(harness.root_node_id, "docs");
        let _c2 = harness.store.commit_operation(op2).unwrap();

        // Sync both clients.
        let cursor_a = harness.local_a.get_cursor(harness.workspace_id).unwrap();
        let (ops_a, _, _) = harness
            .store
            .fetch_operations(harness.workspace_id, cursor_a, 100)
            .unwrap();
        for committed_op in &ops_a {
            harness
                .local_a
                .apply_operation(&committed_op.op, committed_op.cursor)
                .unwrap();
        }
        // The loop above already applies all operations.

        let cursor_b = harness.local_b.get_cursor(harness.workspace_id).unwrap();
        let (ops_b, _, _) = harness
            .store
            .fetch_operations(harness.workspace_id, cursor_b, 100)
            .unwrap();
        for committed_op in &ops_b {
            harness
                .local_b
                .apply_operation(&committed_op.op, committed_op.cursor)
                .unwrap();
        }
        // The loop above already applies all operations.

        // Both clients should see the same directories.
        let children_a = harness
            .local_a
            .list_children(harness.workspace_id, Some(harness.root_node_id))
            .unwrap();
        let children_b = harness
            .local_b
            .list_children(harness.workspace_id, Some(harness.root_node_id))
            .unwrap();
        assert_eq!(children_a.len(), 2);
        assert_eq!(children_b.len(), 2);
    }
}
