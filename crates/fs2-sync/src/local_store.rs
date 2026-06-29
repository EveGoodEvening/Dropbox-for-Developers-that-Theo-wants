//! Local `SQLite` metadata store.
//!
//! Each client keeps metadata, hydration state, pending ops, blob cache,
//! rules, and conflicts in a local `SQLite` database. This module provides
//! the schema migrations, store API, and operation replay.

use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use fs2_core::{
    Cursor, NodeId, NodeKind, NodeRevision, Operation, OperationKind, RevisionContent, RevisionId,
    WorkspaceId,
};
use rusqlite::Connection;

/// Error returned by local store operations.
#[derive(Debug, thiserror::Error)]
pub enum LocalStoreError {
    /// `SQLite` error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Node not found.
    #[error("node not found: {0}")]
    NodeNotFound(String),
    /// Workspace not found.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),
    /// Invalid data.
    #[error("invalid data: {0}")]
    InvalidData(String),
}

/// Convenience alias.
pub type LocalStoreResult<T> = Result<T, LocalStoreError>;

/// Local `SQLite` store. Thread-safe via a Mutex-protected connection.
pub struct LocalStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for LocalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalStore").finish_non_exhaustive()
    }
}

impl LocalStore {
    /// Open or create a local store at the given path.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or migrations fail.
    pub fn open(path: &Path) -> LocalStoreResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.run_migrations()?;
        Ok(store)
    }

    /// Open an in-memory store (for tests).
    ///
    /// # Errors
    /// Returns an error if migrations fail.
    pub fn open_in_memory() -> LocalStoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.run_migrations()?;
        Ok(store)
    }

    fn run_migrations(&self) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(MIGRATION_SQL)?;
        Ok(())
    }

    /// Register a workspace in the local store.
    pub fn upsert_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        root_node_id: NodeId,
    ) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO local_workspaces \
             (workspace_id, name, mount_path, root_node_id, last_cursor, created_at) \
             VALUES (?1, ?2, NULL, ?3, 0, ?4)",
            rusqlite::params![
                workspace_id.to_string(),
                name,
                root_node_id.to_string(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get the last sync cursor for a workspace.
    pub fn get_cursor(&self, workspace_id: WorkspaceId) -> LocalStoreResult<Cursor> {
        let conn = self.conn.lock().unwrap();
        let cursor: i64 = conn
            .query_row(
                "SELECT last_cursor FROM local_workspaces WHERE workspace_id = ?1",
                rusqlite::params![workspace_id.to_string()],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(Cursor::from(cursor))
    }

    /// Update the sync cursor for a workspace.
    pub fn set_cursor(&self, workspace_id: WorkspaceId, cursor: Cursor) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE local_workspaces SET last_cursor = ?1 WHERE workspace_id = ?2",
            rusqlite::params![cursor.as_i64(), workspace_id.to_string()],
        )?;
        Ok(())
    }

    /// Get a node by its path in the workspace.
    pub fn get_node_by_path(
        &self,
        workspace_id: WorkspaceId,
        path: &str,
    ) -> LocalStoreResult<Option<LocalNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node_id, workspace_id, parent_id, name, kind, current_revision_id, deleted \
             FROM local_nodes WHERE workspace_id = ?1 AND path = ?2 AND deleted = 0",
        )?;
        let result = stmt
            .query_row(rusqlite::params![workspace_id.to_string(), path], |row| {
                Ok(LocalNode {
                    node_id: parse_node_id(row.get(0)?),
                    workspace_id: parse_workspace_id(row.get(1)?),
                    parent_id: parse_optional_node_id(row.get(2)?),
                    name: row.get(3)?,
                    kind: parse_node_kind(row.get::<_, String>(4)?),
                    current_revision_id: parse_optional_revision_id(row.get(5)?),
                    deleted: row.get::<_, i64>(6)? != 0,
                })
            })
            .ok();
        Ok(result)
    }

    /// Get a node by its id.
    pub fn get_node_by_id(&self, node_id: NodeId) -> LocalStoreResult<Option<LocalNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node_id, workspace_id, parent_id, name, kind, current_revision_id, deleted \
             FROM local_nodes WHERE node_id = ?1",
        )?;
        let result = stmt
            .query_row(rusqlite::params![node_id.to_string()], |row| {
                Ok(LocalNode {
                    node_id: parse_node_id(row.get(0)?),
                    workspace_id: parse_workspace_id(row.get(1)?),
                    parent_id: parse_optional_node_id(row.get(2)?),
                    name: row.get(3)?,
                    kind: parse_node_kind(row.get::<_, String>(4)?),
                    current_revision_id: parse_optional_revision_id(row.get(5)?),
                    deleted: row.get::<_, i64>(6)? != 0,
                })
            })
            .ok();
        Ok(result)
    }

    /// Get a revision by id, including its blob id and content metadata.
    ///
    /// Used by the hydration path to find the blob to download for a file node.
    pub fn get_revision(&self, revision_id: RevisionId) -> LocalStoreResult<Option<LocalRevision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT revision_id, node_id, blob_id, chunk_ids, symlink_target, size, content_hash, \
             encryption_header, posix_mode, mtime, created_at \
             FROM local_revisions WHERE revision_id = ?1",
        )?;
        let result = stmt
            .query_row(rusqlite::params![revision_id.to_string()], |row| {
                Ok(LocalRevision {
                    revision_id: parse_revision_id(row.get(0)?),
                    node_id: parse_node_id(row.get(1)?),
                    blob_id: row.get(2)?,
                    chunk_ids: row.get(3)?,
                    symlink_target: row.get(4)?,
                    size: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
                    content_hash: row.get(6)?,
                    encryption_header: row.get(7)?,
                    posix_mode: u32::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
                    mtime: row.get(9)?,
                    created_at: row.get(10)?,
                })
            })
            .ok();
        Ok(result)
    }


    /// List live children of a directory node.
    pub fn list_children(
        &self,
        workspace_id: WorkspaceId,
        parent_id: Option<NodeId>,
    ) -> LocalStoreResult<Vec<LocalNode>> {
        let conn = self.conn.lock().unwrap();
        let parent_str = parent_id.map(|n| n.to_string());
        let mut stmt = conn.prepare(
            "SELECT node_id, workspace_id, parent_id, name, kind, current_revision_id, deleted \
             FROM local_nodes WHERE workspace_id = ?1 AND parent_id IS ?2 AND deleted = 0",
        )?;
        let nodes = stmt
            .query_map(
                rusqlite::params![workspace_id.to_string(), parent_str],
                |row| {
                    Ok(LocalNode {
                        node_id: parse_node_id(row.get(0)?),
                        workspace_id: parse_workspace_id(row.get(1)?),
                        parent_id: parse_optional_node_id(row.get(2)?),
                        name: row.get(3)?,
                        kind: parse_node_kind(row.get::<_, String>(4)?),
                        current_revision_id: parse_optional_revision_id(row.get(5)?),
                        deleted: row.get::<_, i64>(6)? != 0,
                    })
                },
            )?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(nodes)
    }

    /// Apply a remote operation to the local store (operation replay).
    ///
    /// This updates local nodes, revisions, and the cursor atomically.
    pub fn apply_operation(&self, op: &Operation, cursor: Cursor) -> LocalStoreResult<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        apply_operation_tx(&tx, op, cursor)?;
        tx.commit()?;
        Ok(())
    }

    /// Put a pending operation in the local queue.
    pub fn put_pending_op(&self, op: &Operation) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        let payload = serde_json::to_string(&op.kind)
            .map_err(|e| LocalStoreError::InvalidData(format!("failed to serialize op: {e}")))?;
        conn.execute(
            "INSERT OR REPLACE INTO pending_ops \
             (op_id, workspace_id, kind, payload, created_at, retry_count, last_error) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL)",
            rusqlite::params![
                op.op_id.to_string(),
                op.workspace_id.to_string(),
                op_kind_name(&op.kind),
                payload,
                op.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// List all pending operations.
    pub fn list_pending_ops(&self) -> LocalStoreResult<Vec<PendingOp>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT op_id, workspace_id, kind, payload, created_at, retry_count, last_error \
             FROM pending_ops ORDER BY created_at",
        )?;
        let ops = stmt
            .query_map([], |row| {
                Ok(PendingOp {
                    op_id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    kind: row.get(2)?,
                    payload: row.get(3)?,
                    created_at: row.get(4)?,
                    retry_count: row.get(5)?,
                    last_error: row.get(6)?,
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();
        Ok(ops)
    }

    /// Remove a pending operation after it has been acknowledged.
    pub fn remove_pending_op(&self, op_id: &str) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pending_ops WHERE op_id = ?1",
            rusqlite::params![op_id],
        )?;
        Ok(())
    }

    /// Record a conflict for a node (used when a local op is stale).
    ///
    /// # Errors
    /// Returns an error if the conflict cannot be inserted.
    pub fn record_conflict(
        &self,
        workspace_id: WorkspaceId,
        node_id: NodeId,
        conflict_path: &str,
        remote_revision_id: Option<&str>,
        local_revision_id: Option<&str>,
    ) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO conflicts \
             (id, workspace_id, node_id, conflict_path, remote_revision_id, local_revision_id, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
            rusqlite::params![
                id,
                workspace_id.to_string(),
                node_id.to_string(),
                conflict_path,
                remote_revision_id,
                local_revision_id,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Mark a blob as cached.
    pub fn mark_blob_cached(&self, blob_id: &str, path: &str, size: u64) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO blob_cache (blob_id, path, size, verified, last_accessed_at, pinned_ref_count) \
             VALUES (?1, ?2, ?3, 1, ?4, 0)",
            rusqlite::params![blob_id, path, size, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Set the hydration state for a node.
    pub fn set_hydration_state(&self, node_id: NodeId, state: &str) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO local_state (node_id, hydration_state, local_blob_path, dirty_base_revision_id, last_accessed_at, pinned, error_code, error_message) \
             VALUES (?1, ?2, NULL, NULL, ?3, 0, NULL, NULL) \
             ON CONFLICT(node_id) DO UPDATE SET hydration_state = excluded.hydration_state, last_accessed_at = excluded.last_accessed_at",
            rusqlite::params![node_id.to_string(), state, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Get the hydration state for a node.
    pub fn get_hydration_state(&self, node_id: NodeId) -> LocalStoreResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row(
                "SELECT hydration_state FROM local_state WHERE node_id = ?1",
                rusqlite::params![node_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .ok();
        Ok(result)
    }

    /// Set the pinned flag for a node.
    pub fn set_pinned(&self, node_id: NodeId, pinned: bool) -> LocalStoreResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE local_state SET pinned = ?1 WHERE node_id = ?2",
            rusqlite::params![i32::from(pinned), node_id.to_string()],
        )?;
        Ok(())
    }

    /// Get the total cache size (sum of `blob_cache.size`).
    pub fn get_cache_size(&self) -> LocalStoreResult<u64> {
        let conn = self.conn.lock().unwrap();
        let size: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM blob_cache", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        Ok(u64::try_from(size).unwrap_or(0))
    }

    /// Prune the cache to fit within `max_bytes` using LRU eviction.
    ///
    /// Never evicts:
    /// - dirty local files (`hydration_state` = `DirtyLocal`)
    /// - uploading files (`hydration_state` = `Uploading`)
    /// - conflict files (`hydration_state` = `Conflict`)
    /// - pinned files (pinned = 1)
    ///
    /// Returns the number of bytes evicted.
    pub fn prune_cache(&self, max_bytes: u64) -> LocalStoreResult<u64> {
        let conn = self.conn.lock().unwrap();
        // Select evictable blobs ordered by last_accessed_at (oldest first).
        let mut stmt = conn.prepare(
            "SELECT bc.blob_id, bc.path, bc.size \
             FROM blob_cache bc \
             LEFT JOIN local_state ls ON ls.local_blob_path = bc.path \
             WHERE (ls.hydration_state IS NULL \
                    OR ls.hydration_state NOT IN ('DirtyLocal', 'Uploading', 'Conflict')) \
             AND COALESCE(ls.pinned, 0) = 0 \
             ORDER BY bc.last_accessed_at ASC",
        )?;
        let evictable: Vec<(String, String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .filter_map(std::result::Result::ok)
            .collect();
        let current_size: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM blob_cache", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        let current_size = u64::try_from(current_size).unwrap_or(0);
        if current_size <= max_bytes {
            return Ok(0);
        }
        let mut to_evict = current_size.saturating_sub(max_bytes);
        let mut evicted: u64 = 0;
        for (blob_id, path, size) in evictable {
            if to_evict == 0 {
                break;
            }
            // Delete the blob file.
            let _ = std::fs::remove_file(&path);
            // Remove from blob_cache.
            conn.execute(
                "DELETE FROM blob_cache WHERE blob_id = ?1",
                rusqlite::params![blob_id],
            )?;
            evicted += u64::try_from(size).unwrap_or(0);
            to_evict = to_evict.saturating_sub(u64::try_from(size).unwrap_or(0));
        }
        Ok(evicted)
    }

    /// Get the effective rule for a workspace and path.
    ///
    /// Returns the highest-priority rule's `(pattern, action)` for the given
    /// workspace. This is a simple implementation that ignores path matching
    /// and returns the highest-priority rule for the workspace overall.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn get_effective_rule(
        &self,
        workspace_id: WorkspaceId,
        _path: &str,
    ) -> LocalStoreResult<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT pattern, action FROM rules \
             WHERE workspace_id = ?1 \
             ORDER BY priority DESC \
             LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![workspace_id.to_string()])?;
        if let Some(row) = rows.next()? {
            let pattern: String = row.get(0)?;
            let action: String = row.get(1)?;
            Ok(Some((pattern, action)))
        } else {
            Ok(None)
        }
    }
}

/// A local node record.
#[derive(Debug, Clone)]
pub struct LocalNode {
    /// Node id.
    pub node_id: NodeId,
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Parent node id.
    pub parent_id: Option<NodeId>,
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: NodeKind,
    /// Current revision id.
    pub current_revision_id: Option<RevisionId>,
    /// Whether deleted.
    pub deleted: bool,
}

/// A local revision record, including blob id for file content.
#[derive(Debug, Clone)]
pub struct LocalRevision {
    /// Revision id.
    pub revision_id: RevisionId,
    /// Node id.
    pub node_id: NodeId,
    /// Blob id for file content (`None` for directories).
    pub blob_id: Option<String>,
    /// Chunk ids (JSON-encoded).
    pub chunk_ids: Option<String>,
    /// Symlink target (`Some` for symlinks).
    pub symlink_target: Option<String>,
    /// Content size in bytes.
    pub size: u64,
    /// Plaintext content hash.
    pub content_hash: Option<String>,
    /// Encryption header for the blob.
    pub encryption_header: Option<String>,
    /// POSIX mode bits.
    pub posix_mode: u32,
    /// Modification time.
    pub mtime: String,
    /// Creation time.
    pub created_at: String,
}

/// A pending operation in the local queue.
#[derive(Debug, Clone)]
pub struct PendingOp {
    /// Operation id (string).
    pub op_id: String,
    /// Workspace id (string).
    pub workspace_id: String,
    /// Operation kind name.
    pub kind: String,
    /// Serialized payload.
    pub payload: String,
    /// Creation timestamp.
    pub created_at: String,
    /// Retry count.
    pub retry_count: i64,
    /// Last error message.
    pub last_error: Option<String>,
}

/// Apply an operation within a transaction.
fn apply_operation_tx(
    tx: &rusqlite::Transaction<'_>,
    op: &Operation,
    cursor: Cursor,
) -> LocalStoreResult<()> {
    let ws_id = op.workspace_id.to_string();
    match &op.kind {
        OperationKind::CreateNode {
            parent_id,
            name,
            kind: node_kind,
            initial_revision,
        } => {
            let node_id = NodeId::new();
            let parent_str = parent_id.to_string();
            // Compute path from parent.
            let parent_path: String = tx
                .query_row(
                    "SELECT path FROM local_nodes WHERE node_id = ?1",
                    rusqlite::params![parent_str],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            let path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            let rev_id = initial_revision.as_ref().map(|r| r.revision_id.to_string());
            tx.execute(
                "INSERT INTO local_nodes \
                 (node_id, workspace_id, parent_id, name, normalized_name, path, kind, current_revision_id, deleted, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9)",
                rusqlite::params![
                    node_id.to_string(),
                    ws_id,
                    parent_str,
                    name,
                    name.to_lowercase(),
                    path,
                    node_kind.as_str(),
                    rev_id,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            if let Some(rev) = initial_revision {
                insert_revision_tx(tx, rev)?;
            }
        }
        OperationKind::PutFileRevision {
            node_id,
            base_revision_id: _,
            revision,
        } => {
            tx.execute(
                "UPDATE local_nodes SET current_revision_id = ?1, updated_at = ?2 WHERE node_id = ?3",
                rusqlite::params![
                    revision.revision_id.to_string(),
                    Utc::now().to_rfc3339(),
                    node_id.to_string(),
                ],
            )?;
            insert_revision_tx(tx, revision)?;
        }
        OperationKind::MoveNode {
            node_id,
            new_parent_id,
            new_name,
            ..
        } => {
            // Compute new path.
            let parent_path: String = tx
                .query_row(
                    "SELECT path FROM local_nodes WHERE node_id = ?1",
                    rusqlite::params![new_parent_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            let new_path = if parent_path.is_empty() {
                new_name.clone()
            } else {
                format!("{parent_path}/{new_name}")
            };
            // Get old path before updating.
            let old_path: String = tx
                .query_row(
                    "SELECT path FROM local_nodes WHERE node_id = ?1",
                    rusqlite::params![node_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            tx.execute(
                "UPDATE local_nodes SET parent_id = ?1, name = ?2, path = ?3, updated_at = ?4 WHERE node_id = ?5",
                rusqlite::params![
                    new_parent_id.to_string(),
                    new_name,
                    new_path,
                    Utc::now().to_rfc3339(),
                    node_id.to_string(),
                ],
            )?;
            // Update all descendant paths.
            if !old_path.is_empty() {
                let old_prefix = format!("{old_path}/");
                let new_prefix = format!("{new_path}/");
                tx.execute(
                    "UPDATE local_nodes SET path = ?1 || substr(path, ?2) \
                     WHERE workspace_id = ?3 AND path LIKE ?4 AND node_id != ?5",
                    rusqlite::params![
                        new_prefix,
                        (old_prefix.len() + 1) as i64,
                        ws_id,
                        format!("{old_prefix}%"),
                        node_id.to_string(),
                    ],
                )?;
            }
        }
        OperationKind::DeleteNode { node_id, recursive } => {
            if *recursive {
                // Mark all descendants as deleted by traversing the tree.
                let mut queue = vec![node_id.to_string()];
                while let Some(current) = queue.pop() {
                    let children: Vec<String> = tx
                        .prepare(
                            "SELECT node_id FROM local_nodes WHERE parent_id = ?1 AND deleted = 0",
                        )?
                        .query_map(rusqlite::params![current], |row| row.get::<_, String>(0))?
                        .filter_map(std::result::Result::ok)
                        .collect();
                    for child in children {
                        queue.push(child);
                    }
                    tx.execute(
                        "UPDATE local_nodes SET deleted = 1, updated_at = ?1 WHERE node_id = ?2",
                        rusqlite::params![Utc::now().to_rfc3339(), current],
                    )?;
                }
            } else {
                tx.execute(
                    "UPDATE local_nodes SET deleted = 1, updated_at = ?1 WHERE node_id = ?2",
                    rusqlite::params![Utc::now().to_rfc3339(), node_id.to_string()],
                )?;
            }
        }
        OperationKind::RestoreNode {
            node_id,
            parent_id,
            name,
        } => {
            let parent_path: String = tx
                .query_row(
                    "SELECT path FROM local_nodes WHERE node_id = ?1",
                    rusqlite::params![parent_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            let new_path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            tx.execute(
                "UPDATE local_nodes SET deleted = 0, parent_id = ?1, name = ?2, path = ?3, updated_at = ?4 WHERE node_id = ?5",
                rusqlite::params![
                    parent_id.to_string(),
                    name,
                    new_path,
                    Utc::now().to_rfc3339(),
                    node_id.to_string(),
                ],
            )?;
        }
        OperationKind::SetRule { .. }
        | OperationKind::SetEnvVar { .. }
        | OperationKind::DeleteEnvVar { .. } => {
            // Rule and env operations are logged but do not modify the node tree.
        }
    }
    // Update cursor.
    tx.execute(
        "UPDATE local_workspaces SET last_cursor = ?1 WHERE workspace_id = ?2",
        rusqlite::params![cursor.as_i64(), ws_id],
    )?;
    Ok(())
}

fn insert_revision_tx(tx: &rusqlite::Transaction<'_>, rev: &NodeRevision) -> LocalStoreResult<()> {
    let (blob_id, chunk_ids, symlink_target, content_hash, encryption_header) = match &rev.content {
        RevisionContent::Directory => (None, None, None, None, None),
        RevisionContent::File {
            blob_id,
            chunk_ids,
            content_hash,
            encryption_header,
        } => (
            Some(blob_id.as_str()),
            Some(serde_json::to_string(chunk_ids).unwrap_or_default()),
            None,
            Some(content_hash.as_str()),
            encryption_header.as_deref(),
        ),
        RevisionContent::Symlink { target } => (None, None, Some(target.as_str()), None, None),
    };
    tx.execute(
        "INSERT OR REPLACE INTO local_revisions \
         (revision_id, node_id, blob_id, chunk_ids, symlink_target, size, content_hash, encryption_header, posix_mode, mtime, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            rev.revision_id.to_string(),
            rev.node_id.to_string(),
            blob_id,
            chunk_ids,
            symlink_target,
            rev.size as i64,
            content_hash,
            encryption_header,
            i64::from(rev.posix_mode),
            rev.mtime.to_rfc3339(),
            rev.created_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn op_kind_name(kind: &OperationKind) -> &'static str {
    match kind {
        OperationKind::CreateNode { .. } => "create_node",
        OperationKind::PutFileRevision { .. } => "put_file_revision",
        OperationKind::MoveNode { .. } => "move_node",
        OperationKind::DeleteNode { .. } => "delete_node",
        OperationKind::RestoreNode { .. } => "restore_node",
        OperationKind::SetRule { .. } => "set_rule",
        OperationKind::SetEnvVar { .. } => "set_env_var",
        OperationKind::DeleteEnvVar { .. } => "delete_env_var",
    }
}

fn parse_node_id(s: String) -> NodeId {
    NodeId::from_uuid(s.parse().unwrap_or_else(|_| uuid::Uuid::nil()))
}

fn parse_workspace_id(s: String) -> WorkspaceId {
    WorkspaceId::from_uuid(s.parse().unwrap_or_else(|_| uuid::Uuid::nil()))
}

fn parse_optional_node_id(s: Option<String>) -> Option<NodeId> {
    s.map(parse_node_id)
}

fn parse_optional_revision_id(s: Option<String>) -> Option<RevisionId> {
    s.map(|v| RevisionId::from_uuid(v.parse().unwrap_or_else(|_| uuid::Uuid::nil())))
}

fn parse_revision_id(s: String) -> RevisionId {
    RevisionId::from_uuid(s.parse().unwrap_or_else(|_| uuid::Uuid::nil()))
}

fn parse_node_kind(s: String) -> NodeKind {
    NodeKind::from_str_err(&s).unwrap_or(NodeKind::File)
}

const MIGRATION_SQL: &str = "\
CREATE TABLE IF NOT EXISTS local_workspaces (
  workspace_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  mount_path TEXT,
  root_node_id TEXT NOT NULL,
  last_cursor INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS local_nodes (
  node_id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  parent_id TEXT,
  name TEXT NOT NULL,
  normalized_name TEXT NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  current_revision_id TEXT,
  deleted INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS local_nodes_path_idx ON local_nodes(workspace_id, path);
CREATE INDEX IF NOT EXISTS local_nodes_parent_idx ON local_nodes(workspace_id, parent_id);

CREATE TABLE IF NOT EXISTS local_revisions (
  revision_id TEXT PRIMARY KEY,
  node_id TEXT NOT NULL,
  blob_id TEXT,
  chunk_ids TEXT,
  symlink_target TEXT,
  size INTEGER NOT NULL,
  content_hash TEXT,
  encryption_header TEXT,
  posix_mode INTEGER NOT NULL,
  mtime TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS local_state (
  node_id TEXT PRIMARY KEY,
  hydration_state TEXT NOT NULL,
  local_blob_path TEXT,
  dirty_base_revision_id TEXT,
  last_accessed_at TEXT,
  pinned INTEGER NOT NULL DEFAULT 0,
  error_code TEXT,
  error_message TEXT
);

CREATE TABLE IF NOT EXISTS pending_ops (
  op_id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL,
  retry_count INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);

CREATE TABLE IF NOT EXISTS blob_cache (
  blob_id TEXT PRIMARY KEY,
  path TEXT NOT NULL,
  size INTEGER NOT NULL,
  verified INTEGER NOT NULL DEFAULT 0,
  last_accessed_at TEXT,
  pinned_ref_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS rules (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  pattern TEXT NOT NULL,
  action TEXT NOT NULL,
  source TEXT NOT NULL,
  priority INTEGER NOT NULL,
  metadata TEXT
);

CREATE TABLE IF NOT EXISTS conflicts (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  conflict_path TEXT NOT NULL,
  remote_revision_id TEXT,
  local_revision_id TEXT,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL
);
";

#[cfg(test)]
mod tests {
    use super::*;
    use fs2_core::{DeviceId, NodeKind};

    fn setup_store() -> (LocalStore, WorkspaceId, NodeId) {
        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        let root_id = NodeId::new();
        store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
        // Insert root node manually.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO local_nodes \
                 (node_id, workspace_id, parent_id, name, normalized_name, path, kind, current_revision_id, deleted, updated_at) \
                 VALUES (?1, ?2, NULL, '', '', '', 'directory', NULL, 0, ?3)",
                rusqlite::params![
                    root_id.to_string(),
                    ws_id.to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )
            .unwrap();
        }
        (store, ws_id, root_id)
    }

    #[test]
    fn open_in_memory_creates_tables() {
        let store = LocalStore::open_in_memory().unwrap();
        // Verify tables exist by querying them.
        let conn = store.conn.lock().unwrap();
        conn.query_row("SELECT count(*) FROM local_workspaces", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    }

    #[test]
    fn cursor_starts_at_zero() {
        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        store
            .upsert_workspace(ws_id, "test", NodeId::new())
            .unwrap();
        assert_eq!(store.get_cursor(ws_id).unwrap(), Cursor::zero());
    }

    #[test]
    fn apply_create_node_operation() {
        let (store, ws_id, root_node_id) = setup_store();
        let op = Operation::new(
            ws_id,
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: root_node_id,
                name: "apps".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        store.apply_operation(&op, Cursor::from(1)).unwrap();
        let node = store.get_node_by_path(ws_id, "apps").unwrap().unwrap();
        assert_eq!(node.name, "apps");
        assert_eq!(node.kind, NodeKind::Directory);
        assert!(!node.deleted);
        assert_eq!(store.get_cursor(ws_id).unwrap(), Cursor::from(1));
    }

    #[test]
    fn apply_delete_and_restore() {
        let (store, ws_id, root_node_id) = setup_store();
        // Create a node.
        let op = Operation::new(
            ws_id,
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: root_node_id,
                name: "file.txt".to_owned(),
                kind: NodeKind::File,
                initial_revision: None,
            },
            Utc::now(),
        );
        store.apply_operation(&op, Cursor::from(1)).unwrap();
        let node = store.get_node_by_path(ws_id, "file.txt").unwrap().unwrap();
        // Delete it.
        let del_op = Operation::new(
            ws_id,
            DeviceId::new(),
            Cursor::from(1),
            OperationKind::DeleteNode {
                node_id: node.node_id,
                recursive: false,
            },
            Utc::now(),
        );
        store.apply_operation(&del_op, Cursor::from(2)).unwrap();
        // Should not be findable by path (deleted = 1).
        assert!(store.get_node_by_path(ws_id, "file.txt").unwrap().is_none());
        // But should still exist by id.
        let deleted_node = store.get_node_by_id(node.node_id).unwrap().unwrap();
        assert!(deleted_node.deleted);
        // Restore it.
        let restore_op = Operation::new(
            ws_id,
            DeviceId::new(),
            Cursor::from(2),
            OperationKind::RestoreNode {
                node_id: node.node_id,
                parent_id: root_node_id,
                name: "file.txt".to_owned(),
            },
            Utc::now(),
        );
        store.apply_operation(&restore_op, Cursor::from(3)).unwrap();
        let restored = store.get_node_by_path(ws_id, "file.txt").unwrap().unwrap();
        assert!(!restored.deleted);
    }

    #[test]
    fn pending_ops_roundtrip() {
        let (store, _ws_id, _root_id) = setup_store();
        let ws_id = WorkspaceId::new();
        let op = Operation::new(
            ws_id,
            DeviceId::new(),
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: NodeId::new(),
                name: "test".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            Utc::now(),
        );
        store.put_pending_op(&op).unwrap();
        let pending = store.list_pending_ops().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, op.op_id.to_string());
        store.remove_pending_op(&op.op_id.to_string()).unwrap();
        assert!(store.list_pending_ops().unwrap().is_empty());
    }

    #[test]
    fn hydration_state_roundtrip() {
        let (store, _ws_id, _root_id) = setup_store();
        let node_id = NodeId::new();
        store.set_hydration_state(node_id, "hydrated").unwrap();
        assert_eq!(
            store.get_hydration_state(node_id).unwrap(),
            Some("hydrated".to_owned())
        );
    }

    #[test]
    fn blob_cache_roundtrip() {
        let (store, _ws_id, _root_id) = setup_store();
        store
            .mark_blob_cached("sha256:abc", "/tmp/blob", 1024)
            .unwrap();
        // Verify it exists.
        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM blob_cache WHERE blob_id = ?1",
                rusqlite::params!["sha256:abc"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn replay_from_zero() {
        let (store, ws_id, root_node_id) = setup_store();
        // Apply a sequence of operations.
        let ops = [
            Operation::new(
                ws_id,
                DeviceId::new(),
                Cursor::zero(),
                OperationKind::CreateNode {
                    parent_id: root_node_id,
                    name: "apps".to_owned(),
                    kind: NodeKind::Directory,
                    initial_revision: None,
                },
                Utc::now(),
            ),
            Operation::new(
                ws_id,
                DeviceId::new(),
                Cursor::from(1),
                OperationKind::CreateNode {
                    parent_id: root_node_id,
                    name: "docs".to_owned(),
                    kind: NodeKind::Directory,
                    initial_revision: None,
                },
                Utc::now(),
            ),
        ];
        for (i, op) in ops.iter().enumerate() {
            store
                .apply_operation(op, Cursor::from((i + 1) as i64))
                .unwrap();
        }
        // Verify state.
        let children = store.list_children(ws_id, Some(root_node_id)).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(store.get_cursor(ws_id).unwrap(), Cursor::from(2));
    }

    #[test]
    fn cache_size_accounting() {
        let (store, _ws_id, _root_id) = setup_store();
        store
            .mark_blob_cached("sha256:abc", "/tmp/blob1", 1024)
            .unwrap();
        store
            .mark_blob_cached("sha256:def", "/tmp/blob2", 2048)
            .unwrap();
        assert_eq!(store.get_cache_size().unwrap(), 3072);
    }

    #[test]
    fn prune_cache_evicts_oldest() {
        let (store, _ws_id, _root_id) = setup_store();
        // Add two blobs.
        store
            .mark_blob_cached("sha256:old", "/tmp/old_blob", 1024)
            .unwrap();
        store
            .mark_blob_cached("sha256:new", "/tmp/new_blob", 1024)
            .unwrap();
        assert_eq!(store.get_cache_size().unwrap(), 2048);
        // Prune to 1024 bytes — should evict one blob.
        let evicted = store.prune_cache(1024).unwrap();
        assert!(evicted >= 1024);
        assert_eq!(store.get_cache_size().unwrap(), 1024);
    }

    #[test]
    fn prune_cache_no_op_when_under_limit() {
        let (store, _ws_id, _root_id) = setup_store();
        store
            .mark_blob_cached("sha256:abc", "/tmp/blob", 100)
            .unwrap();
        let evicted = store.prune_cache(1000).unwrap();
        assert_eq!(evicted, 0);
        assert_eq!(store.get_cache_size().unwrap(), 100);
    }
}
