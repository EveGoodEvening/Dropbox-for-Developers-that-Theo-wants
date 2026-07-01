#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Local daemon metadata store and operation replay.

use chrono::{DateTime, Utc};
use fs2_core::{
    BlobId, Cursor, FsRule, Node, NodeId, NodeKind, NodeRevision, Operation, OperationKind,
    RevisionId, WorkspaceId, WorkspacePath,
};
use fs2_rules::{rule_pattern_matches, RulePathKind};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::{fmt, fs, path::Path};

/// Result type for local daemon operations.
pub type Result<T, E = LocalStoreError> = std::result::Result<T, E>;

/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-daemon"
}

#[derive(Debug)]
pub enum LocalStoreError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
    Invalid(String),
}

impl fmt::Display for LocalStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LocalStoreError {}

impl From<rusqlite::Error> for LocalStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for LocalStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<std::io::Error> for LocalStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<fs2_core::ParseFs2IdError> for LocalStoreError {
    fn from(error: fs2_core::ParseFs2IdError) -> Self {
        Self::Invalid(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationState {
    MetadataOnly,
    Hydrated,
    Dirty,
    Uploading,
    Conflict,
}

impl fmt::Display for HydrationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MetadataOnly => "metadata_only",
            Self::Hydrated => "hydrated",
            Self::Dirty => "dirty",
            Self::Uploading => "uploading",
            Self::Conflict => "conflict",
        })
    }
}

impl std::str::FromStr for HydrationState {
    type Err = LocalStoreError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "metadata_only" => Ok(Self::MetadataOnly),
            "hydrated" => Ok(Self::Hydrated),
            "dirty" => Ok(Self::Dirty),
            "uploading" => Ok(Self::Uploading),
            "conflict" => Ok(Self::Conflict),
            _ => Err(LocalStoreError::Invalid(format!(
                "unknown hydration state {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobCacheEntry {
    pub blob_id: BlobId,
    pub path: String,
    pub size: u64,
    pub verified: bool,
    pub pinned_ref_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOperation {
    pub operation: Operation,
    pub retry_count: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingBlobUpload {
    pub blob_id: BlobId,
    pub workspace_id: WorkspaceId,
    pub bytes: Vec<u8>,
    pub encryption_header: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalNodeState {
    pub node_id: NodeId,
    pub hydration_state: HydrationState,
    pub local_blob_path: Option<String>,
    pub dirty_base_revision_id: Option<RevisionId>,
    pub pinned: bool,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug)]
pub struct LocalStore {
    conn: Connection,
}

impl LocalStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.initialize_schema()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.initialize_schema()?;
        Ok(store)
    }

    pub fn initialize_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        name: &str,
        root_node_id: NodeId,
    ) -> Result<()> {
        let now = Utc::now();
        let root = Node {
            node_id: root_node_id,
            workspace_id,
            parent_id: None,
            name: String::new(),
            kind: NodeKind::Directory,
            current_rev: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            tombstone_version: None,
        };
        self.initialize_workspace_from_root(name, &root)
    }

    pub fn initialize_workspace_from_root(&mut self, name: &str, root: &Node) -> Result<()> {
        if root.parent_id.is_some() || !root.name.is_empty() || root.kind != NodeKind::Directory {
            return Err(LocalStoreError::Invalid(
                "workspace root must be a nameless directory without parent".to_owned(),
            ));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO local_workspaces
             (workspace_id, name, root_node_id, last_cursor, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![
                root.workspace_id.to_string(),
                name,
                root.node_id.to_string(),
                root.created_at,
            ],
        )?;
        insert_or_update_node(&tx, root, "")?;
        tx.execute(
            "INSERT OR IGNORE INTO local_state
             (node_id, hydration_state, pinned) VALUES (?1, ?2, 0)",
            params![
                root.node_id.to_string(),
                HydrationState::Hydrated.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_node_by_path(&self, workspace_id: WorkspaceId, path: &str) -> Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT node_json FROM local_nodes
                 WHERE workspace_id = ?1 AND path = ?2 AND deleted_at IS NULL",
                params![workspace_id.to_string(), normalize_path(path)?],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    pub fn get_node_by_id(&self, node_id: NodeId) -> Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT node_json FROM local_nodes WHERE node_id = ?1 AND deleted_at IS NULL",
                params![node_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    pub fn list_children(&self, parent_id: NodeId) -> Result<Vec<Node>> {
        let mut statement = self.conn.prepare(
            "SELECT node_json FROM local_nodes
             WHERE parent_id = ?1 AND deleted_at IS NULL ORDER BY name",
        )?;
        let rows = statement.query_map(params![parent_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn put_pending_op(&mut self, operation: &Operation) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO pending_ops
             (op_id, workspace_id, operation_json, created_at, retry_count, last_error)
             VALUES (?1, ?2, ?3, ?4, COALESCE((SELECT retry_count FROM pending_ops WHERE op_id = ?1), 0), NULL)",
            params![
                operation.op_id.to_string(),
                operation.workspace_id.to_string(),
                serde_json::to_string(operation)?,
                operation.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_pending_ops(&self, workspace_id: WorkspaceId) -> Result<Vec<PendingOperation>> {
        let mut statement = self.conn.prepare(
            "SELECT operation_json, retry_count, last_error FROM pending_ops
             WHERE workspace_id = ?1 ORDER BY created_at, op_id",
        )?;
        let rows = statement.query_map(params![workspace_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (json, retry_count, last_error) = row?;
            Ok(PendingOperation {
                operation: serde_json::from_str(&json)?,
                retry_count,
                last_error,
            })
        })
        .collect()
    }

    pub fn mark_pending_op_failed(&mut self, operation: &Operation, error: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE pending_ops
             SET retry_count = retry_count + 1, last_error = ?2
             WHERE op_id = ?1",
            params![operation.op_id.to_string(), error],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn put_pending_blob_upload(
        &mut self,
        blob_id: &BlobId,
        workspace_id: WorkspaceId,
        bytes: &[u8],
        encryption_header: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO pending_blob_uploads
             (blob_id, workspace_id, bytes, encryption_header, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob_id.to_string(),
                workspace_id.to_string(),
                bytes,
                encryption_header,
                Utc::now(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_blob_upload(&self, blob_id: &BlobId) -> Result<Option<PendingBlobUpload>> {
        self.conn
            .query_row(
                "SELECT blob_id, workspace_id, bytes, encryption_header
                 FROM pending_blob_uploads WHERE blob_id = ?1",
                params![blob_id.to_string()],
                |row| {
                    let stored_blob_id = row.get::<_, String>(0)?;
                    let workspace_id = row.get::<_, String>(1)?;
                    Ok(PendingBlobUpload {
                        blob_id: parse_blob_id(&stored_blob_id)?,
                        workspace_id: workspace_id.parse().map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        bytes: row.get(2)?,
                        encryption_header: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn remove_pending_blob_upload(&mut self, blob_id: &BlobId) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM pending_blob_uploads WHERE blob_id = ?1",
            params![blob_id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_blob_cached(
        &mut self,
        blob_id: &BlobId,
        path: &str,
        size: u64,
        verified: bool,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO blob_cache
             (blob_id, path, size, verified, last_accessed_at, pinned_ref_count)
             VALUES (?1, ?2, ?3, ?4, ?5,
                     COALESCE((SELECT pinned_ref_count FROM blob_cache WHERE blob_id = ?1), 0))",
            params![blob_id.to_string(), path, size, verified, Utc::now()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn blob_cache_entry(&self, blob_id: &BlobId) -> Result<Option<BlobCacheEntry>> {
        self.conn
            .query_row(
                "SELECT blob_id, path, size, verified, pinned_ref_count
                 FROM blob_cache WHERE blob_id = ?1",
                params![blob_id.to_string()],
                |row| {
                    let stored_blob_id = row.get::<_, String>(0)?;
                    Ok(BlobCacheEntry {
                        blob_id: parse_blob_id(&stored_blob_id)?,
                        path: row.get(1)?,
                        size: row.get(2)?,
                        verified: row.get(3)?,
                        pinned_ref_count: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_hydration_state(
        &mut self,
        node_id: NodeId,
        hydration_state: HydrationState,
        local_blob_path: Option<&str>,
        pinned: bool,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO local_state (node_id, hydration_state, local_blob_path, pinned)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(node_id) DO UPDATE SET
               hydration_state = excluded.hydration_state,
               local_blob_path = excluded.local_blob_path,
               pinned = excluded.pinned",
            params![
                node_id.to_string(),
                hydration_state.to_string(),
                local_blob_path,
                pinned,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn node_state(&self, node_id: NodeId) -> Result<Option<LocalNodeState>> {
        let row = self
            .conn
            .query_row(
                "SELECT node_id, hydration_state, local_blob_path, dirty_base_revision_id,
                        pinned, error_code, error_message
                 FROM local_state WHERE node_id = ?1",
                params![node_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, bool>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                node_id,
                hydration_state,
                local_blob_path,
                dirty_base_revision_id,
                pinned,
                error_code,
                error_message,
            )| {
                Ok(LocalNodeState {
                    node_id: parse_node_id_for_store(&node_id)?,
                    hydration_state: hydration_state.parse()?,
                    local_blob_path,
                    dirty_base_revision_id: parse_optional_revision_id_for_store(
                        dirty_base_revision_id,
                    )?,
                    pinned,
                    error_code,
                    error_message,
                })
            },
        )
        .transpose()
    }

    pub fn get_effective_rule(
        &self,
        workspace_id: WorkspaceId,
        path: &str,
    ) -> Result<Option<FsRule>> {
        let normalized = normalize_path(path)?;
        let path_kind = self
            .conn
            .query_row(
                "SELECT node_json FROM local_nodes
                 WHERE workspace_id = ?1 AND path = ?2 AND deleted_at IS NULL",
                params![workspace_id.to_string(), normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str::<Node>(&json).map(|node| node.kind))
            .transpose()?;
        let mut statement = self.conn.prepare(
            "SELECT pattern, rule_json FROM rules WHERE workspace_id = ?1 ORDER BY priority DESC, id DESC",
        )?;
        let mut rows = statement.query(params![workspace_id.to_string()])?;
        while let Some(row) = rows.next()? {
            let pattern: String = row.get(0)?;
            if rule_matches(&pattern, &normalized, path_kind)? {
                let rule_json: String = row.get(1)?;
                return serde_json::from_str(&rule_json)
                    .map(Some)
                    .map_err(Into::into);
            }
        }
        Ok(None)
    }

    pub fn last_cursor(&self, workspace_id: WorkspaceId) -> Result<Cursor> {
        let value = self.conn.query_row(
            "SELECT last_cursor FROM local_workspaces WHERE workspace_id = ?1",
            params![workspace_id.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        Cursor::new(value).map_err(|error| LocalStoreError::Invalid(error.to_string()))
    }

    pub fn apply_operation(&mut self, operation: &Operation) -> Result<Cursor> {
        let assigned_cursor = Cursor::new(
            operation
                .base_cursor
                .value()
                .checked_add(1)
                .ok_or_else(|| LocalStoreError::Invalid("cursor overflow".to_owned()))?,
        )
        .map_err(|error| LocalStoreError::Invalid(error.to_string()))?;
        self.apply_committed_operation(operation, assigned_cursor)
    }

    pub fn apply_committed_operation(
        &mut self,
        operation: &Operation,
        assigned_cursor: Cursor,
    ) -> Result<Cursor> {
        let tx = self.conn.transaction()?;
        let last_cursor = workspace_cursor(&tx, operation.workspace_id)?;
        let expected_cursor = Cursor::new(
            last_cursor
                .value()
                .checked_add(1)
                .ok_or_else(|| LocalStoreError::Invalid("cursor overflow".to_owned()))?,
        )
        .map_err(|error| LocalStoreError::Invalid(error.to_string()))?;
        if assigned_cursor != expected_cursor {
            return Err(LocalStoreError::Invalid(format!(
                "committed cursor {assigned_cursor} does not follow local cursor {last_cursor}"
            )));
        }
        apply_operation_in_tx(&tx, operation, assigned_cursor)?;
        tx.execute(
            "UPDATE local_workspaces SET last_cursor = ?2 WHERE workspace_id = ?1",
            params![operation.workspace_id.to_string(), assigned_cursor.value()],
        )?;
        tx.execute(
            "DELETE FROM pending_ops WHERE op_id = ?1",
            params![operation.op_id.to_string()],
        )?;
        tx.commit()?;
        Ok(assigned_cursor)
    }

    fn initialize_schema(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        self.migrate_schema()
    }

    fn migrate_schema(&self) -> Result<()> {
        if !column_exists(&self.conn, "pending_ops", "retry_count")? {
            self.conn.execute(
                "ALTER TABLE pending_ops ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !column_exists(&self.conn, "pending_ops", "last_error")? {
            self.conn
                .execute("ALTER TABLE pending_ops ADD COLUMN last_error TEXT", [])?;
        }
        Ok(())
    }
}

const SCHEMA: &str = r"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS local_workspaces (
    workspace_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
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
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT,
    tombstone_version INTEGER,
    node_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_local_nodes_path ON local_nodes(workspace_id, path);
CREATE UNIQUE INDEX IF NOT EXISTS idx_local_nodes_live_path
    ON local_nodes(workspace_id, path) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_local_nodes_parent ON local_nodes(parent_id, deleted_at, name);

CREATE TABLE IF NOT EXISTS local_revisions (
    revision_id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    base_revision_id TEXT,
    content_json TEXT NOT NULL,
    revision_json TEXT NOT NULL,
    posix_mode INTEGER NOT NULL,
    mtime TEXT NOT NULL,
    size INTEGER NOT NULL,
    executable INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS local_state (
    node_id TEXT PRIMARY KEY,
    hydration_state TEXT NOT NULL DEFAULT 'metadata_only',
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
    operation_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);

CREATE TABLE IF NOT EXISTS pending_blob_uploads (
    blob_id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    bytes BLOB NOT NULL,
    encryption_header TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS blob_cache (
    blob_id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    verified INTEGER NOT NULL,
    last_accessed_at TEXT NOT NULL,
    pinned_ref_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL,
    pattern TEXT NOT NULL,
    action TEXT NOT NULL,
    source TEXT NOT NULL,
    priority INTEGER NOT NULL,
    rule_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rules_workspace_priority ON rules(workspace_id, priority DESC, id DESC);

CREATE TABLE IF NOT EXISTS env_vars (
    env_var_id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    encrypted_payload TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    deleted_at TEXT,
    updated_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conflicts (
    conflict_id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    conflict_path TEXT NOT NULL,
    remote_revision_id TEXT,
    local_revision_id TEXT,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL
);
";

fn apply_operation_in_tx(
    tx: &Transaction<'_>,
    operation: &Operation,
    assigned_cursor: Cursor,
) -> Result<()> {
    match &operation.kind {
        OperationKind::CreateNode {
            node_id,
            parent_id,
            name,
            kind,
            initial_revision,
        } => apply_create_node(
            tx,
            operation,
            *node_id,
            *parent_id,
            name,
            *kind,
            initial_revision.as_ref(),
        ),
        OperationKind::PutFileRevision {
            node_id, revision, ..
        } => {
            ensure_live_node(tx, *node_id)?;
            insert_revision(tx, revision)?;
            update_node_revision(tx, *node_id, revision.revision_id, operation.created_at)
        }
        OperationKind::MoveNode {
            node_id,
            new_parent_id,
            new_name,
            ..
        } => apply_move_node(
            tx,
            operation.workspace_id,
            *node_id,
            *new_parent_id,
            new_name,
            operation.created_at,
        ),
        OperationKind::DeleteNode { node_id, .. } => {
            apply_delete_node(tx, operation.workspace_id, *node_id, operation.created_at)
        }
        OperationKind::RestoreNode {
            node_id,
            parent_id,
            name,
        } => apply_restore_node(tx, *node_id, *parent_id, name, operation.created_at),
        OperationKind::SetRule { path_pattern, rule } => {
            tx.execute(
                "INSERT INTO rules (workspace_id, pattern, action, source, priority, rule_json)
                 VALUES (?1, ?2, ?3, 'operation', ?4, ?5)",
                params![
                    operation.workspace_id.to_string(),
                    path_pattern,
                    format!("{:?}", rule.action),
                    assigned_cursor.value(),
                    serde_json::to_string(rule)?,
                ],
            )?;
            Ok(())
        }
        OperationKind::SetEnvVar {
            env_var_id,
            encrypted_payload,
            metadata,
        } => {
            tx.execute(
                "INSERT INTO env_vars
                 (env_var_id, workspace_id, encrypted_payload, metadata_json, deleted_at, updated_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)
                 ON CONFLICT(env_var_id) DO UPDATE SET
                   encrypted_payload = excluded.encrypted_payload,
                   metadata_json = excluded.metadata_json,
                   deleted_at = NULL,
                   updated_at = excluded.updated_at",
                params![
                    env_var_id.to_string(),
                    operation.workspace_id.to_string(),
                    encrypted_payload,
                    serde_json::to_string(metadata)?,
                    operation.created_at,
                ],
            )?;
            Ok(())
        }
        OperationKind::DeleteEnvVar { env_var_id } => {
            tx.execute(
                "UPDATE env_vars SET deleted_at = ?2, updated_at = ?2 WHERE env_var_id = ?1",
                params![env_var_id.to_string(), operation.created_at],
            )?;
            Ok(())
        }
    }
}

fn apply_create_node(
    tx: &Transaction<'_>,
    operation: &Operation,
    node_id: NodeId,
    parent_id: NodeId,
    name: &str,
    kind: NodeKind,
    initial_revision: Option<&NodeRevision>,
) -> Result<()> {
    ensure_live_node(tx, parent_id)?;
    let parent_path = node_path(tx, parent_id)?;
    let path = child_path(&parent_path, name)?;
    let current_rev = initial_revision.map(|revision| revision.revision_id);
    let node = Node {
        node_id,
        workspace_id: operation.workspace_id,
        parent_id: Some(parent_id),
        name: name.to_owned(),
        kind,
        current_rev,
        created_at: operation.created_at,
        updated_at: operation.created_at,
        deleted_at: None,
        tombstone_version: None,
    };
    if let Some(revision) = initial_revision {
        insert_revision(tx, revision)?;
    }
    insert_or_update_node(tx, &node, &path)?;
    tx.execute(
        "INSERT OR IGNORE INTO local_state (node_id, hydration_state, pinned) VALUES (?1, ?2, 0)",
        params![
            node_id.to_string(),
            HydrationState::MetadataOnly.to_string()
        ],
    )?;
    Ok(())
}

fn apply_move_node(
    tx: &Transaction<'_>,
    workspace_id: WorkspaceId,
    node_id: NodeId,
    new_parent_id: NodeId,
    new_name: &str,
    updated_at: DateTime<Utc>,
) -> Result<()> {
    ensure_live_node(tx, new_parent_id)?;
    let mut node = ensure_live_node(tx, node_id)?;
    let old_path = node_path(tx, node_id)?;
    let new_parent_path = node_path(tx, new_parent_id)?;
    let new_path = child_path(&new_parent_path, new_name)?;
    node.parent_id = Some(new_parent_id);
    new_name.clone_into(&mut node.name);
    node.updated_at = updated_at;
    insert_or_update_node(tx, &node, &new_path)?;
    rewrite_descendant_paths(tx, workspace_id, &old_path, &new_path)
}

fn apply_delete_node(
    tx: &Transaction<'_>,
    workspace_id: WorkspaceId,
    node_id: NodeId,
    deleted_at: DateTime<Utc>,
) -> Result<()> {
    ensure_live_node(tx, node_id)?;
    let path = node_path(tx, node_id)?;
    let tombstone_version = workspace_cursor(tx, workspace_id)?.value() + 1;
    for (node_id, _path, node_json) in matching_paths(tx, workspace_id, &path, true)? {
        let mut node = serde_json::from_str::<Node>(&node_json)?;
        node.deleted_at = Some(deleted_at);
        node.tombstone_version = Some(tombstone_version);
        tx.execute(
            "UPDATE local_nodes
             SET deleted_at = ?2, tombstone_version = ?3, node_json = ?4
             WHERE node_id = ?1",
            params![
                node_id,
                deleted_at,
                tombstone_version,
                serde_json::to_string(&node)?,
            ],
        )?;
    }
    Ok(())
}

fn apply_restore_node(
    tx: &Transaction<'_>,
    node_id: NodeId,
    parent_id: NodeId,
    name: &str,
    updated_at: DateTime<Utc>,
) -> Result<()> {
    ensure_live_node(tx, parent_id)?;
    let mut node = node_by_id_any(tx, node_id)?;
    let parent_path = node_path(tx, parent_id)?;
    let new_path = child_path(&parent_path, name)?;
    node.parent_id = Some(parent_id);
    name.clone_into(&mut node.name);
    node.deleted_at = None;
    node.tombstone_version = None;
    node.updated_at = updated_at;
    insert_or_update_node(tx, &node, &new_path)
}

fn insert_or_update_node(tx: &Transaction<'_>, node: &Node, path: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO local_nodes
         (node_id, workspace_id, parent_id, name, normalized_name, path, kind,
          current_revision_id, created_at, updated_at, deleted_at, tombstone_version, node_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(node_id) DO UPDATE SET
          parent_id = excluded.parent_id,
          name = excluded.name,
          normalized_name = excluded.normalized_name,
          path = excluded.path,
          kind = excluded.kind,
          current_revision_id = excluded.current_revision_id,
          updated_at = excluded.updated_at,
          deleted_at = excluded.deleted_at,
          tombstone_version = excluded.tombstone_version,
          node_json = excluded.node_json",
        params![
            node.node_id.to_string(),
            node.workspace_id.to_string(),
            node.parent_id.map(|id| id.to_string()),
            node.name,
            node.name.to_lowercase(),
            normalize_path(path)?,
            format!("{:?}", node.kind),
            node.current_rev.map(|id| id.to_string()),
            node.created_at,
            node.updated_at,
            node.deleted_at,
            node.tombstone_version,
            serde_json::to_string(node)?,
        ],
    )?;
    Ok(())
}

fn insert_revision(tx: &Transaction<'_>, revision: &NodeRevision) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO local_revisions
         (revision_id, node_id, workspace_id, device_id, base_revision_id, content_json,
          revision_json, posix_mode, mtime, size, executable, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            revision.revision_id.to_string(),
            revision.node_id.to_string(),
            revision.workspace_id.to_string(),
            revision.device_id.to_string(),
            revision.base_revision_id.map(|id| id.to_string()),
            serde_json::to_string(&revision.content)?,
            serde_json::to_string(revision)?,
            revision.posix_mode,
            revision.mtime,
            revision.size,
            revision.executable,
            revision.created_at,
        ],
    )?;
    Ok(())
}

fn update_node_revision(
    tx: &Transaction<'_>,
    node_id: NodeId,
    revision_id: RevisionId,
    updated_at: DateTime<Utc>,
) -> Result<()> {
    let mut node = ensure_live_node(tx, node_id)?;
    node.current_rev = Some(revision_id);
    node.updated_at = updated_at;
    let path = node_path(tx, node_id)?;
    insert_or_update_node(tx, &node, &path)
}

fn rewrite_descendant_paths(
    tx: &Transaction<'_>,
    workspace_id: WorkspaceId,
    old_path: &str,
    new_path: &str,
) -> Result<()> {
    let descendants = matching_paths(tx, workspace_id, old_path, false)?;
    for (node_id, path, node_json) in descendants {
        let node = serde_json::from_str::<Node>(&node_json)?;
        let suffix = path.strip_prefix(old_path).ok_or_else(|| {
            LocalStoreError::Invalid("descendant path prefix mismatch".to_owned())
        })?;
        let rewritten = format!("{new_path}{suffix}");
        tx.execute(
            "UPDATE local_nodes SET path = ?2, node_json = ?3 WHERE node_id = ?1",
            params![node_id, rewritten, serde_json::to_string(&node)?],
        )?;
    }
    Ok(())
}

fn matching_paths(
    tx: &Transaction<'_>,
    workspace_id: WorkspaceId,
    base_path: &str,
    include_base: bool,
) -> Result<Vec<(String, String, String)>> {
    let mut statement = tx.prepare(
        "SELECT node_id, path, node_json FROM local_nodes
         WHERE workspace_id = ?1 ORDER BY path",
    )?;
    let rows = statement.query_map(params![workspace_id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let child_prefix = format!("{base_path}/");
    let mut matches = Vec::new();
    for row in rows {
        let (node_id, path, node_json) = row?;
        if (include_base && path == base_path) || path.starts_with(&child_prefix) {
            matches.push((node_id, path, node_json));
        }
    }
    Ok(matches)
}

fn workspace_cursor(tx: &Transaction<'_>, workspace_id: WorkspaceId) -> Result<Cursor> {
    let value = tx.query_row(
        "SELECT last_cursor FROM local_workspaces WHERE workspace_id = ?1",
        params![workspace_id.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    Cursor::new(value).map_err(|error| LocalStoreError::Invalid(error.to_string()))
}

fn node_by_id_any(tx: &Transaction<'_>, node_id: NodeId) -> Result<Node> {
    tx.query_row(
        "SELECT node_json FROM local_nodes WHERE node_id = ?1",
        params![node_id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .map_err(LocalStoreError::Sqlite)
    .and_then(|json| serde_json::from_str(&json).map_err(Into::into))
}

fn ensure_live_node(tx: &Transaction<'_>, node_id: NodeId) -> Result<Node> {
    tx.query_row(
        "SELECT node_json FROM local_nodes WHERE node_id = ?1 AND deleted_at IS NULL",
        params![node_id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .map_err(LocalStoreError::Sqlite)
    .and_then(|json| serde_json::from_str(&json).map_err(Into::into))
}

fn node_path(tx: &Transaction<'_>, node_id: NodeId) -> Result<String> {
    tx.query_row(
        "SELECT path FROM local_nodes WHERE node_id = ?1",
        params![node_id.to_string()],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn child_path(parent_path: &str, name: &str) -> Result<String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err(LocalStoreError::Invalid("invalid node name".to_owned()));
    }
    if parent_path.is_empty() {
        Ok(name.to_owned())
    } else {
        Ok(format!("{parent_path}/{name}"))
    }
}

fn normalize_path(path: &str) -> Result<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.contains('\\') {
        return Err(LocalStoreError::Invalid(
            "invalid workspace path".to_owned(),
        ));
    }
    for segment in trimmed.split('/') {
        if segment == "." || segment == ".." {
            return Err(LocalStoreError::Invalid(
                "invalid workspace path".to_owned(),
            ));
        }
    }
    Ok(trimmed.to_owned())
}

fn rule_matches(pattern: &str, path: &str, path_kind: Option<NodeKind>) -> Result<bool> {
    let workspace_path =
        WorkspacePath::parse(path).map_err(|error| LocalStoreError::Invalid(error.to_string()))?;
    let rule_kind = match path_kind {
        Some(NodeKind::Directory) => RulePathKind::Directory,
        Some(NodeKind::File | NodeKind::Symlink) | None => RulePathKind::File,
    };
    rule_pattern_matches(pattern, &workspace_path, rule_kind)
        .map_err(|error| LocalStoreError::Invalid(error.to_string()))
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = conn.prepare(&pragma)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_blob_id(value: &str) -> rusqlite::Result<BlobId> {
    value
        .parse()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn parse_node_id_for_store(value: &str) -> Result<NodeId> {
    value
        .parse()
        .map_err(|error| LocalStoreError::Invalid(format!("invalid node id in store: {error}")))
}

fn parse_optional_revision_id_for_store(value: Option<String>) -> Result<Option<RevisionId>> {
    value
        .map(|revision| {
            revision.parse().map_err(|error| {
                LocalStoreError::Invalid(format!("invalid revision id in store: {error}"))
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;
    use fs2_core::{
        BlobId, DeviceId, EnvScope, EnvVarId, EnvVarMetadata, OpId, RevisionContent, SecretKind,
    };
    use tempfile::TempDir;

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-daemon");
    }

    #[test]
    fn directory_rule_patterns_respect_path_kind() -> Result<()> {
        assert!(rule_matches(
            "node_modules/",
            "node_modules",
            Some(NodeKind::Directory)
        )?);
        assert!(!rule_matches(
            "node_modules/",
            "node_modules",
            Some(NodeKind::File)
        )?);
        assert!(rule_matches(
            "node_modules/",
            "apps/api/node_modules/pkg/index.js",
            None
        )?);
        Ok(())
    }

    #[test]
    fn initializes_state_root_and_db() -> Result<()> {
        let dir = TempDir::new()?;
        let db = dir.path().join("state/fs2.sqlite");
        let mut store = LocalStore::open(&db)?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();

        store.initialize_workspace(workspace_id, "demo", root_id)?;

        assert!(db.exists());
        assert_eq!(store.last_cursor(workspace_id)?.value(), 0);
        assert_eq!(
            store
                .get_node_by_path(workspace_id, "")?
                .map(|node| node.node_id),
            Some(root_id)
        );
        Ok(())
    }

    #[test]
    fn initializes_root_from_backend_manifest_metadata() -> Result<()> {
        let mut store = LocalStore::in_memory()?;
        let created_at = Utc::now() - chrono::Duration::days(1);
        let updated_at = created_at + chrono::Duration::minutes(5);
        let root = Node {
            node_id: NodeId::new_v4(),
            workspace_id: WorkspaceId::new_v4(),
            parent_id: None,
            name: String::new(),
            kind: NodeKind::Directory,
            current_rev: None,
            created_at,
            updated_at,
            deleted_at: None,
            tombstone_version: None,
        };

        store.initialize_workspace_from_root("demo", &root)?;

        assert_eq!(store.get_node_by_path(root.workspace_id, "")?, Some(root));
        Ok(())
    }

    #[test]
    fn applies_operations_and_replays_tree_from_zero() -> Result<()> {
        let mut store = LocalStore::in_memory()?;
        let ids = Ids::new();
        store.initialize_workspace(ids.workspace, "demo", ids.root)?;

        let docs = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::CreateNode {
                node_id: ids.docs,
                parent_id: ids.root,
                name: "docs".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?;
        assert_eq!(store.apply_operation(&docs)?.value(), 1);

        let revision = file_revision(ids, None)?;
        let create_file = op(
            ids.workspace,
            ids.device,
            1,
            OperationKind::CreateNode {
                node_id: ids.file,
                parent_id: ids.docs,
                name: "readme.md".to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(revision.clone()),
            },
        )?;
        assert_eq!(store.apply_operation(&create_file)?.value(), 2);

        let children = store.list_children(ids.docs)?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "readme.md");
        assert_eq!(
            store
                .get_node_by_path(ids.workspace, "docs/readme.md")?
                .map(|node| node.current_rev),
            Some(Some(revision.revision_id))
        );

        let updated_revision = file_revision(ids, Some(revision.revision_id))?;
        let put_revision = op(
            ids.workspace,
            ids.device,
            2,
            OperationKind::PutFileRevision {
                node_id: ids.file,
                base_revision_id: Some(revision.revision_id),
                revision: updated_revision.clone(),
            },
        )?;
        assert_eq!(store.apply_operation(&put_revision)?.value(), 3);
        assert_eq!(
            store
                .get_node_by_id(ids.file)?
                .and_then(|node| node.current_rev),
            Some(updated_revision.revision_id)
        );

        let move_file = op(
            ids.workspace,
            ids.device,
            3,
            OperationKind::MoveNode {
                node_id: ids.file,
                old_parent_id: ids.docs,
                old_name: "readme.md".to_owned(),
                new_parent_id: ids.root,
                new_name: "README.md".to_owned(),
            },
        )?;
        assert_eq!(store.apply_operation(&move_file)?.value(), 4);
        assert!(store
            .get_node_by_path(ids.workspace, "docs/readme.md")?
            .is_none());
        assert!(store
            .get_node_by_path(ids.workspace, "README.md")?
            .is_some());
        Ok(())
    }

    #[test]
    fn metadata_helpers_are_transactional() -> Result<()> {
        let mut store = LocalStore::in_memory()?;
        let ids = Ids::new();
        store.initialize_workspace(ids.workspace, "demo", ids.root)?;
        let pending = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::DeleteNode {
                node_id: ids.root,
                recursive: true,
            },
        )?;
        store.put_pending_op(&pending)?;
        assert_eq!(store.list_pending_ops(ids.workspace)?.len(), 1);

        let blob_id = BlobId::new("sha256:abc".to_owned())?;
        store.mark_blob_cached(&blob_id, "objects/abc", 42, true)?;
        assert_eq!(
            store.blob_cache_entry(&blob_id)?.map(|entry| entry.size),
            Some(42)
        );

        store.set_hydration_state(ids.root, HydrationState::Hydrated, Some("root"), true)?;
        assert_eq!(
            store
                .node_state(ids.root)?
                .map(|state| state.hydration_state),
            Some(HydrationState::Hydrated)
        );
        Ok(())
    }

    #[test]
    fn migrates_legacy_pending_ops_retry_columns() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("legacy.sqlite");
        let ids = Ids::new();
        let pending = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::DeleteNode {
                node_id: ids.root,
                recursive: true,
            },
        )?;
        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "CREATE TABLE pending_ops (
                    op_id TEXT PRIMARY KEY,
                    workspace_id TEXT NOT NULL,
                    operation_json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );",
            )?;
            conn.execute(
                "INSERT INTO pending_ops (op_id, workspace_id, operation_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    pending.op_id.to_string(),
                    ids.workspace.to_string(),
                    serde_json::to_string(&pending)?,
                    pending.created_at.to_rfc3339(),
                ],
            )?;
        }

        let mut store = LocalStore::open(&path)?;
        let listed = store.list_pending_ops(ids.workspace)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].retry_count, 0);
        assert_eq!(listed[0].last_error, None);

        store.mark_pending_op_failed(&pending, "after migration")?;
        let listed = store.list_pending_ops(ids.workspace)?;
        assert_eq!(listed[0].retry_count, 1);
        assert_eq!(listed[0].last_error.as_deref(), Some("after migration"));
        Ok(())
    }

    #[test]
    fn applies_rules_env_delete_restore_and_keeps_cursor_on_failure() -> Result<()> {
        let mut store = LocalStore::in_memory()?;
        let ids = Ids::new();
        store.initialize_workspace(ids.workspace, "demo", ids.root)?;

        let generated_rule = FsRule {
            action: fs2_core::RuleAction::Generated,
            manager: Some("node".to_owned()),
            scope: None,
        };
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::SetRule {
                path_pattern: "node_modules/".to_owned(),
                rule: generated_rule.clone(),
            },
        )?)?;
        assert_eq!(
            store.get_effective_rule(ids.workspace, "node_modules/pkg/index.js")?,
            Some(generated_rule.clone())
        );
        assert_eq!(
            store.get_effective_rule(ids.workspace, "apps/api/node_modules/pkg/index.js")?,
            Some(generated_rule)
        );

        let env_var_id = EnvVarId::new_v4();
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            1,
            OperationKind::SetEnvVar {
                env_var_id,
                encrypted_payload: "ciphertext".to_owned(),
                metadata: EnvVarMetadata {
                    env_name: "API_KEY".to_owned(),
                    environment: "dev".to_owned(),
                    scope: EnvScope::Workspace,
                    secret_kind: SecretKind::Secret,
                },
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            2,
            OperationKind::DeleteEnvVar { env_var_id },
        )?)?;

        let dir = NodeId::new_v4();
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            3,
            OperationKind::CreateNode {
                node_id: dir,
                parent_id: ids.root,
                name: "tmp".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            4,
            OperationKind::DeleteNode {
                node_id: dir,
                recursive: true,
            },
        )?)?;
        assert!(store.get_node_by_path(ids.workspace, "tmp")?.is_none());
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            5,
            OperationKind::RestoreNode {
                node_id: dir,
                parent_id: ids.root,
                name: "tmp".to_owned(),
            },
        )?)?;
        assert!(store.get_node_by_path(ids.workspace, "tmp")?.is_some());

        let bad = op(
            ids.workspace,
            ids.device,
            6,
            OperationKind::CreateNode {
                node_id: NodeId::new_v4(),
                parent_id: NodeId::new_v4(),
                name: "lost".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?;
        assert!(store.apply_operation(&bad).is_err());
        assert_eq!(store.last_cursor(ids.workspace)?.value(), 6);
        Ok(())
    }

    #[test]
    fn committed_replay_uses_assigned_cursor_not_request_base() -> Result<()> {
        let mut store = LocalStore::in_memory()?;
        let ids = Ids::new();
        store.initialize_workspace(ids.workspace, "demo", ids.root)?;

        let first = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::CreateNode {
                node_id: NodeId::new_v4(),
                parent_id: ids.root,
                name: "a".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?;
        let second = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::CreateNode {
                node_id: NodeId::new_v4(),
                parent_id: ids.root,
                name: "b".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?;

        store.apply_committed_operation(&first, Cursor::new(1)?)?;
        store.apply_committed_operation(&second, Cursor::new(2)?)?;

        let older_rule = FsRule {
            action: fs2_core::RuleAction::Generated,
            manager: None,
            scope: None,
        };
        let newer_rule = FsRule {
            action: fs2_core::RuleAction::Secret,
            manager: None,
            scope: None,
        };
        let first_rule = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::SetRule {
                path_pattern: "apps/*/.env".to_owned(),
                rule: older_rule,
            },
        )?;
        let second_rule = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::SetRule {
                path_pattern: "apps/*/.env".to_owned(),
                rule: newer_rule.clone(),
            },
        )?;
        store.apply_committed_operation(&first_rule, Cursor::new(3)?)?;
        store.apply_committed_operation(&second_rule, Cursor::new(4)?)?;
        assert_eq!(store.last_cursor(ids.workspace)?.value(), 4);
        assert!(store.get_node_by_path(ids.workspace, "b")?.is_some());
        assert_eq!(
            store.get_effective_rule(ids.workspace, "apps/api/.env")?,
            Some(newer_rule)
        );
        let basename_rule = FsRule {
            action: fs2_core::RuleAction::LocalOnly,
            manager: None,
            scope: None,
        };
        let basename_rule_op = op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::SetRule {
                path_pattern: ".env".to_owned(),
                rule: basename_rule.clone(),
            },
        )?;
        store.apply_committed_operation(&basename_rule_op, Cursor::new(5)?)?;
        assert_eq!(
            store.get_effective_rule(ids.workspace, "apps/web/.env")?,
            Some(basename_rule)
        );
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn replay_allows_tombstoned_path_reuse_and_embedded_dots() -> Result<()> {
        let mut store = LocalStore::in_memory()?;
        let ids = Ids::new();
        store.initialize_workspace(ids.workspace, "demo", ids.root)?;
        let first_foo = NodeId::new_v4();
        let second_foo = NodeId::new_v4();

        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            0,
            OperationKind::CreateNode {
                node_id: first_foo,
                parent_id: ids.root,
                name: "foo".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            1,
            OperationKind::DeleteNode {
                node_id: first_foo,
                recursive: true,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            2,
            OperationKind::CreateNode {
                node_id: second_foo,
                parent_id: ids.root,
                name: "foo".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            3,
            OperationKind::CreateNode {
                node_id: NodeId::new_v4(),
                parent_id: ids.root,
                name: "foo..bar".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;

        let a_b = NodeId::new_v4();
        let axb = NodeId::new_v4();
        let axb_child = NodeId::new_v4();
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            4,
            OperationKind::CreateNode {
                node_id: a_b,
                parent_id: ids.root,
                name: "a_b".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            5,
            OperationKind::CreateNode {
                node_id: axb,
                parent_id: ids.root,
                name: "axb".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            6,
            OperationKind::CreateNode {
                node_id: axb_child,
                parent_id: axb,
                name: "child".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
        )?)?;
        store.apply_operation(&op(
            ids.workspace,
            ids.device,
            7,
            OperationKind::DeleteNode {
                node_id: a_b,
                recursive: true,
            },
        )?)?;
        assert_eq!(
            store
                .get_node_by_path(ids.workspace, "foo")?
                .map(|node| node.node_id),
            Some(second_foo)
        );
        assert!(store.get_node_by_path(ids.workspace, "foo..bar")?.is_some());
        assert!(store
            .get_node_by_path(ids.workspace, "axb/child")?
            .is_some());
        Ok(())
    }
    #[derive(Debug, Clone, Copy)]
    struct Ids {
        workspace: WorkspaceId,
        device: DeviceId,
        root: NodeId,
        docs: NodeId,
        file: NodeId,
    }

    impl Ids {
        fn new() -> Self {
            Self {
                workspace: WorkspaceId::new_v4(),
                device: DeviceId::new_v4(),
                root: NodeId::new_v4(),
                docs: NodeId::new_v4(),
                file: NodeId::new_v4(),
            }
        }
    }

    fn op(
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        base_cursor: i64,
        kind: OperationKind,
    ) -> Result<Operation> {
        Ok(Operation {
            op_id: OpId::new_v4(),
            workspace_id,
            device_id,
            base_cursor: Cursor::new(base_cursor)
                .map_err(|error| LocalStoreError::Invalid(error.to_string()))?,
            kind,
            created_at: Utc::now(),
        })
    }

    fn file_revision(ids: Ids, base_revision_id: Option<RevisionId>) -> Result<NodeRevision> {
        Ok(NodeRevision {
            revision_id: RevisionId::new_v4(),
            node_id: ids.file,
            workspace_id: ids.workspace,
            device_id: ids.device,
            base_revision_id,
            content: RevisionContent::File {
                blob_id: BlobId::new("sha256:abc".to_owned())?,
                chunk_ids: Vec::new(),
                content_hash: "sha256:abc".to_owned(),
                encryption_header: Some("header".to_owned()),
            },
            posix_mode: 0o100_644,
            mtime: Utc::now(),
            size: 5,
            executable: false,
            created_at: Utc::now(),
        })
    }
}
