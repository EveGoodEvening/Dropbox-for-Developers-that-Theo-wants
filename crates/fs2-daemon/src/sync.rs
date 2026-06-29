//! Sync engine: outbound queue, inbound loop, offline detection.
//!
//! The outbound queue persists pending operations in `SQLite` and submits them
//! to the backend idempotently. The inbound loop fetches remote operations
//! and applies them locally.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fs2_core::{Cursor, WorkspaceId};
use fs2_sync::{ApiClient, LocalStore};
use tracing::{debug, info, warn};

/// Daemon sync state.
#[derive(Debug, Clone)]
pub struct SyncState {
    /// Whether the daemon is online (backend reachable).
    pub online: Arc<AtomicBool>,
}

impl SyncState {
    /// Create a new sync state (initially offline).
    #[must_use]
    pub fn new() -> Self {
        Self {
            online: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Check if the daemon is online.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Set the online status.
    pub fn set_online(&self, online: bool) {
        self.online.store(online, Ordering::Relaxed);
    }
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the workspace-relative target path of an operation for rule
/// checking. Returns `None` if the path cannot be determined (e.g. the
/// node no longer exists).
fn op_target_path(store: &LocalStore, op: &fs2_core::Operation) -> Option<String> {
    use fs2_core::OperationKind;
    match &op.kind {
        OperationKind::CreateNode { parent_id, name, .. } => {
            let parent_path = store.node_path(*parent_id).ok().flatten().unwrap_or_default();
            Some(if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            })
        }
        OperationKind::PutFileRevision { node_id, .. }
        | OperationKind::MoveNode { node_id, .. }
        | OperationKind::DeleteNode { node_id, .. }
        | OperationKind::RestoreNode { node_id, .. } => {
            store.node_path(*node_id).ok().flatten()
        }
        _ => None,
    }
}

/// Whether an op should be suppressed (not uploaded) based on its rule action.
fn op_suppressed(action: fs2_rules::Action) -> bool {
    action.suppresses_upload()
}

/// Check whether a pending op should be suppressed before uploading.
/// Returns true if the op targets a generated/ignored/local-only/dependency-cache
/// path (design §8/§9, upload-storm suppression).
fn should_suppress_op(store: &LocalStore, op: &fs2_core::Operation) -> bool {
    if let Some(path) = op_target_path(store, op) {
        let engine = fs2_rules::RuleEngine::new(
            fs2_rules::builtin_profiles(),
            fs2_rules::Action::Normal,
        );
        let rule = engine.resolve(&path);
        if op_suppressed(rule.action) {
            debug!("suppressing op for {path} (action={})", rule.action);
            return true;
        }
    }
    false
}

/// Outbound queue: submits pending operations to the backend.
pub struct OutboundQueue {
    /// API client.
    client: ApiClient,
    /// Local store.
    store: Arc<LocalStore>,
    /// Sync state.
    state: SyncState,
}

impl OutboundQueue {
    /// Create a new outbound queue.
    #[must_use]
    pub fn new(client: ApiClient, store: Arc<LocalStore>, state: SyncState) -> Self {
        Self {
            client,
            store,
            state,
        }
    }

    /// Process all pending operations: submit each to the backend.
    ///
    /// Removes pending ops only after the backend acknowledges them.
    /// On failure, increments retry count and keeps the op.
    ///
    /// # Errors
    /// Returns an error if the local store cannot be accessed.
    pub async fn process_pending(&self, workspace_id: WorkspaceId) -> anyhow::Result<usize> {
        let pending = self.store.list_pending_ops()?;
        let mut processed = 0;
        for op_data in pending {
            // Deserialize the operation from the payload.
            let op_kind: fs2_core::OperationKind = match serde_json::from_str(&op_data.payload) {
                Ok(k) => k,
                Err(e) => {
                    warn!("failed to deserialize pending op: {e}");
                    continue;
                }
            };

            // Reconstruct the operation.
            let op_id: uuid::Uuid = op_data.op_id.parse().unwrap_or_else(|_| uuid::Uuid::nil());
            let ws_id: uuid::Uuid = op_data
                .workspace_id
                .parse()
                .unwrap_or_else(|_| uuid::Uuid::nil());
            let op = fs2_core::Operation {
                op_id: fs2_core::OpId::from_uuid(op_id),
                workspace_id: WorkspaceId::from_uuid(ws_id),
                device_id: fs2_core::DeviceId::from_uuid(uuid::Uuid::nil()),
                base_cursor: Cursor::zero(),
                kind: op_kind,
                created_at: chrono::Utc::now(),
            };

            // Rule-based suppression: skip ops targeting generated/ignored/
            // local-only/dependency-cache paths (design §8/§9, upload-storm
            // suppression). The op is removed from the queue without uploading.
            if should_suppress_op(&self.store, &op) {
                if let Err(e) = self.store.remove_pending_op(&op_data.op_id) {
                    warn!("failed to remove suppressed op: {e}");
                }
                continue;
            }

            // Submit to backend.
            match self.client.commit_operation(workspace_id, &op).await {
                Ok(_response) => {
                    // Success: remove from pending queue.
                    if let Err(e) = self.store.remove_pending_op(&op_data.op_id) {
                        warn!("failed to remove pending op: {e}");
                    }
                    processed += 1;
                    debug!("submitted pending op {} successfully", op_data.op_id);
                }
                Err(e) => {
                    warn!("failed to submit pending op {}: {e}", op_data.op_id);
                    // Keep in queue for retry.
                }
            }
        }
        Ok(processed)
    }

    /// Run the outbound loop: periodically process pending operations.
    ///
    /// This runs until the `shutdown` signal is received.
    pub async fn run(
        self,
        workspace_id: WorkspaceId,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if self.state.is_online() {
                        if let Err(e) = self.process_pending(workspace_id).await {
                            warn!("outbound queue error: {e}");
                        }
                    }
                }
                result = shutdown.changed() => {
                    if result.is_ok() && *shutdown.borrow() {
                        info!("outbound queue shutting down");
                        break;
                    }
                }
            }
        }
    }
}

/// Inbound sync loop: fetches remote operations and applies them locally.
pub struct InboundLoop {
    /// API client.
    client: ApiClient,
    /// Local store.
    store: Arc<LocalStore>,
    /// Sync state.
    state: SyncState,
}

impl InboundLoop {
    /// Create a new inbound loop.
    #[must_use]
    pub fn new(client: ApiClient, store: Arc<LocalStore>, state: SyncState) -> Self {
        Self {
            client,
            store,
            state,
        }
    }

    /// Fetch and apply remote operations since the local cursor.
    ///
    /// # Errors
    /// Returns an error if fetching or applying operations fails.
    pub async fn sync_remote(&self, workspace_id: WorkspaceId) -> anyhow::Result<usize> {
        let cursor = self.store.get_cursor(workspace_id)?;
        let response = self
            .client
            .fetch_operations(workspace_id, cursor, 100)
            .await?;

        let mut applied = 0;
        for op in &response.operations {
            // Use the operation's cursor from the response.
            // In a real implementation, the cursor would come from the
            // committed operation metadata. For now, we advance the cursor
            // after each successful apply.
            if let Err(e) = self.store.apply_operation(op, cursor) {
                warn!("failed to apply remote op: {e}");
                break;
            }
            applied += 1;
        }

        // Update cursor to next_cursor from the response.
        if applied > 0 {
            self.store
                .set_cursor(workspace_id, Cursor::from(response.next_cursor))?;
        }

        Ok(applied)
    }

    /// Rebase after a reconnect: fetch and apply remote ops since the local
    /// cursor, then process pending local ops.
    ///
    /// Each pending op is submitted to the backend. If the backend rejects it
    /// because the base revision is stale (revision conflict), a conflict
    /// record is created locally and the op is removed from the queue.
    /// Otherwise the op is removed on success and kept for retry on
    /// transient failures.
    ///
    /// # Errors
    /// Returns an error if fetching/applying remote ops or accessing the
    /// local store fails.
    pub async fn rebase_after_reconnect(&self, workspace_id: WorkspaceId) -> anyhow::Result<()> {
        // 1. Fetch and apply remote ops since the local cursor.
        let applied = self.sync_remote(workspace_id).await?;
        if applied > 0 {
            info!("rebase: applied {applied} remote ops");
        }

        // 2. Process pending local ops: submit each, create conflict if stale.
        let pending = self.store.list_pending_ops()?;
        let mut submitted = 0;
        let mut conflicts = 0;
        for op_data in pending {
            // Deserialize the operation kind from the payload.
            let op_kind: fs2_core::OperationKind = match serde_json::from_str(&op_data.payload) {
                Ok(k) => k,
                Err(e) => {
                    warn!("rebase: failed to deserialize pending op: {e}");
                    continue;
                }
            };

            let op_id: uuid::Uuid = op_data.op_id.parse().unwrap_or_else(|_| uuid::Uuid::nil());
            let ws_id: uuid::Uuid = op_data
                .workspace_id
                .parse()
                .unwrap_or_else(|_| uuid::Uuid::nil());
            let op = fs2_core::Operation {
                op_id: fs2_core::OpId::from_uuid(op_id),
                workspace_id: WorkspaceId::from_uuid(ws_id),
                device_id: fs2_core::DeviceId::from_uuid(uuid::Uuid::nil()),
                base_cursor: Cursor::zero(),
                kind: op_kind,
                created_at: chrono::Utc::now(),
            };

            // Apply the same rule-based suppression as the outbound queue so
            // generated/ignored ops are not uploaded during rebase either.
            if should_suppress_op(&self.store, &op) {
                if let Err(e) = self.store.remove_pending_op(&op_data.op_id) {
                    warn!("rebase: failed to remove suppressed op: {e}");
                }
                continue;
            }

            match self.client.commit_operation(workspace_id, &op).await {
                Ok(_response) => {
                    if let Err(e) = self.store.remove_pending_op(&op_data.op_id) {
                        warn!("rebase: failed to remove pending op: {e}");
                    }
                    submitted += 1;
                    debug!("rebase: submitted pending op {}", op_data.op_id);
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("revision_conflict") || msg.contains("conflict") {
                        // Stale base revision: record a conflict preserving the
                        // local revision reference and mark the node conflict
                        // state so its bytes are not evicted (design §13.3/§7.3).
                        let device_name = hostname::get()
                            .map_or_else(|_| "unknown".to_owned(), |h| h.to_string_lossy().to_string());
                        if let Some(node_id) = op.target_node() {
                            if let Err(err) = record_stale_op_conflict(
                                &self.store,
                                workspace_id,
                                node_id,
                                &op,
                                &device_name,
                            ) {
                                warn!("rebase: failed to record conflict: {err}");
                            }
                        }
                        if let Err(err) = self.store.remove_pending_op(&op_data.op_id) {
                            warn!("rebase: failed to remove stale op: {err}");
                        }
                        conflicts += 1;
                        warn!(
                            "rebase: pending op {} is stale, recorded conflict",
                            op_data.op_id
                        );
                    } else {
                        // Transient failure: keep in queue for retry.
                        warn!("rebase: failed to submit pending op {}: {e}", op_data.op_id);
                    }
                }
            }
        }

        info!("rebase: submitted {submitted} ops, {conflicts} conflicts");
        Ok(())
    }

    /// Check backend health and update online status.
    pub async fn check_health(&self) {
        let healthy = self.client.health().await.unwrap_or(false);
        let was_online = self.state.is_online();
        self.state.set_online(healthy);
        if healthy != was_online {
            if healthy {
                info!("backend is now online");
            } else {
                warn!("backend is now offline");
            }
        }
    }

    /// Run the inbound loop: periodically fetch and apply remote operations.
    ///
    /// This runs until the `shutdown` signal is received.
    pub async fn run(
        self,
        workspace_id: WorkspaceId,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut health_interval = tokio::time::interval(Duration::from_secs(10));
        let mut sync_interval = tokio::time::interval(Duration::from_secs(30));

        // Spawn a WebSocket event listener that pushes live change
        // notifications to a channel. On each event we fetch and apply remote
        // ops immediately, instead of waiting for the next poll tick. The
        // periodic sync_interval remains as a fallback for missed events and
        // offline-to-online transitions.
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<fs2_sync::WorkspaceEvent>();
        let ws_shutdown = shutdown.clone();
        let ws_client = self.client.clone();
        let ws_handle = tokio::spawn(fs2_sync::listen_workspace_events(
            ws_client,
            workspace_id,
            event_tx,
            ws_shutdown,
        ));

        loop {
            tokio::select! {
                _ = health_interval.tick() => {
                    self.check_health().await;
                }
                _ = sync_interval.tick() => {
                    if self.state.is_online() {
                        if let Err(e) = self.sync_remote(workspace_id).await {
                            warn!("inbound sync error: {e}");
                        }
                    }
                }
                Some(_event) = event_rx.recv() => {
                    // Live event: fetch and apply remote ops immediately.
                    if self.state.is_online() {
                        if let Err(e) = self.sync_remote(workspace_id).await {
                            warn!("inbound event sync error: {e}");
                        }
                    }
                }
                result = shutdown.changed() => {
                    if result.is_ok() && *shutdown.borrow() {
                        info!("inbound loop shutting down");
                        ws_handle.abort();
                        break;
                    }
                }
            }
        }
    }
}

/// Record a conflict for a stale operation, preserving the local revision
/// reference and marking the node's hydration state as `conflict` so its
/// bytes are not evicted (`design.md` §13.3/§7.3).
///
/// # Errors
/// Returns an error if the conflict cannot be recorded or the node state
/// cannot be updated.
fn record_stale_op_conflict(
    store: &fs2_sync::LocalStore,
    workspace_id: fs2_core::WorkspaceId,
    node_id: fs2_core::NodeId,
    op: &fs2_core::Operation,
    device_name: &str,
) -> anyhow::Result<()> {
    let ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H-%M-%S")
        .to_string();
    // Derive a display filename from the node, if known.
    let base_name = store
        .get_node_by_id(node_id)
        .ok()
        .flatten()
        .map_or_else(|| node_id.to_string(), |n| n.name);
    let conflict_path = conflict_filename(&base_name, device_name, &ts);
    // Extract the local revision id from a PutFileRevision op.
    let local_rev = match &op.kind {
        fs2_core::OperationKind::PutFileRevision { revision, .. } => {
            Some(revision.revision_id.to_string())
        }
        _ => None,
    };
    store.record_conflict(
        workspace_id,
        node_id,
        &conflict_path,
        None,
        local_rev.as_deref(),
    )?;
    // Mark the node's local state as Conflict so its bytes are not evicted.
    store.set_hydration_state(node_id, "conflict")?;
    Ok(())
}

/// Generate a deterministic conflict filename per `design.md` §13.3:
/// `<filename>.conflict.<device-name>.<timestamp>.<ext>`.
///
/// Sanitizes the device name and preserves the file extension.
fn conflict_filename(base_name: &str, device_name: &str, ts: &str) -> String {
    let sanitized_device: String = device_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    // Split base_name into stem + extension.
    let (stem, ext) = match base_name.rfind('.') {
        Some(idx) if idx > 0 => (&base_name[..idx], &base_name[idx + 1..]),
        _ => (base_name, ""),
    };
    if ext.is_empty() {
        format!("{stem}.conflict.{sanitized_device}.{ts}")
    } else {
        format!("{stem}.conflict.{sanitized_device}.{ts}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_online_offline() {
        let state = SyncState::new();
        assert!(!state.is_online());
        state.set_online(true);
        assert!(state.is_online());
        state.set_online(false);
        assert!(!state.is_online());
    }

    #[test]
    fn conflict_filename_preserves_extension() {
        let name = conflict_filename("app.ts", "mac-mini-2", "2026-06-28T13-04-11");
        assert_eq!(name, "app.conflict.mac-mini-2.2026-06-28T13-04-11.ts");
    }

    #[test]
    fn conflict_filename_no_extension() {
        let name = conflict_filename("README", "laptop", "2026-06-29T00-00-00");
        assert_eq!(name, "README.conflict.laptop.2026-06-29T00-00-00");
    }

    #[test]
    fn conflict_filename_sanitizes_device_name() {
        let name = conflict_filename("app.ts", "my device/name", "2026-06-29T00-00-00");
        assert_eq!(name, "app.conflict.my-device-name.2026-06-29T00-00-00.ts");
    }

    #[test]
    fn record_stale_op_conflict_preserves_local_revision_and_marks_state() {
        use fs2_core::{Cursor, DeviceId, NodeKind, NodeRevision, Operation, OperationKind,
            RevisionContent, RevisionId, WorkspaceId};
        use fs2_sync::LocalStore;

        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        let root_id = fs2_core::NodeId::new();
        let device = DeviceId::new();
        store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
        store.insert_root_node(ws_id, root_id).unwrap();

        // Create a file node via operation replay.
        let file_node_id = fs2_core::NodeId::new();
        let rev_id = RevisionId::new();
        let blob_id = format!("sha256:{file_node_id}");
        let op = Operation::new(
            ws_id,
            device,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: root_id,
                name: "app.ts".to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(NodeRevision {
                    revision_id: rev_id,
                    node_id: file_node_id,
                    workspace_id: ws_id,
                    device_id: device,
                    base_revision_id: None,
                    content: RevisionContent::File {
                        blob_id: blob_id.clone(),
                        chunk_ids: vec![],
                        content_hash: "hash".to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: chrono::Utc::now(),
                    size: 10,
                    executable: false,
                    created_at: chrono::Utc::now(),
                }),
            },
            chrono::Utc::now(),
        );
        store.apply_operation(&op, Cursor::from(1)).unwrap();
        let file_node = store
            .get_node_by_path(ws_id, "app.ts")
            .unwrap()
            .expect("file node");

        // Build a PutFileRevision op (the stale local edit) carrying a new revision.
        let new_rev_id = RevisionId::new();
        let stale_op = Operation::new(
            ws_id,
            device,
            Cursor::from(1),
            OperationKind::PutFileRevision {
                node_id: file_node.node_id,
                base_revision_id: Some(rev_id),
                revision: NodeRevision {
                    revision_id: new_rev_id,
                    node_id: file_node.node_id,
                    workspace_id: ws_id,
                    device_id: device,
                    base_revision_id: Some(rev_id),
                    content: RevisionContent::File {
                        blob_id: format!("sha256:{new_rev_id}"),
                        chunk_ids: vec![],
                        content_hash: "hash2".to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: chrono::Utc::now(),
                    size: 12,
                    executable: false,
                    created_at: chrono::Utc::now(),
                },
            },
            chrono::Utc::now(),
        );

        // Record the conflict as the rebase path would.
        record_stale_op_conflict(&store, ws_id, file_node.node_id, &stale_op, "laptop-1")
            .unwrap();

        // A conflict row exists with the local revision id preserved.
        let conflicts = store.list_conflicts(ws_id).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].local_revision_id.as_deref(),
            Some(new_rev_id.to_string().as_str()),
            "local revision id must be preserved in the conflict row"
        );
        assert!(conflicts[0].conflict_path.contains("app.conflict.laptop-1."));
        assert_eq!(std::path::Path::new(&conflicts[0].conflict_path).extension().and_then(|e| e.to_str()), Some("ts"), "conflict path preserves extension");

        // The node's hydration state is 'conflict' (non-evictable).
        let state = store
            .get_hydration_state(file_node.node_id)
            .unwrap()
            .expect("state row");
        assert_eq!(state, "conflict");
    }

    #[test]
    fn op_suppressed_filters_generated_and_ignored() {
        assert!(op_suppressed(fs2_rules::Action::Ignore));
        assert!(op_suppressed(fs2_rules::Action::Generated));
        assert!(op_suppressed(fs2_rules::Action::LocalOnly));
        assert!(op_suppressed(fs2_rules::Action::DependencyCache));
        assert!(!op_suppressed(fs2_rules::Action::Normal));
        assert!(!op_suppressed(fs2_rules::Action::Lazy));
        assert!(!op_suppressed(fs2_rules::Action::Pin));
    }

    #[test]
    fn op_target_path_resolves_create_node() {
        use fs2_core::{Cursor, DeviceId, NodeKind, Operation, OperationKind};
        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        let root_id = fs2_core::NodeId::new();
        let device = DeviceId::new();
        store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
        store.insert_root_node(ws_id, root_id).unwrap();
        // Create a node_modules directory under root.
        let op = Operation::new(
            ws_id,
            device,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: root_id,
                name: "node_modules".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        store.apply_operation(&op, Cursor::from(1)).unwrap();
        // The target path of a CreateNode for node_modules/pkg under the
        // node_modules dir should resolve.
        let nm_node = store
            .get_node_by_path(ws_id, "node_modules")
            .unwrap()
            .expect("node_modules");
        let child_op = Operation::new(
            ws_id,
            device,
            Cursor::from(1),
            OperationKind::CreateNode {
                parent_id: nm_node.node_id,
                name: "pkg".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        let path = op_target_path(&store, &child_op).expect("path");
        assert_eq!(path, "node_modules/pkg");
        // The rule engine suppresses node_modules paths.
        let engine = fs2_rules::RuleEngine::new(
            fs2_rules::builtin_profiles(),
            fs2_rules::Action::Normal,
        );
        let rule = engine.resolve(&path);
        assert!(op_suppressed(rule.action), "node_modules/pkg should be suppressed");
    }

    #[test]
    fn upload_storm_suppression_skips_generated_ops() {
        // 23.3: a pending op targeting a generated path is suppressed
        // (removed without uploading). We verify the suppression decision
        // by checking that op_suppressed + op_target_path correctly identify
        // node_modules ops as suppressible.
        use fs2_core::{Cursor, DeviceId, NodeKind, Operation, OperationKind};
        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        let root_id = fs2_core::NodeId::new();
        let device = DeviceId::new();
        store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
        store.insert_root_node(ws_id, root_id).unwrap();
        // Create a node_modules/pkg/index.js op (generated path).
        let nm_op = Operation::new(
            ws_id,
            device,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: root_id,
                name: "node_modules".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        store.apply_operation(&nm_op, Cursor::from(1)).unwrap();
        let nm_node = store
            .get_node_by_path(ws_id, "node_modules")
            .unwrap()
            .expect("node_modules");
        let pkg_op = Operation::new(
            ws_id,
            device,
            Cursor::from(1),
            OperationKind::CreateNode {
                parent_id: nm_node.node_id,
                name: "pkg".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        store.apply_operation(&pkg_op, Cursor::from(2)).unwrap();
        let pkg_node = store
            .get_node_by_path(ws_id, "node_modules/pkg")
            .unwrap()
            .expect("pkg");
        let file_op = Operation::new(
            ws_id,
            device,
            Cursor::from(2),
            OperationKind::CreateNode {
                parent_id: pkg_node.node_id,
                name: "index.js".to_owned(),
                kind: NodeKind::File,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        let path = op_target_path(&store, &file_op).expect("path");
        assert_eq!(path, "node_modules/pkg/index.js");
        let engine = fs2_rules::RuleEngine::new(
            fs2_rules::builtin_profiles(),
            fs2_rules::Action::Normal,
        );
        let rule = engine.resolve(&path);
        assert!(
            op_suppressed(rule.action),
            "node_modules/pkg/index.js must be suppressed (action={})",
            rule.action
        );
        // A normal source file is NOT suppressed.
        let src_op = Operation::new(
            ws_id,
            device,
            Cursor::from(3),
            OperationKind::CreateNode {
                parent_id: root_id,
                name: "main.rs".to_owned(),
                kind: NodeKind::File,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        let src_path = op_target_path(&store, &src_op).expect("src path");
        let src_rule = engine.resolve(&src_path);
        assert!(!op_suppressed(src_rule.action), "main.rs should not be suppressed");
    }
}
