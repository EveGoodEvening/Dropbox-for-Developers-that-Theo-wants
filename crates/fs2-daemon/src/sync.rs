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
                        // Stale base revision: record a conflict and drop the op.
                        if let Some(node_id) = op.target_node() {
                            let conflict_path = format!("conflict-{}", op_data.op_id);
                            if let Err(err) = self.store.record_conflict(
                                workspace_id,
                                node_id,
                                &conflict_path,
                                None,
                                None,
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
        let mut sync_interval = tokio::time::interval(Duration::from_secs(5));
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
                result = shutdown.changed() => {
                    if result.is_ok() && *shutdown.borrow() {
                        info!("inbound loop shutting down");
                        break;
                    }
                }
            }
        }
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
}
