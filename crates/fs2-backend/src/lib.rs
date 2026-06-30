#![allow(
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate
)]
//! Backend HTTP server skeleton with development-only auth/device endpoints.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bytes::Bytes;
use chrono::Utc;
use fs2_core::{
    names_collide, CasePolicy, Cursor, DeviceId, EnvVarId, EnvVarMetadata, FsRule, Node, NodeId,
    NodeKind, NodeName, NodeRevision, OpId, Operation, OperationKind, RevisionContent, RevisionId,
    UserId, WorkspaceId, WorkspacePath,
};
use fs2_core::{ErrorEnvelope, Fs2Error};
use hmac::{Hmac, Mac};
use object_store::{aws::AmazonS3Builder, path::Path as ObjectStorePath, ObjectStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    env, fmt,
    future::Future,
    net::{AddrParseError, SocketAddr},
    path::{Path as FsPath, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;
use tokio::{
    net::TcpListener,
    sync::{broadcast, RwLock},
};
use tracing::info;
use tracing_subscriber::{fmt as tracing_fmt, EnvFilter};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:3000";
pub const DEFAULT_DATABASE_URL: &str = "postgres://fs2:fs2@localhost:5432/fs2";
pub const DEFAULT_OBJECT_STORE: &str = "local:./.fs2-dev/blobs";
const DEFAULT_DEV_SECRET: &str = "dev-only-secret-change-before-production";
static POSTGRES_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations/postgres");

const DEV_AUTH_WARNING: &str = "development-only auth; not for production";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendConfig {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub jwt_secret: RedactedSecret,
    pub session_secret: RedactedSecret,
}

impl BackendConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    pub fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let bind_addr = lookup("FS2_BIND_ADDR")
            .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned())
            .parse()
            .map_err(ConfigError::BindAddress)?;
        let database_url =
            lookup("DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned());
        let object_store = ObjectStoreConfig::from_lookup(
            lookup("FS2_OBJECT_STORE")
                .as_deref()
                .unwrap_or(DEFAULT_OBJECT_STORE),
            &mut lookup,
        )?;
        let jwt_secret = RedactedSecret::new(
            lookup("FS2_JWT_SECRET").unwrap_or_else(|| DEFAULT_DEV_SECRET.to_owned()),
        )?;
        let session_secret = RedactedSecret::new(
            lookup("FS2_SESSION_SECRET").unwrap_or_else(|| DEFAULT_DEV_SECRET.to_owned()),
        )?;
        Ok(Self {
            bind_addr,
            database_url,
            object_store,
            jwt_secret,
            session_secret,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectStoreConfig {
    Local {
        root: PathBuf,
    },
    S3 {
        bucket: String,
        endpoint: String,
        region: String,
        access_key_id: RedactedSecret,
        secret_access_key: RedactedSecret,
        session_token: Option<RedactedSecret>,
        allow_http: bool,
        virtual_hosted_style: bool,
    },
}

impl ObjectStoreConfig {
    fn from_lookup(
        value: &str,
        lookup: &mut impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        if let Some(root) = value.strip_prefix("local:") {
            if root.is_empty() {
                return Err(ConfigError::ObjectStore(
                    "local object store root must not be empty".to_owned(),
                ));
            }
            return Ok(Self::Local {
                root: PathBuf::from(root),
            });
        }

        if let Some(rest) = value.strip_prefix("s3:") {
            let (bucket, endpoint) = rest.split_once('@').ok_or_else(|| {
                ConfigError::ObjectStore(
                    "S3 object store must be s3:<bucket>@<endpoint>".to_owned(),
                )
            })?;
            if bucket.is_empty() || endpoint.is_empty() {
                return Err(ConfigError::ObjectStore(
                    "S3 object store bucket and endpoint must not be empty".to_owned(),
                ));
            }
            let access_key_id = required_secret(lookup, "FS2_S3_ACCESS_KEY_ID")?;
            let secret_access_key = required_secret(lookup, "FS2_S3_SECRET_ACCESS_KEY")?;
            let session_token = lookup("FS2_S3_SESSION_TOKEN")
                .map(RedactedSecret::new)
                .transpose()?;
            return Ok(Self::S3 {
                bucket: bucket.to_owned(),
                endpoint: endpoint.to_owned(),
                region: lookup("FS2_S3_REGION").unwrap_or_else(|| "us-east-1".to_owned()),
                access_key_id,
                secret_access_key,
                session_token,
                allow_http: parse_config_bool(
                    lookup("FS2_S3_ALLOW_HTTP").as_deref(),
                    "FS2_S3_ALLOW_HTTP",
                )?,
                virtual_hosted_style: parse_config_bool(
                    lookup("FS2_S3_VIRTUAL_HOSTED_STYLE").as_deref(),
                    "FS2_S3_VIRTUAL_HOSTED_STYLE",
                )?,
            });
        }

        Err(ConfigError::ObjectStore(
            "object store must start with local: or s3:".to_owned(),
        ))
    }
}

fn required_secret(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    key: &str,
) -> Result<RedactedSecret, ConfigError> {
    RedactedSecret::new(lookup(key).ok_or_else(|| {
        ConfigError::ObjectStore(format!("{key} is required for S3 object store"))
    })?)
}

fn parse_config_bool(value: Option<&str>, key: &str) -> Result<bool, ConfigError> {
    match value {
        Some("true" | "1" | "yes" | "on") => Ok(true),
        None | Some("false" | "0" | "no" | "off") => Ok(false),
        Some(_) => Err(ConfigError::ObjectStore(format!(
            "{key} must be a boolean value"
        ))),
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RedactedSecret(String);

impl RedactedSecret {
    fn new(value: String) -> Result<Self, ConfigError> {
        if value.is_empty() {
            return Err(ConfigError::Secret("secret must not be empty".to_owned()));
        }
        Ok(Self(value))
    }

    pub fn expose_for_signing(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug)]
pub enum ConfigError {
    BindAddress(AddrParseError),
    ObjectStore(String),
    Secret(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BindAddress(error) => write!(formatter, "invalid bind address: {error}"),
            Self::ObjectStore(message) | Self::Secret(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

pub type BlobStoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait BlobStore: fmt::Debug + Send + Sync {
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: Bytes,
    ) -> BlobStoreFuture<'a, Result<(), BlobStoreError>>;

    fn get<'a>(&'a self, key: &'a str) -> BlobStoreFuture<'a, Result<Bytes, BlobStoreError>>;

    fn exists<'a>(&'a self, key: &'a str) -> BlobStoreFuture<'a, Result<bool, BlobStoreError>>;
}

#[derive(Debug)]
pub enum BlobStoreError {
    InvalidKey(String),
    Io(std::io::Error),
    ObjectStore(String),
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "blob store I/O error: {error}"),
            Self::ObjectStore(message) => write!(formatter, "blob object-store error: {message}"),
        }
    }
}

impl std::error::Error for BlobStoreError {}

impl From<std::io::Error> for BlobStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone)]
pub struct LocalFilesystemBlobStore {
    root: PathBuf,
}

impl LocalFilesystemBlobStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> Result<PathBuf, BlobStoreError> {
        validate_blob_key(key)?;
        Ok(self.root.join(key))
    }
}

impl BlobStore for LocalFilesystemBlobStore {
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: Bytes,
    ) -> BlobStoreFuture<'a, Result<(), BlobStoreError>> {
        Box::pin(async move {
            let path = self.path_for(key)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let temporary = create_temporary_blob(&path, bytes).await?;
            tokio::fs::rename(temporary, path).await?;
            Ok(())
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BlobStoreFuture<'a, Result<Bytes, BlobStoreError>> {
        Box::pin(async move { Ok(Bytes::from(tokio::fs::read(self.path_for(key)?).await?)) })
    }

    fn exists<'a>(&'a self, key: &'a str) -> BlobStoreFuture<'a, Result<bool, BlobStoreError>> {
        Box::pin(async move {
            match tokio::fs::metadata(self.path_for(key)?).await {
                Ok(metadata) => Ok(metadata.is_file()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(BlobStoreError::Io(error)),
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct ObjectStoreBlobStore {
    store: Arc<dyn ObjectStore>,
}

impl ObjectStoreBlobStore {
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    fn path_for(key: &str) -> Result<ObjectStorePath, BlobStoreError> {
        validate_blob_key(key)?;
        Ok(ObjectStorePath::from(key))
    }
}

impl BlobStore for ObjectStoreBlobStore {
    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: Bytes,
    ) -> BlobStoreFuture<'a, Result<(), BlobStoreError>> {
        Box::pin(async move {
            self.store
                .put(&Self::path_for(key)?, bytes.into())
                .await
                .map_err(object_store_error)?;
            Ok(())
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BlobStoreFuture<'a, Result<Bytes, BlobStoreError>> {
        Box::pin(async move {
            self.store
                .get(&Self::path_for(key)?)
                .await
                .map_err(object_store_error)?
                .bytes()
                .await
                .map_err(object_store_error)
        })
    }

    fn exists<'a>(&'a self, key: &'a str) -> BlobStoreFuture<'a, Result<bool, BlobStoreError>> {
        Box::pin(async move {
            match self.store.head(&Self::path_for(key)?).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::NotFound { .. }) => Ok(false),
                Err(error) => Err(object_store_error(error)),
            }
        })
    }
}

fn object_store_error(error: object_store::Error) -> BlobStoreError {
    match error {
        object_store::Error::NotFound { .. } => BlobStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "blob object not found",
        )),
        error => BlobStoreError::ObjectStore(error.to_string()),
    }
}

async fn create_temporary_blob(path: &FsPath, bytes: Bytes) -> Result<PathBuf, BlobStoreError> {
    let parent = path.parent().unwrap_or_else(|| FsPath::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            BlobStoreError::InvalidKey("blob key must end in UTF-8 file name".to_owned())
        })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BlobStoreError::InvalidKey(error.to_string()))?
        .as_nanos();
    for attempt in 0..16_u8 {
        let temporary = parent.join(format!(
            ".fs2-upload-{name}-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
        {
            Ok(mut file) => {
                file.write_all(&bytes).await?;
                return Ok(temporary);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(BlobStoreError::Io(error)),
        }
    }
    Err(BlobStoreError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate temporary blob path",
    )))
}

fn validate_blob_key(key: &str) -> Result<(), BlobStoreError> {
    if key.is_empty() {
        return Err(BlobStoreError::InvalidKey(
            "blob key must not be empty".to_owned(),
        ));
    }
    let path = FsPath::new(key);
    if path.is_absolute() {
        return Err(BlobStoreError::InvalidKey(
            "blob key must be relative".to_owned(),
        ));
    }
    if key.contains('\\') {
        return Err(BlobStoreError::InvalidKey(
            "blob key must use forward slashes".to_owned(),
        ));
    }
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(BlobStoreError::InvalidKey(
                "blob key must not contain empty or traversal segments".to_owned(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AppState {
    jwt_secret: RedactedSecret,
    dev_user_id: UserId,
    devices: Arc<RwLock<HashMap<DeviceId, DeviceRecord>>>,
    blob_store: Arc<dyn BlobStore>,
    blob_metadata: Arc<RwLock<HashMap<String, BlobMetadata>>>,
    workspaces: Arc<RwLock<HashMap<WorkspaceId, WorkspaceRecord>>>,
    /// Committed operations per workspace, sorted ascending by assigned cursor.
    operations: Arc<RwLock<HashMap<WorkspaceId, Vec<CommittedOperation>>>>,
    /// Idempotency index mapping `(workspace_id, op_id)` to the assigned cursor.
    idempotency: Arc<RwLock<HashMap<(WorkspaceId, OpId), Cursor>>>,
    event_tx: broadcast::Sender<WorkspaceEvent>,
}

impl AppState {
    pub fn dev(jwt_secret: RedactedSecret) -> Self {
        Self::dev_with_blob_root(jwt_secret, PathBuf::from(".fs2-dev/blobs"))
    }

    pub fn dev_with_blob_root(jwt_secret: RedactedSecret, blob_root: impl Into<PathBuf>) -> Self {
        Self::dev_with_blob_store(
            jwt_secret,
            Arc::new(LocalFilesystemBlobStore::new(blob_root)),
        )
    }

    pub fn dev_with_blob_store(jwt_secret: RedactedSecret, blob_store: Arc<dyn BlobStore>) -> Self {
        let (event_tx, _event_rx) = broadcast::channel(1024);
        Self {
            jwt_secret,
            dev_user_id: UserId::new_v4(),
            devices: Arc::new(RwLock::new(HashMap::new())),
            blob_store,
            blob_metadata: Arc::new(RwLock::new(HashMap::new())),
            workspaces: Arc::new(RwLock::new(HashMap::new())),
            operations: Arc::new(RwLock::new(HashMap::new())),
            idempotency: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub user_id: UserId,
    pub name: String,
    pub platform: serde_json::Value,
    pub public_key: String,
    pub revoked: bool,
}

#[derive(Debug, Deserialize)]
pub struct DevLoginRequest {
    pub device_name: String,
    #[serde(default)]
    pub platform: serde_json::Value,
    pub public_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DevLoginResponse {
    pub access_token: String,
    pub token_type: String,
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub warning: String,
}

#[derive(Debug, Deserialize)]
pub struct EnrollDeviceRequest {
    pub name: String,
    #[serde(default)]
    pub platform: serde_json::Value,
    pub public_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnrollDeviceResponse {
    pub device_id: DeviceId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceListResponse {
    pub devices: Vec<DeviceRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMetadata {
    pub blob_id: String,
    pub size: u64,
    pub encryption_header: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub name: String,
    pub root_node_id: NodeId,
    pub current_cursor: Cursor,
    pub root_node: Node,
    /// All live and tombstoned nodes in the workspace keyed by `node_id`.
    #[serde(default)]
    pub nodes: HashMap<NodeId, Node>,
    /// Revisions keyed by `revision_id`, used for conflict checks and replay.
    #[serde(default)]
    pub revisions: HashMap<RevisionId, NodeRevision>,
    /// Workspace rule state keyed by path pattern.
    #[serde(default)]
    pub rules: HashMap<String, FsRule>,
    /// Encrypted environment records keyed by env var id.
    #[serde(default)]
    pub env_vars: HashMap<EnvVarId, EnvRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvRecord {
    pub env_var_id: EnvVarId,
    pub encrypted_payload: String,
    pub metadata: EnvVarMetadata,
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateWorkspaceResponse {
    pub workspace_id: WorkspaceId,
    pub root_node_id: NodeId,
    pub current_cursor: Cursor,
}

/// An operation committed to the workspace log with its assigned cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedOperation {
    pub operation: Operation,
    pub cursor: Cursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    WorkspaceOpsAvailable {
        workspace_id: WorkspaceId,
        from_cursor: Cursor,
        to_cursor: Cursor,
    },
}

#[derive(Debug, Deserialize)]
pub struct CommitOperationRequest {
    pub op_id: OpId,
    pub base_cursor: Cursor,
    pub kind: OperationKind,
    #[serde(default = "default_operation_created_at")]
    pub created_at: chrono::DateTime<Utc>,
}

fn default_operation_created_at() -> chrono::DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommitOperationResponse {
    pub workspace_id: WorkspaceId,
    pub op_id: OpId,
    pub cursor: Cursor,
    pub committed: CommittedOperation,
}

#[derive(Debug, Deserialize)]
pub struct FetchOpsQuery {
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestQuery {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub depth: u32,
    pub node: Node,
    pub current_revision: Option<NodeRevision>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub workspace_id: WorkspaceId,
    pub nodes: Vec<ManifestEntry>,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FetchOpsResponse {
    pub workspace_id: WorkspaceId,
    pub operations: Vec<CommittedOperation>,
    pub has_more: bool,
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Deserialize)]
pub struct DevBlobUploadRequest {
    pub blob_id: String,
    pub bytes_base64: String,
    pub size: u64,
    pub encryption_header: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DevBlobUploadResponse {
    pub blob_id: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
pub struct DevBlobDownloadRequest {
    pub blob_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DevBlobDownloadResponse {
    pub blob_id: String,
    pub bytes_base64: String,
    pub size: u64,
    pub encryption_header: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlobStatusResponse {
    pub blob_id: String,
    pub exists: bool,
    pub size: Option<u64>,
    pub encryption_header: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    sub: UserId,
    device_id: DeviceId,
    exp: u64,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedDevice {
    pub user_id: UserId,
    pub device_id: DeviceId,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized(&'static str),
    DeviceRevoked,
    InvalidRequest(&'static str),
    BlobMissing,
    /// Stable structured error backed by the shared `Fs2Error` code table.
    Structured {
        error: Fs2Error,
        details: serde_json::Map<String, serde_json::Value>,
    },
    Internal(String),
}

impl ApiError {
    /// Builds a structured error with no extra details.
    fn structured(error: Fs2Error) -> Self {
        Self::Structured {
            error,
            details: serde_json::Map::new(),
        }
    }

    /// Builds a structured error carrying a single string detail field.
    fn structured_with(error: Fs2Error, field: &str, value: impl Into<String>) -> Self {
        let mut details = serde_json::Map::new();
        details.insert(field.to_owned(), serde_json::Value::String(value.into()));
        Self::Structured { error, details }
    }
}

impl From<Fs2Error> for ApiError {
    fn from(error: Fs2Error) -> Self {
        Self::structured(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Structured { error, details } => {
                let status = StatusCode::from_u16(error.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let envelope = ErrorEnvelope {
                    error: fs2_core::ErrorBody {
                        code: error,
                        message: error.default_message().to_owned(),
                        details,
                    },
                };
                (status, Json(envelope)).into_response()
            }
            Self::Unauthorized(message) => {
                structured_legacy(StatusCode::UNAUTHORIZED, "unauthorized", message.to_owned())
            }
            Self::DeviceRevoked => structured_legacy(
                StatusCode::UNAUTHORIZED,
                "device_revoked",
                "device token has been revoked".to_owned(),
            ),
            Self::InvalidRequest(message) => structured_legacy(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                message.to_owned(),
            ),
            Self::BlobMissing => structured_legacy(
                StatusCode::NOT_FOUND,
                "blob_missing",
                "blob does not exist".to_owned(),
            ),
            Self::Internal(message) => {
                structured_legacy(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
            }
        }
    }
}

fn structured_legacy(status: StatusCode, code: &'static str, message: String) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

pub fn app() -> Router {
    let state = AppState::dev(
        RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())
            .unwrap_or_else(|_| RedactedSecret("fallback-development-secret".to_owned())),
    );
    app_with_state(state)
}

pub fn app_with_state(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/auth/dev-login", post(dev_login))
        .route("/v1/auth/whoami", get(whoami))
        .route("/v1/devices", get(list_devices).post(enroll_device))
        .route("/v1/devices/:device_id/revoke", post(revoke_device))
        .route("/v1/workspaces", post(create_workspace))
        .route(
            "/v1/workspaces/:workspace_id/ops",
            get(fetch_operations).post(commit_operation),
        )
        .route("/v1/workspaces/:workspace_id/manifest", get(fetch_manifest))
        .route(
            "/v1/workspaces/:workspace_id/events/ws",
            get(workspace_events_ws),
        )
        .route("/v1/blobs/dev-upload", post(dev_blob_upload))
        .route("/v1/blobs/dev-download", post(dev_blob_download))
        .route("/v1/blobs/:blob_id/status", get(blob_status))
        .with_state(state)
}

pub async fn migrate_database(
    database_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?;
    POSTGRES_MIGRATOR.run(&pool).await?;
    pool.close().await;
    Ok(())
}

pub async fn serve(
    config: BackendConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    migrate_database(&config.database_url)
        .await
        .map_err(std::io::Error::other)?;
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(bind_addr = %config.bind_addr, "fs2-backend listening");
    let blob_store = blob_store_for_config(&config.object_store).map_err(std::io::Error::other)?;
    let state = AppState::dev_with_blob_store(config.jwt_secret, blob_store);
    axum::serve(listener, app_with_state(state))
        .with_graceful_shutdown(shutdown)
        .await
}

pub fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fs2_backend=info"));
    let _ = tracing_fmt().with_env_filter(filter).try_init();
}

async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "OK")
}

async fn dev_login(
    State(state): State<AppState>,
    Json(request): Json<DevLoginRequest>,
) -> Result<Json<DevLoginResponse>, ApiError> {
    validate_device_fields(&request.device_name, &request.public_key)?;
    let device = DeviceRecord {
        device_id: DeviceId::new_v4(),
        user_id: state.dev_user_id,
        name: request.device_name,
        platform: request.platform,
        public_key: request.public_key,
        revoked: false,
    };
    state
        .devices
        .write()
        .await
        .insert(device.device_id, device.clone());
    let access_token = encode_access_token(device.user_id, device.device_id, &state.jwt_secret)?;
    Ok(Json(DevLoginResponse {
        access_token,
        token_type: "Bearer".to_owned(),
        user_id: device.user_id,
        device_id: device.device_id,
        warning: DEV_AUTH_WARNING.to_owned(),
    }))
}

async fn whoami(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthenticatedDeviceResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    Ok(Json(AuthenticatedDeviceResponse {
        user_id: auth.user_id,
        device_id: auth.device_id,
    }))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthenticatedDeviceResponse {
    pub user_id: UserId,
    pub device_id: DeviceId,
}

async fn enroll_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EnrollDeviceRequest>,
) -> Result<Json<EnrollDeviceResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    validate_device_fields(&request.name, &request.public_key)?;
    let device = DeviceRecord {
        device_id: DeviceId::new_v4(),
        user_id: auth.user_id,
        name: request.name,
        platform: request.platform,
        public_key: request.public_key,
        revoked: false,
    };
    let device_id = device.device_id;
    state.devices.write().await.insert(device_id, device);
    Ok(Json(EnrollDeviceResponse { device_id }))
}

async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeviceListResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    let mut devices = state
        .devices
        .read()
        .await
        .values()
        .filter(|device| device.user_id == auth.user_id)
        .cloned()
        .collect::<Vec<_>>();
    devices.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Json(DeviceListResponse { devices }))
}

async fn revoke_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    let device_id = DeviceId::from_str(&device_id)
        .map_err(|_| ApiError::InvalidRequest("device_id must be a UUID"))?;
    {
        let mut devices = state.devices.write().await;
        let device = devices
            .get_mut(&device_id)
            .ok_or(ApiError::InvalidRequest("device not found"))?;
        if device.user_id != auth.user_id {
            return Err(ApiError::Unauthorized(
                "cannot revoke a device owned by another user",
            ));
        }
        device.revoked = true;
        drop(devices);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn create_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<Json<CreateWorkspaceResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    validate_workspace_name(&request.name)?;

    let workspace_id = WorkspaceId::new_v4();
    let root_node_id = NodeId::new_v4();
    let current_cursor = Cursor::new(0).map_err(|error| ApiError::Internal(error.to_string()))?;
    let now = Utc::now();
    let root_node = Node {
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
    let mut nodes = HashMap::new();
    nodes.insert(root_node_id, root_node.clone());
    let record = WorkspaceRecord {
        workspace_id,
        user_id: auth.user_id,
        name: request.name,
        root_node_id,
        current_cursor,
        root_node,
        nodes,
        revisions: HashMap::new(),
        rules: HashMap::new(),
        env_vars: HashMap::new(),
    };

    state.workspaces.write().await.insert(workspace_id, record);

    Ok(Json(CreateWorkspaceResponse {
        workspace_id,
        root_node_id,
        current_cursor,
    }))
}

async fn commit_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<WorkspaceId>,
    Json(request): Json<CommitOperationRequest>,
) -> Result<Json<CommitOperationResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    let operation = Operation {
        op_id: request.op_id,
        workspace_id,
        device_id: auth.device_id,
        base_cursor: request.base_cursor,
        kind: request.kind,
        created_at: request.created_at,
    };
    operation
        .validate_shape()
        .map_err(shape_error_into_api_error)?;

    // Snapshot known blob ids so the blob-existence check runs without holding the
    // blob metadata lock while we mutate the workspace tree. Lock order stays
    // idempotency -> workspaces -> operations; blob_metadata is read beforehand.
    let known_blobs: std::collections::HashSet<String> =
        state.blob_metadata.read().await.keys().cloned().collect();
    let blob_exists = |blob_id: &str| known_blobs.contains(blob_id);

    // Lock order: idempotency -> workspaces -> operations (acquired consistently).
    let mut idempotency = state.idempotency.write().await;
    if let Some(existing_cursor) = idempotency.get(&(workspace_id, operation.op_id)) {
        let workspaces = state.workspaces.read().await;
        let workspace = workspaces
            .get(&workspace_id)
            .ok_or_else(|| ApiError::structured(Fs2Error::WorkspaceNotFound))?;
        let owns_workspace = workspace.user_id == auth.user_id;
        drop(workspaces);
        if !owns_workspace {
            return Err(ApiError::Unauthorized("device does not own this workspace"));
        }
        let operations = state.operations.read().await;
        let committed = operations
            .get(&workspace_id)
            .and_then(|log| {
                log.iter()
                    .find(|committed| {
                        committed.cursor == *existing_cursor
                            && committed.operation.op_id == operation.op_id
                    })
                    .cloned()
            })
            .ok_or_else(|| ApiError::Internal("idempotency log entry missing".to_owned()))?;
        drop(operations);
        return Ok(Json(CommitOperationResponse {
            workspace_id,
            op_id: operation.op_id,
            cursor: *existing_cursor,
            committed,
        }));
    }

    let mut workspaces = state.workspaces.write().await;
    let workspace = workspaces
        .get_mut(&workspace_id)
        .ok_or_else(|| ApiError::structured(Fs2Error::WorkspaceNotFound))?;
    if workspace.user_id != auth.user_id {
        return Err(ApiError::Unauthorized("device does not own this workspace"));
    }

    // Apply the operation to the in-memory node tree before allocating the cursor.
    apply_operation(workspace, &operation, &blob_exists)?;

    // Allocate the next cursor atomically under the workspace lock.
    let next_value = workspace
        .current_cursor
        .value()
        .checked_add(1)
        .ok_or_else(|| ApiError::Internal("cursor overflow".to_owned()))?;
    let assigned_cursor =
        Cursor::new(next_value).map_err(|error| ApiError::Internal(error.to_string()))?;
    workspace.current_cursor = assigned_cursor;

    let op_id = operation.op_id;
    let committed = CommittedOperation {
        operation,
        cursor: assigned_cursor,
    };
    drop(workspaces);

    let mut operations = state.operations.write().await;
    let log = operations.entry(workspace_id).or_default();
    log.push(committed.clone());
    drop(operations);

    idempotency.insert((workspace_id, op_id), assigned_cursor);
    drop(idempotency);

    let _ = state.event_tx.send(WorkspaceEvent::WorkspaceOpsAvailable {
        workspace_id,
        from_cursor: assigned_cursor,
        to_cursor: assigned_cursor,
    });

    Ok(Json(CommitOperationResponse {
        workspace_id,
        op_id,
        cursor: assigned_cursor,
        committed,
    }))
}

async fn fetch_operations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<WorkspaceId>,
    Query(query): Query<FetchOpsQuery>,
) -> Result<Json<FetchOpsResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    {
        let workspaces = state.workspaces.read().await;
        let workspace = workspaces
            .get(&workspace_id)
            .ok_or_else(|| ApiError::structured(Fs2Error::WorkspaceNotFound))?;
        if workspace.user_id != auth.user_id {
            return Err(ApiError::Unauthorized("device does not own this workspace"));
        }
        drop(workspaces);
    }

    let since = query.since.unwrap_or(0);
    if since < 0 {
        return Err(ApiError::InvalidRequest("since must not be negative"));
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_OPS_PAGE_LIMIT)
        .clamp(1, MAX_OPS_PAGE_LIMIT);

    let operations = state.operations.read().await;
    let log = operations.get(&workspace_id);
    let filtered: Vec<CommittedOperation> = log
        .map(|log| {
            log.iter()
                .filter(|committed| committed.cursor.value() > since)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    drop(operations);

    let has_more = filtered.len() > limit as usize;
    let next_cursor = if has_more {
        filtered
            .get(limit as usize - 1)
            .map(|committed| committed.cursor)
    } else {
        None
    };
    let page: Vec<CommittedOperation> = filtered.into_iter().take(limit as usize).collect();

    Ok(Json(FetchOpsResponse {
        workspace_id,
        operations: page,
        has_more,
        next_cursor,
    }))
}

async fn fetch_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<WorkspaceId>,
    Query(query): Query<ManifestQuery>,
) -> Result<Json<ManifestResponse>, ApiError> {
    let auth = authenticate(&headers, &state).await?;
    let requested_path = WorkspacePath::parse(query.path.as_deref().unwrap_or_default())
        .map_err(|_| ApiError::structured(Fs2Error::InvalidOperation))?;
    let requested_depth = query.depth.unwrap_or(DEFAULT_MANIFEST_DEPTH);
    let depth = requested_depth.min(MAX_MANIFEST_DEPTH);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_MANIFEST_PAGE_LIMIT)
        .clamp(1, MAX_MANIFEST_PAGE_LIMIT) as usize;
    let offset = query.offset.unwrap_or(0);

    let workspaces = state.workspaces.read().await;
    let workspace = workspaces
        .get(&workspace_id)
        .ok_or_else(|| ApiError::structured(Fs2Error::WorkspaceNotFound))?;
    if workspace.user_id != auth.user_id {
        return Err(ApiError::Unauthorized("device does not own this workspace"));
    }
    let root_node_id = resolve_workspace_path(workspace, &requested_path)?;
    let mut entries = Vec::new();
    collect_manifest_entries(
        workspace,
        root_node_id,
        requested_path.as_str(),
        0,
        depth,
        &mut entries,
    )?;
    drop(workspaces);

    let has_more = entries.len().saturating_sub(offset) > limit;
    let next_offset = has_more.then_some(offset + limit);
    let nodes = entries.into_iter().skip(offset).take(limit).collect();

    Ok(Json(ManifestResponse {
        workspace_id,
        nodes,
        has_more,
        next_offset,
    }))
}

async fn workspace_events_ws(
    State(state): State<AppState>,
    Path(workspace_id): Path<WorkspaceId>,
    Query(query): Query<HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let token = query
        .get("access_token")
        .or_else(|| query.get("token"))
        .ok_or(ApiError::Unauthorized("missing access token"))?;
    let auth = authenticate_token(token, &state).await?;
    {
        let workspaces = state.workspaces.read().await;
        let workspace = workspaces
            .get(&workspace_id)
            .ok_or_else(|| ApiError::structured(Fs2Error::WorkspaceNotFound))?;
        if workspace.user_id != auth.user_id {
            return Err(ApiError::Unauthorized("device does not own this workspace"));
        }
        drop(workspaces);
    }

    let rx = state.event_tx.subscribe();
    Ok(upgrade.on_upgrade(move |socket| stream_workspace_events(socket, rx, workspace_id)))
}

async fn stream_workspace_events(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<WorkspaceEvent>,
    workspace_id: WorkspaceId,
) {
    loop {
        tokio::select! {
            received = rx.recv() => {
                let event = match received {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)
                    | broadcast::error::RecvError::Closed) => break,
                };
                let WorkspaceEvent::WorkspaceOpsAvailable { workspace_id: event_workspace_id, .. } = event;
                if event_workspace_id != workspace_id {
                    continue;
                }
                let Ok(text) = serde_json::to_string(&event) else {
                    continue;
                };
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

const DEFAULT_OPS_PAGE_LIMIT: u32 = 100;
const MAX_OPS_PAGE_LIMIT: u32 = 1000;
const DEFAULT_MANIFEST_DEPTH: u32 = 1;
const MAX_MANIFEST_DEPTH: u32 = 32;
const DEFAULT_MANIFEST_PAGE_LIMIT: u32 = 100;
const MAX_MANIFEST_PAGE_LIMIT: u32 = 1000;
fn resolve_workspace_path(
    workspace: &WorkspaceRecord,
    path: &WorkspacePath,
) -> Result<NodeId, ApiError> {
    let mut current = workspace.root_node_id;
    for segment in path.segments() {
        current = workspace
            .nodes
            .values()
            .find(|node| {
                node.parent_id == Some(current) && node.deleted_at.is_none() && node.name == segment
            })
            .map(|node| node.node_id)
            .ok_or_else(|| ApiError::structured(Fs2Error::NodeNotFound))?;
    }
    Ok(current)
}

fn collect_manifest_entries(
    workspace: &WorkspaceRecord,
    node_id: NodeId,
    path: &str,
    current_depth: u32,
    max_depth: u32,
    entries: &mut Vec<ManifestEntry>,
) -> Result<(), ApiError> {
    let node = live_node(workspace, node_id)?.clone();
    let current_revision = node
        .current_rev
        .and_then(|revision_id| workspace.revisions.get(&revision_id).cloned());
    entries.push(ManifestEntry {
        path: path.to_owned(),
        depth: current_depth,
        node,
        current_revision,
    });

    if current_depth >= max_depth {
        return Ok(());
    }

    let mut children = workspace
        .nodes
        .values()
        .filter(|child| child.parent_id == Some(node_id) && child.deleted_at.is_none())
        .collect::<Vec<_>>();
    children.sort_by(|left, right| left.name.cmp(&right.name));

    for child in children {
        let child_path = if path.is_empty() {
            child.name.clone()
        } else {
            format!("{path}/{}", child.name)
        };
        collect_manifest_entries(
            workspace,
            child.node_id,
            &child_path,
            current_depth + 1,
            max_depth,
            entries,
        )?;
    }
    Ok(())
}

/// Converts a shared `ErrorEnvelope` from shape validation into a structured `ApiError`.
fn shape_error_into_api_error(envelope: ErrorEnvelope) -> ApiError {
    ApiError::Structured {
        error: Fs2Error::InvalidOperation,
        details: envelope.error.details,
    }
}

/// Validates an operation against the live workspace tree and applies it in place.
///
/// Supports all shared operation variants in the in-memory development backend.
fn apply_operation(
    workspace: &mut WorkspaceRecord,
    operation: &Operation,
    blob_exists: &dyn Fn(&str) -> bool,
) -> Result<(), ApiError> {
    let now = operation.created_at;
    match &operation.kind {
        OperationKind::CreateNode {
            node_id,
            parent_id,
            name,
            kind,
            initial_revision,
        } => apply_create_node(
            workspace,
            CreateNodeInput {
                node_id: *node_id,
                parent_id: *parent_id,
                name,
                kind: *kind,
                initial_revision: initial_revision.as_ref(),
            },
            now,
            blob_exists,
        ),
        OperationKind::PutFileRevision {
            node_id,
            base_revision_id,
            revision,
        } => apply_put_file_revision(
            workspace,
            *node_id,
            *base_revision_id,
            revision,
            now,
            blob_exists,
        ),
        OperationKind::MoveNode {
            node_id,
            old_parent_id,
            old_name,
            new_parent_id,
            new_name,
        } => apply_move_node(
            workspace,
            *node_id,
            *old_parent_id,
            old_name,
            *new_parent_id,
            new_name,
            now,
        ),
        OperationKind::DeleteNode { node_id, recursive } => {
            apply_delete_node(workspace, *node_id, *recursive, now)
        }
        OperationKind::RestoreNode {
            node_id,
            parent_id,
            name,
        } => apply_restore_node(workspace, *node_id, *parent_id, name, now),
        OperationKind::SetRule { path_pattern, rule } => {
            workspace.rules.insert(path_pattern.clone(), rule.clone());
            Ok(())
        }
        OperationKind::SetEnvVar {
            env_var_id,
            encrypted_payload,
            metadata,
        } => {
            workspace.env_vars.insert(
                *env_var_id,
                EnvRecord {
                    env_var_id: *env_var_id,
                    encrypted_payload: encrypted_payload.clone(),
                    metadata: metadata.clone(),
                },
            );
            Ok(())
        }
        OperationKind::DeleteEnvVar { env_var_id } => {
            workspace
                .env_vars
                .remove(env_var_id)
                .ok_or_else(|| ApiError::structured(Fs2Error::InvalidOperation))?;
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
struct CreateNodeInput<'a> {
    node_id: NodeId,
    parent_id: NodeId,
    name: &'a str,
    kind: NodeKind,
    initial_revision: Option<&'a NodeRevision>,
}

fn apply_create_node(
    workspace: &mut WorkspaceRecord,
    input: CreateNodeInput<'_>,
    now: chrono::DateTime<Utc>,
    blob_exists: &dyn Fn(&str) -> bool,
) -> Result<(), ApiError> {
    let CreateNodeInput {
        node_id,
        parent_id,
        name,
        kind,
        initial_revision,
    } = input;
    if workspace.nodes.contains_key(&node_id) {
        return Err(ApiError::structured_with(
            Fs2Error::PathCollision,
            "node_id",
            node_id.to_string(),
        ));
    }
    {
        let parent = live_node(workspace, parent_id)?;
        if parent.kind != NodeKind::Directory {
            return Err(ApiError::structured_with(
                Fs2Error::InvalidOperation,
                "parent_id",
                "parent must be a directory",
            ));
        }
    }
    let candidate_name = NodeName::parse(name).map_err(|_| {
        ApiError::structured_with(Fs2Error::InvalidOperation, "name", name.to_owned())
    })?;
    if has_live_sibling_collision(workspace, parent_id, &candidate_name) {
        return Err(ApiError::structured(Fs2Error::PathCollision));
    }

    if let Some(revision) = initial_revision {
        validate_new_revision(workspace, revision, blob_exists)?;
    }

    let current_rev = initial_revision.map(|revision| {
        workspace
            .revisions
            .insert(revision.revision_id, revision.clone());
        revision.revision_id
    });
    let node = Node {
        node_id,
        workspace_id: workspace.workspace_id,
        parent_id: Some(parent_id),
        name: name.to_owned(),
        kind,
        current_rev,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        tombstone_version: None,
    };
    workspace.nodes.insert(node_id, node);
    Ok(())
}

fn revision_conflict(
    node_id: NodeId,
    current_revision_id: Option<RevisionId>,
    base_revision_id: Option<RevisionId>,
) -> ApiError {
    let mut details = serde_json::Map::new();
    details.insert(
        "node_id".to_owned(),
        serde_json::Value::String(node_id.to_string()),
    );
    details.insert(
        "current_revision_id".to_owned(),
        current_revision_id.map_or(serde_json::Value::Null, |revision_id| {
            serde_json::Value::String(revision_id.to_string())
        }),
    );
    details.insert(
        "base_revision_id".to_owned(),
        base_revision_id.map_or(serde_json::Value::Null, |revision_id| {
            serde_json::Value::String(revision_id.to_string())
        }),
    );
    ApiError::Structured {
        error: Fs2Error::RevisionConflict,
        details,
    }
}

fn apply_put_file_revision(
    workspace: &mut WorkspaceRecord,
    node_id: NodeId,
    base_revision_id: Option<RevisionId>,
    revision: &NodeRevision,
    now: chrono::DateTime<Utc>,
    blob_exists: &dyn Fn(&str) -> bool,
) -> Result<(), ApiError> {
    // Validate with an immutable borrow first, then mutate.
    {
        let node = live_node(workspace, node_id)?;
        if node.kind != NodeKind::File {
            return Err(ApiError::structured_with(
                Fs2Error::InvalidOperation,
                "node_id",
                "node must be a file",
            ));
        }
        // Conflict rule: base_revision_id must equal the node's current revision.
        if node.current_rev != base_revision_id {
            return Err(revision_conflict(
                node.node_id,
                node.current_rev,
                base_revision_id,
            ));
        }
    }
    validate_new_revision(workspace, revision, blob_exists)?;

    workspace
        .revisions
        .insert(revision.revision_id, revision.clone());
    let node = live_node_mut(workspace, node_id)?;
    node.current_rev = Some(revision.revision_id);
    node.updated_at = now;
    Ok(())
}

fn validate_new_revision(
    workspace: &WorkspaceRecord,
    revision: &NodeRevision,
    blob_exists: &dyn Fn(&str) -> bool,
) -> Result<(), ApiError> {
    if workspace.revisions.contains_key(&revision.revision_id) {
        return Err(ApiError::structured_with(
            Fs2Error::InvalidOperation,
            "revision_id",
            "revision id already exists",
        ));
    }
    if let RevisionContent::File { blob_id, .. } = &revision.content {
        if !blob_exists(blob_id.as_str()) {
            return Err(ApiError::structured_with(
                Fs2Error::BlobMissing,
                "blob_id",
                blob_id.to_string(),
            ));
        }
    }
    Ok(())
}

fn apply_move_node(
    workspace: &mut WorkspaceRecord,
    node_id: NodeId,
    old_parent_id: NodeId,
    old_name: &str,
    new_parent_id: NodeId,
    new_name: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(), ApiError> {
    // Validate with immutable borrows first, then mutate.
    {
        let node = live_node(workspace, node_id)?;
        if node.parent_id != Some(old_parent_id) || node.name != old_name {
            return Err(ApiError::structured_with(
                Fs2Error::InvalidOperation,
                "node_id",
                "old_parent_id/old_name do not match node location",
            ));
        }
    }
    if node_id == new_parent_id || is_descendant_of(workspace, new_parent_id, node_id) {
        return Err(ApiError::structured_with(
            Fs2Error::InvalidOperation,
            "new_parent_id",
            "move would create a cycle",
        ));
    }
    let new_parent = live_node(workspace, new_parent_id)?;
    if new_parent.kind != NodeKind::Directory {
        return Err(ApiError::structured_with(
            Fs2Error::InvalidOperation,
            "new_parent_id",
            "new parent must be a directory",
        ));
    }
    let candidate_name = NodeName::parse(new_name).map_err(|_| {
        ApiError::structured_with(Fs2Error::InvalidOperation, "new_name", new_name.to_owned())
    })?;
    if has_live_sibling_collision_excluding(
        workspace,
        new_parent_id,
        &candidate_name,
        Some(node_id),
    ) {
        return Err(ApiError::structured(Fs2Error::PathCollision));
    }
    let node = live_node_mut(workspace, node_id)?;
    node.parent_id = Some(new_parent_id);
    new_name.clone_into(&mut node.name);
    node.updated_at = now;
    Ok(())
}

fn apply_delete_node(
    workspace: &mut WorkspaceRecord,
    node_id: NodeId,
    recursive: bool,
    now: chrono::DateTime<Utc>,
) -> Result<(), ApiError> {
    // Validate with immutable borrows first, then mutate.
    {
        let node = live_node(workspace, node_id)?;
        if node.parent_id.is_none() {
            return Err(ApiError::structured_with(
                Fs2Error::InvalidOperation,
                "node_id",
                "cannot delete the workspace root",
            ));
        }
    }
    let has_live_children = workspace
        .nodes
        .values()
        .any(|child| child.parent_id == Some(node_id) && child.deleted_at.is_none());
    if has_live_children && !recursive {
        return Err(ApiError::structured_with(
            Fs2Error::InvalidOperation,
            "recursive",
            "recursive flag required to delete a non-empty directory",
        ));
    }
    let tombstone_version = Some(workspace.current_cursor.value() + 1);
    {
        let node = live_node_mut(workspace, node_id)?;
        node.deleted_at = Some(now);
        node.updated_at = now;
        node.tombstone_version = tombstone_version;
    }
    if recursive {
        delete_descendants(workspace, node_id, now, tombstone_version);
    }
    Ok(())
}

fn apply_restore_node(
    workspace: &mut WorkspaceRecord,
    node_id: NodeId,
    parent_id: NodeId,
    name: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(), ApiError> {
    // Validate with immutable borrows first, then mutate.
    let is_tombstoned = {
        let node = workspace
            .nodes
            .get(&node_id)
            .ok_or_else(|| ApiError::structured(Fs2Error::NodeNotFound))?;
        if node.deleted_at.is_none() {
            return Err(ApiError::structured_with(
                Fs2Error::InvalidOperation,
                "node_id",
                "node is not tombstoned",
            ));
        }
        true
    };
    let _ = is_tombstoned;
    let parent = live_node(workspace, parent_id)?;
    if parent.kind != NodeKind::Directory {
        return Err(ApiError::structured_with(
            Fs2Error::InvalidOperation,
            "parent_id",
            "parent must be a directory",
        ));
    }
    let candidate_name = NodeName::parse(name).map_err(|_| {
        ApiError::structured_with(Fs2Error::InvalidOperation, "name", name.to_owned())
    })?;
    if has_live_sibling_collision_excluding(workspace, parent_id, &candidate_name, Some(node_id)) {
        return Err(ApiError::structured(Fs2Error::PathCollision));
    }
    let node = workspace
        .nodes
        .get_mut(&node_id)
        .ok_or_else(|| ApiError::structured(Fs2Error::NodeNotFound))?;
    node.parent_id = Some(parent_id);
    name.clone_into(&mut node.name);
    node.deleted_at = None;
    node.tombstone_version = None;
    node.updated_at = now;
    Ok(())
}

/// Returns a live (non-tombstoned) node or a structured `node_not_found` error.
fn live_node(workspace: &WorkspaceRecord, node_id: NodeId) -> Result<&Node, ApiError> {
    let node = workspace
        .nodes
        .get(&node_id)
        .ok_or_else(|| ApiError::structured(Fs2Error::NodeNotFound))?;
    if node.deleted_at.is_some() {
        return Err(ApiError::structured(Fs2Error::NodeNotFound));
    }
    Ok(node)
}

fn live_node_mut(workspace: &mut WorkspaceRecord, node_id: NodeId) -> Result<&mut Node, ApiError> {
    let node = workspace
        .nodes
        .get_mut(&node_id)
        .ok_or_else(|| ApiError::structured(Fs2Error::NodeNotFound))?;
    if node.deleted_at.is_some() {
        return Err(ApiError::structured(Fs2Error::NodeNotFound));
    }
    Ok(node)
}

/// Checks whether a candidate name collides with any live sibling under `parent_id`.
fn has_live_sibling_collision(
    workspace: &WorkspaceRecord,
    parent_id: NodeId,
    name: &NodeName,
) -> bool {
    has_live_sibling_collision_excluding(workspace, parent_id, name, None)
}

/// Checks sibling collisions, optionally excluding one node (the mover/restorer itself).
fn has_live_sibling_collision_excluding(
    workspace: &WorkspaceRecord,
    parent_id: NodeId,
    name: &NodeName,
    exclude: Option<NodeId>,
) -> bool {
    workspace.nodes.values().any(|sibling| {
        sibling.parent_id == Some(parent_id)
            && sibling.deleted_at.is_none()
            && exclude != Some(sibling.node_id)
            && names_collide(
                name,
                &NodeName::parse(sibling.name.as_str()).unwrap_or_else(|_| name.clone()),
                CasePolicy::Portable,
            )
    })
}

/// Returns true if `candidate` is a descendant of `ancestor` in the live tree.
fn is_descendant_of(workspace: &WorkspaceRecord, candidate: NodeId, ancestor: NodeId) -> bool {
    let mut current = candidate;
    while let Some(node) = workspace.nodes.get(&current) {
        match node.parent_id {
            Some(parent) if parent == ancestor => return true,
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

/// Recursively tombstones all descendants of `node_id`.
fn delete_descendants(
    workspace: &mut WorkspaceRecord,
    node_id: NodeId,
    now: chrono::DateTime<Utc>,
    tombstone_version: Option<i64>,
) {
    let children: Vec<NodeId> = workspace
        .nodes
        .values()
        .filter(|child| child.parent_id == Some(node_id))
        .map(|child| child.node_id)
        .collect();
    for child_id in children {
        if let Some(child) = workspace.nodes.get_mut(&child_id) {
            child.deleted_at = Some(now);
            child.updated_at = now;
            child.tombstone_version = tombstone_version;
        }
        delete_descendants(workspace, child_id, now, tombstone_version);
    }
}

async fn dev_blob_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DevBlobUploadRequest>,
) -> Result<Json<DevBlobUploadResponse>, ApiError> {
    let _auth = authenticate(&headers, &state).await?;
    let bytes = URL_SAFE_NO_PAD
        .decode(&request.bytes_base64)
        .map_err(|_| ApiError::InvalidRequest("bytes_base64 must be URL-safe base64"))?;
    if bytes.len() as u64 != request.size {
        return Err(ApiError::InvalidRequest(
            "declared blob size does not match payload",
        ));
    }
    validate_blob_hash(&request.blob_id, &bytes)?;
    state
        .blob_store
        .put(&request.blob_id, Bytes::from(bytes))
        .await
        .map_err(blob_store_api_error)?;
    let metadata = BlobMetadata {
        blob_id: request.blob_id.clone(),
        size: request.size,
        encryption_header: request.encryption_header,
    };
    state
        .blob_metadata
        .write()
        .await
        .insert(metadata.blob_id.clone(), metadata);
    Ok(Json(DevBlobUploadResponse {
        blob_id: request.blob_id,
        size: request.size,
    }))
}

async fn dev_blob_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DevBlobDownloadRequest>,
) -> Result<Json<DevBlobDownloadResponse>, ApiError> {
    let _auth = authenticate(&headers, &state).await?;
    let bytes = state
        .blob_store
        .get(&request.blob_id)
        .await
        .map_err(blob_store_api_error)?;
    validate_blob_hash(&request.blob_id, &bytes)?;
    let metadata = state
        .blob_metadata
        .read()
        .await
        .get(&request.blob_id)
        .cloned();
    Ok(Json(DevBlobDownloadResponse {
        blob_id: request.blob_id,
        bytes_base64: URL_SAFE_NO_PAD.encode(&bytes),
        size: bytes.len() as u64,
        encryption_header: metadata.and_then(|metadata| metadata.encryption_header),
    }))
}

async fn blob_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(blob_id): Path<String>,
) -> Result<Json<BlobStatusResponse>, ApiError> {
    let _auth = authenticate(&headers, &state).await?;
    let exists = state
        .blob_store
        .exists(&blob_id)
        .await
        .map_err(blob_store_api_error)?;
    let metadata = state.blob_metadata.read().await.get(&blob_id).cloned();
    Ok(Json(BlobStatusResponse {
        blob_id,
        exists,
        size: metadata.as_ref().map(|metadata| metadata.size),
        encryption_header: metadata.and_then(|metadata| metadata.encryption_header),
    }))
}

fn blob_store_api_error(error: BlobStoreError) -> ApiError {
    match error {
        BlobStoreError::InvalidKey(_) => ApiError::InvalidRequest("invalid blob id"),
        BlobStoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ApiError::BlobMissing
        }
        BlobStoreError::Io(error) => ApiError::Internal(error.to_string()),
        BlobStoreError::ObjectStore(message) => ApiError::Internal(message),
    }
}

fn blob_store_for_config(
    object_store: &ObjectStoreConfig,
) -> Result<Arc<dyn BlobStore>, ConfigError> {
    match object_store {
        ObjectStoreConfig::Local { root } => Ok(Arc::new(LocalFilesystemBlobStore::new(root))),
        ObjectStoreConfig::S3 {
            bucket,
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            session_token,
            allow_http,
            virtual_hosted_style,
        } => {
            let mut builder = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_endpoint(endpoint)
                .with_region(region)
                .with_access_key_id(access_key_id.expose_for_signing())
                .with_secret_access_key(secret_access_key.expose_for_signing())
                .with_allow_http(*allow_http)
                .with_virtual_hosted_style_request(*virtual_hosted_style);
            if let Some(token) = session_token {
                builder = builder.with_token(token.expose_for_signing());
            }
            let store = builder
                .build()
                .map_err(|error| ConfigError::ObjectStore(error.to_string()))?;
            Ok(Arc::new(ObjectStoreBlobStore::new(Arc::new(store))))
        }
    }
}

fn validate_blob_hash(blob_id: &str, bytes: &[u8]) -> Result<(), ApiError> {
    if let Some(expected) = blob_id.strip_prefix("sha256:") {
        let actual = hex_lower(&Sha256::digest(bytes));
        if expected != actual {
            return Err(ApiError::InvalidRequest(
                "sha256 blob id does not match payload",
            ));
        }
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn validate_device_fields(name: &str, public_key: &str) -> Result<(), ApiError> {
    if name.trim().is_empty() {
        return Err(ApiError::InvalidRequest("device name must not be empty"));
    }
    if public_key.trim().is_empty() {
        return Err(ApiError::InvalidRequest("public key must not be empty"));
    }
    Ok(())
}

fn validate_workspace_name(name: &str) -> Result<(), ApiError> {
    if name.trim().is_empty() {
        return Err(ApiError::InvalidRequest("workspace name must not be empty"));
    }
    if name.contains('\0') || name.contains('/') || name.contains('\\') {
        return Err(ApiError::InvalidRequest(
            "workspace name must not contain path separators or null bytes",
        ));
    }
    Ok(())
}

async fn authenticate(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedDevice, ApiError> {
    let token = bearer_token(headers)?;
    authenticate_token(token, state).await
}

async fn authenticate_token(
    token: &str,
    state: &AppState,
) -> Result<AuthenticatedDevice, ApiError> {
    let claims = decode_access_token(token, &state.jwt_secret)?;
    let device = {
        let devices = state.devices.read().await;
        let device = devices
            .get(&claims.device_id)
            .ok_or(ApiError::Unauthorized("token device is not enrolled"))?
            .clone();
        drop(devices);
        device
    };
    if device.revoked {
        return Err(ApiError::DeviceRevoked);
    }
    if device.user_id != claims.sub {
        return Err(ApiError::Unauthorized("token subject does not own device"));
    }
    let (user_id, device_id) = (claims.sub, claims.device_id);
    Ok(AuthenticatedDevice { user_id, device_id })
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or(ApiError::Unauthorized("missing bearer token"))?
        .to_str()
        .map_err(|_| ApiError::Unauthorized("authorization header is not valid UTF-8"))?;
    header.strip_prefix("Bearer ").ok_or(ApiError::Unauthorized(
        "authorization header must use Bearer scheme",
    ))
}

type HmacSha256 = Hmac<Sha256>;

fn encode_access_token(
    user_id: UserId,
    device_id: DeviceId,
    secret: &RedactedSecret,
) -> Result<String, ApiError> {
    let exp = unix_timestamp()?
        .checked_add(60 * 60)
        .ok_or_else(|| ApiError::Internal("token expiration overflow".to_owned()))?;
    let claims = AccessClaims {
        sub: user_id,
        device_id,
        exp,
    };
    let payload =
        serde_json::to_vec(&claims).map_err(|error| ApiError::Internal(error.to_string()))?;
    let encoded_payload = URL_SAFE_NO_PAD.encode(payload);
    let signature = sign_token_payload(&encoded_payload, secret)?;
    Ok(format!("{encoded_payload}.{signature}"))
}

fn decode_access_token(token: &str, secret: &RedactedSecret) -> Result<AccessClaims, ApiError> {
    let (encoded_payload, signature) = token
        .split_once('.')
        .ok_or(ApiError::Unauthorized("invalid bearer token"))?;
    verify_token_signature(encoded_payload, signature, secret)?;
    let payload = URL_SAFE_NO_PAD
        .decode(encoded_payload)
        .map_err(|_| ApiError::Unauthorized("invalid bearer token"))?;
    let claims = serde_json::from_slice::<AccessClaims>(&payload)
        .map_err(|_| ApiError::Unauthorized("invalid bearer token"))?;
    let now = unix_timestamp()?;
    if claims.exp <= now {
        return Err(ApiError::Unauthorized("bearer token expired"));
    }
    Ok(claims)
}

fn sign_token_payload(payload: &str, secret: &RedactedSecret) -> Result<String, ApiError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_for_signing().as_bytes())
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    mac.update(payload.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn verify_token_signature(
    payload: &str,
    signature: &str,
    secret: &RedactedSecret,
) -> Result<(), ApiError> {
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| ApiError::Unauthorized("invalid bearer token"))?;
    let mut mac = HmacSha256::new_from_slice(secret.expose_for_signing().as_bytes())
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| ApiError::Unauthorized("invalid bearer token"))
}

fn unix_timestamp() -> Result<u64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| ApiError::Internal(error.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]
    use super::*;
    use axum::{body::Body, http::Request};
    use futures_util::{Stream, StreamExt};
    use object_store::memory::InMemory;
    use serde::de::DeserializeOwned;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio_tungstenite::{connect_async, tungstenite::Message as TungsteniteMessage};
    use tower::util::ServiceExt;

    #[test]
    fn config_loads_defaults_and_redacts_secrets() -> Result<(), Box<dyn std::error::Error>> {
        let config = BackendConfig::from_lookup(|_| None)?;

        assert_eq!(config.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(config.database_url, DEFAULT_DATABASE_URL);
        assert!(matches!(
            config.object_store,
            ObjectStoreConfig::Local { .. }
        ));
        assert!(format!("{config:?}").contains("<redacted>"));
        assert!(!format!("{config:?}").contains(DEFAULT_DEV_SECRET));
        Ok(())
    }

    #[test]
    fn local_blob_store_uses_configured_root() -> Result<(), Box<dyn std::error::Error>> {
        let config = BackendConfig::from_lookup(|key| {
            (key == "FS2_OBJECT_STORE").then(|| "local:/tmp/fs2-test-blobs".to_owned())
        })?;

        assert!(matches!(
            config.object_store,
            ObjectStoreConfig::Local { ref root } if root == &PathBuf::from("/tmp/fs2-test-blobs")
        ));
        assert!(blob_store_for_config(&config.object_store).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn postgres_migrations_run_on_empty_db_and_backend_starts(
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let postgres = DockerPostgres::start()?;
        wait_for_postgres(&postgres.database_url).await?;

        let object_dir = TempDir::new()?;
        let config = BackendConfig {
            bind_addr: "127.0.0.1:0".parse()?,
            database_url: postgres.database_url.clone(),
            object_store: ObjectStoreConfig::Local {
                root: object_dir.path().to_path_buf(),
            },
            jwt_secret: RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?,
            session_secret: RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?,
        };

        serve(config, async {}).await?;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&postgres.database_url)
            .await?;
        let table_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM (VALUES \
             ('users'), ('devices'), ('workspaces'), ('nodes'), ('node_revisions'), \
             ('operations'), ('blobs'), ('env_vars'), ('key_envelopes')) AS expected(name) \
             JOIN information_schema.tables tables \
             ON tables.table_schema = 'public' AND tables.table_name = expected.name",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(table_count, 9);

        let index_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_indexes \
             WHERE schemaname = 'public' \
             AND indexname IN ( \
               'nodes_live_name_idx', \
               'nodes_live_root_name_idx', \
               'nodes_workspace_parent_idx', \
               'nodes_workspace_path_scan_idx', \
               'node_revisions_node_created_idx', \
               'node_revisions_workspace_node_idx', \
               'operations_workspace_cursor_idx', \
               'blobs_workspace_idx', \
               'env_vars_live_workspace_name_idx', \
               'env_vars_live_project_name_idx', \
               'env_vars_live_machine_name_idx', \
               'env_vars_live_project_machine_name_idx', \
               'env_vars_workspace_idx', \
               'key_envelopes_device_idx' \
             )",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(index_count, 14);
        sqlx::query(
            "INSERT INTO users (id, email) \
             VALUES ('00000000-0000-0000-0000-000000000001', 'test@example.invalid')",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO workspaces (id, user_id, name, root_node_id) \
             VALUES ( \
               '00000000-0000-0000-0000-000000000002', \
               '00000000-0000-0000-0000-000000000001', \
               'code', \
               '00000000-0000-0000-0000-000000000003' \
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO nodes (id, workspace_id, parent_id, name, normalized_name, kind) \
             VALUES ( \
               '00000000-0000-0000-0000-000000000004', \
               '00000000-0000-0000-0000-000000000002', \
               NULL, \
               'README.md', \
               'readme.md', \
               'file' \
             )",
        )
        .execute(&pool)
        .await?;
        let duplicate_root_child = sqlx::query(
            "INSERT INTO nodes (id, workspace_id, parent_id, name, normalized_name, kind) \
             VALUES ( \
               '00000000-0000-0000-0000-000000000005', \
               '00000000-0000-0000-0000-000000000002', \
               NULL, \
               'readme.md', \
               'readme.md', \
               'file' \
             )",
        )
        .execute(&pool)
        .await;
        assert!(duplicate_root_child.is_err());
        sqlx::query(
            "INSERT INTO nodes (id, workspace_id, parent_id, name, normalized_name, kind) \
             VALUES ( \
               '00000000-0000-0000-0000-000000000000', \
               '00000000-0000-0000-0000-000000000002', \
               NULL, \
               'sentinel-parent', \
               'sentinel-parent', \
               'directory' \
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO nodes (id, workspace_id, parent_id, name, normalized_name, kind) \
             VALUES ( \
               '00000000-0000-0000-0000-000000000006', \
               '00000000-0000-0000-0000-000000000002', \
               '00000000-0000-0000-0000-000000000000', \
               'readme.md', \
               'readme.md', \
               'file' \
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000010', \
               '00000000-0000-0000-0000-000000000002', \
               'dev', \
               'API_KEY', \
               '{\"type\":\"workspace\"}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000011', \
               '00000000-0000-0000-0000-000000000002', \
               'dev', \
               'API_KEY', \
               '{\"type\":\"machine\",\"device_id\":\"00000000-0000-0000-0000-000000000099\"}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, project_path, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000012', \
               '00000000-0000-0000-0000-000000000002', \
               '', \
               'dev', \
               'API_KEY', \
               '{\"type\":\"project\",\"project_path\":\"\"}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await?;
        let duplicate_workspace_env = sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000013', \
               '00000000-0000-0000-0000-000000000002', \
               'dev', \
               'API_KEY', \
               '{\"type\":\"workspace\"}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await;
        assert!(duplicate_workspace_env.is_err());
        let missing_scope_type = sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000014', \
               '00000000-0000-0000-0000-000000000002', \
               'dev', \
               'BROKEN', \
               '{}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await;
        assert!(missing_scope_type.is_err());

        let null_machine_device = sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000015', \
               '00000000-0000-0000-0000-000000000002', \
               'dev', \
               'BROKEN', \
               '{\"type\":\"machine\",\"device_id\":null}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await;
        assert!(null_machine_device.is_err());

        let mismatched_project_scope = sqlx::query(
            "INSERT INTO env_vars ( \
               id, workspace_id, project_path, environment, name, scope, secret_kind, encrypted_value, metadata \
             ) VALUES ( \
               '00000000-0000-0000-0000-000000000016', \
               '00000000-0000-0000-0000-000000000002', \
               'apps/api', \
               'dev', \
               'BROKEN', \
               '{\"type\":\"project\",\"project_path\":\"apps/web\"}'::jsonb, \
               'secret', \
               'ciphertext', \
               '{}'::jsonb \
             )",
        )
        .execute(&pool)
        .await;
        assert!(mismatched_project_scope.is_err());
        pool.close().await;
        Ok(())
    }

    #[test]
    fn config_parses_s3_store_redacts_credentials_and_rejects_bad_values(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = BackendConfig::from_lookup(|key| match key {
            "FS2_OBJECT_STORE" => Some("s3:bucket@http://localhost:9000".to_owned()),
            "FS2_S3_ACCESS_KEY_ID" => Some("access-key".to_owned()),
            "FS2_S3_SECRET_ACCESS_KEY" => Some("secret-key".to_owned()),
            "FS2_S3_SESSION_TOKEN" => Some("session-token".to_owned()),
            "FS2_S3_REGION" => Some("auto".to_owned()),
            "FS2_S3_ALLOW_HTTP" => Some("true".to_owned()),
            _ => None,
        })?;
        assert!(matches!(
            config.object_store,
            ObjectStoreConfig::S3 {
                ref bucket,
                ref endpoint,
                allow_http: true,
                virtual_hosted_style: false,
                ..
            } if bucket == "bucket" && endpoint == "http://localhost:9000"
        ));
        let debug = format!("{config:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("access-key"));
        assert!(!debug.contains("secret-key"));
        assert!(!debug.contains("session-token"));
        assert!(blob_store_for_config(&config.object_store).is_ok());

        assert!(BackendConfig::from_lookup(|key| {
            (key == "FS2_OBJECT_STORE").then(|| "s3:bucket".to_owned())
        })
        .is_err());
        assert!(BackendConfig::from_lookup(|key| {
            (key == "FS2_OBJECT_STORE").then(|| "local:".to_owned())
        })
        .is_err());
        assert!(BackendConfig::from_lookup(|key| match key {
            "FS2_OBJECT_STORE" => Some("s3:bucket@http://localhost:9000".to_owned()),
            "FS2_S3_SECRET_ACCESS_KEY" => Some("secret-key".to_owned()),
            _ => None,
        })
        .is_err());
        Ok(())
    }

    #[tokio::test]
    async fn object_store_blob_store_round_trips_and_rejects_invalid_keys(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let store = ObjectStoreBlobStore::new(Arc::new(InMemory::new()));
        assert!(!store.exists("sha256:missing").await?);

        store
            .put("sha256:abc/nested", Bytes::from_static(b"blob bytes"))
            .await?;
        assert!(store.exists("sha256:abc/nested").await?);
        assert_eq!(
            store.get("sha256:abc/nested").await?,
            Bytes::from_static(b"blob bytes")
        );

        assert!(matches!(
            store.put("../escape", Bytes::new()).await,
            Err(BlobStoreError::InvalidKey(_))
        ));
        assert!(matches!(
            store.get("sha256:missing").await,
            Err(BlobStoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
        Ok(())
    }

    #[tokio::test]
    async fn healthz_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
        let response = app()
            .oneshot(Request::builder().uri("/healthz").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn dev_login_issues_signed_token_and_whoami_extracts_claims(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app = app();
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "test laptop",
                "platform": {"os": "linux"},
                "public_key": "dev-public-key"
            }),
            None,
        )
        .await?;

        assert_eq!(login.token_type, "Bearer");
        assert_eq!(login.warning, DEV_AUTH_WARNING);

        let whoami = get_json::<AuthenticatedDeviceResponse>(
            app,
            "/v1/auth/whoami",
            Some(&login.access_token),
        )
        .await?;

        assert_eq!(whoami.user_id, login.user_id);
        assert_eq!(whoami.device_id, login.device_id);
        Ok(())
    }

    #[tokio::test]
    async fn workspace_create_makes_single_live_root_node() -> Result<(), Box<dyn std::error::Error>>
    {
        let state = AppState::dev(RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?);
        let app = app_with_state(state.clone());
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "test laptop",
                "platform": {"os": "linux"},
                "public_key": "dev-public-key"
            }),
            None,
        )
        .await?;

        let created = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "personal-code"}),
            Some(&login.access_token),
        )
        .await?;

        assert_eq!(created.current_cursor.value(), 0);
        let workspaces = state.workspaces.read().await;
        assert_eq!(workspaces.len(), 1);
        let workspace = workspaces
            .get(&created.workspace_id)
            .ok_or("created workspace missing from state")?;
        assert_eq!(workspace.user_id, login.user_id);
        assert_eq!(workspace.name, "personal-code");
        assert_eq!(workspace.root_node_id, created.root_node_id);
        assert_eq!(workspace.root_node.node_id, created.root_node_id);
        assert_eq!(workspace.root_node.workspace_id, created.workspace_id);
        assert_eq!(workspace.root_node.kind, NodeKind::Directory);
        assert_eq!(workspace.root_node.parent_id, None);
        assert_eq!(workspace.root_node.deleted_at, None);
        drop(workspaces);

        let rejected = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/workspaces")
                    .header("authorization", format!("Bearer {}", login.access_token))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"name": "bad/name"}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn dev_blob_endpoints_upload_status_download_and_validate_hash(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let state = AppState::dev_with_blob_root(
            RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?,
            dir.path(),
        );
        let app = app_with_state(state);
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "blob-client",
                "platform": {"os": "linux"},
                "public_key": "blob-key"
            }),
            None,
        )
        .await?;
        let bytes = b"ciphertext bytes";
        let blob_id = format!("sha256:{}", hex_lower(&Sha256::digest(bytes)));

        let upload = post_json::<DevBlobUploadResponse>(
            app.clone(),
            "/v1/blobs/dev-upload",
            serde_json::json!({
                "blob_id": blob_id,
                "bytes_base64": URL_SAFE_NO_PAD.encode(bytes),
                "size": bytes.len(),
                "encryption_header": "v1-header"
            }),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(upload.blob_id, blob_id);

        let status = get_json::<BlobStatusResponse>(
            app.clone(),
            &format!("/v1/blobs/{blob_id}/status"),
            Some(&login.access_token),
        )
        .await?;
        assert!(status.exists);
        assert_eq!(status.size, Some(bytes.len() as u64));

        let download = post_json::<DevBlobDownloadResponse>(
            app.clone(),
            "/v1/blobs/dev-download",
            serde_json::json!({"blob_id": blob_id}),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(download.bytes_base64, URL_SAFE_NO_PAD.encode(bytes));
        assert_eq!(download.encryption_header.as_deref(), Some("v1-header"));

        let rejected = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/blobs/dev-upload")
                    .header("authorization", format!("Bearer {}", login.access_token))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "blob_id": "sha256:0000",
                            "bytes_base64": URL_SAFE_NO_PAD.encode(bytes),
                            "size": bytes.len(),
                            "encryption_header": null
                        })
                        .to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let invalid_id = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/blobs/dev-upload")
                    .header("authorization", format!("Bearer {}", login.access_token))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "blob_id": "../escape",
                            "bytes_base64": URL_SAFE_NO_PAD.encode(bytes),
                            "size": bytes.len(),
                            "encryption_header": null
                        })
                        .to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(invalid_id.status(), StatusCode::BAD_REQUEST);
        let missing = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/blobs/dev-download")
                    .header("authorization", format!("Bearer {}", login.access_token))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"blob_id": "sha256:ffffffff"}).to_string(),
                    ))?,
            )
            .await?;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn device_enrollment_lists_two_devices_and_rejects_revoked_device(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let app = app();
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "first",
                "platform": {"os": "linux"},
                "public_key": "first-key"
            }),
            None,
        )
        .await?;

        let second = post_json::<EnrollDeviceResponse>(
            app.clone(),
            "/v1/devices",
            serde_json::json!({
                "name": "second",
                "platform": {"os": "macos"},
                "public_key": "second-key"
            }),
            Some(&login.access_token),
        )
        .await?;
        assert_ne!(second.device_id, login.device_id);

        let devices =
            get_json::<DeviceListResponse>(app.clone(), "/v1/devices", Some(&login.access_token))
                .await?;
        assert_eq!(devices.devices.len(), 2);
        assert!(devices.devices.iter().any(|device| device.name == "first"));
        assert!(devices.devices.iter().any(|device| device.name == "second"));

        let revoke_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/devices/{}/revoke", login.device_id))
                    .header("authorization", format!("Bearer {}", login.access_token))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(revoke_response.status(), StatusCode::NO_CONTENT);

        let rejected = app
            .oneshot(
                Request::builder()
                    .uri("/v1/devices")
                    .header("authorization", format!("Bearer {}", login.access_token))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn local_blob_store_put_get_exists_and_rejects_traversal(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let store = LocalFilesystemBlobStore::new(dir.path());
        let key = "sha256/ab/cdef";

        assert!(!store.exists(key).await?);
        store
            .put(key, Bytes::from_static(b"encrypted bytes"))
            .await?;

        assert!(store.exists(key).await?);
        assert_eq!(
            store.get(key).await?,
            Bytes::from_static(b"encrypted bytes")
        );
        store
            .put("sha256/ab/cdef.tmp", Bytes::from_static(b"tmp blob"))
            .await?;
        store
            .put("sha256/ab/cdef", Bytes::from_static(b"updated bytes"))
            .await?;
        assert_eq!(
            store.get("sha256/ab/cdef.tmp").await?,
            Bytes::from_static(b"tmp blob")
        );
        assert_eq!(
            store.get("sha256/ab/cdef").await?,
            Bytes::from_static(b"updated bytes")
        );
        assert!(store.put("../escape", Bytes::new()).await.is_err());
        assert!(store.get("/absolute").await.is_err());
        assert!(store.put("sha256/ab//cdef", Bytes::new()).await.is_err());
        assert!(store.put("sha256/ab/./cdef", Bytes::new()).await.is_err());
        Ok(())
    }

    async fn post_json_raw(
        app: Router,
        uri: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<Response, Box<dyn std::error::Error>> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        Ok(app
            .oneshot(builder.body(Body::from(body.to_string()))?)
            .await?)
    }

    async fn error_value(
        response: Response,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        serde_json::from_slice(&bytes).map_err(Into::into)
    }

    async fn error_code(response: Response) -> Result<String, Box<dyn std::error::Error>> {
        let value = error_value(response).await?;
        Ok(value
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned())
    }

    /// Builds a minimal file revision for a freshly created file node.
    fn file_revision(
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        node_id: NodeId,
        blob_id: &str,
    ) -> Result<NodeRevision, Box<dyn std::error::Error>> {
        Ok(NodeRevision {
            revision_id: RevisionId::new_v4(),
            node_id,
            workspace_id,
            device_id,
            base_revision_id: None,
            content: RevisionContent::File {
                blob_id: fs2_core::BlobId::new(blob_id)?,
                chunk_ids: Vec::new(),
                content_hash: "hash".to_owned(),
                encryption_header: None,
            },
            posix_mode: 0o644,
            mtime: Utc::now(),
            size: 0,
            executable: false,
            created_at: Utc::now(),
        })
    }

    #[tokio::test]
    async fn commit_op_idempotency_returns_same_cursor_without_double_apply(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::dev(RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?);
        let app = app_with_state(state.clone());
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "ops-laptop",
                "platform": {"os": "linux"},
                "public_key": "ops-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "ops-ws"}),
            Some(&login.access_token),
        )
        .await?;

        let op_id = OpId::new_v4();
        let node_id = NodeId::new_v4();
        let body = serde_json::json!({
            "op_id": op_id,
            "base_cursor": 0,
            "kind": {
                "type": "create_node",
                "node_id": node_id,
                "parent_id": workspace.root_node_id,
                "name": "docs",
                "kind": "directory",
                "initial_revision": null
            }
        });
        let uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);

        let first = post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            body.clone(),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(first.cursor.value(), 1);
        assert_eq!(first.committed.cursor, first.cursor);
        assert_eq!(first.committed.operation.op_id, op_id);

        // Duplicate submission returns the same cursor.
        let duplicate = post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            body.clone(),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(duplicate.cursor, first.cursor);
        assert_eq!(duplicate.op_id, op_id);
        assert_eq!(duplicate.committed, first.committed);

        {
            let mut workspaces = state.workspaces.write().await;
            let ws = workspaces
                .get_mut(&workspace.workspace_id)
                .ok_or("workspace missing after commit")?;
            ws.user_id = UserId::new_v4();
            drop(workspaces);
        }
        let unauthorized_duplicate =
            post_json_raw(app.clone(), &uri, body, Some(&login.access_token)).await?;
        assert_eq!(unauthorized_duplicate.status(), StatusCode::UNAUTHORIZED);

        // The node was applied exactly once.
        let workspaces = state.workspaces.read().await;
        let ws = workspaces
            .get(&workspace.workspace_id)
            .ok_or("workspace missing after commit")?;
        assert_eq!(ws.nodes.len(), 2);
        assert!(ws.nodes.contains_key(&node_id));
        drop(workspaces);

        // The operation log has exactly one entry.
        let operations = state.operations.read().await;
        let log = operations
            .get(&workspace.workspace_id)
            .ok_or("operation log missing after commit")?;
        assert_eq!(log.len(), 1);
        drop(operations);
        Ok(())
    }

    #[tokio::test]
    async fn commit_op_invalid_operation_returns_structured_error(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::dev(RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?);
        let app = app_with_state(state);
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "err-laptop",
                "platform": {"os": "linux"},
                "public_key": "err-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "err-ws"}),
            Some(&login.access_token),
        )
        .await?;

        // CreateNode with a non-existent parent -> node_not_found.
        let missing_parent = NodeId::new_v4();
        let body = serde_json::json!({
            "op_id": OpId::new_v4(),
            "base_cursor": 0,
            "kind": {
                "type": "create_node",
                "node_id": NodeId::new_v4(),
                "parent_id": missing_parent,
                "name": "docs",
                "kind": "directory",
                "initial_revision": null
            }
        });
        let uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);
        let response = post_json_raw(app.clone(), &uri, body, Some(&login.access_token)).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_code(response).await?, "node_not_found");

        // CreateNode with invalid name -> invalid_operation.
        let body = serde_json::json!({
            "op_id": OpId::new_v4(),
            "base_cursor": 0,
            "kind": {
                "type": "create_node",
                "node_id": NodeId::new_v4(),
                "parent_id": workspace.root_node_id,
                "name": "../bad",
                "kind": "directory",
                "initial_revision": null
            }
        });
        let response = post_json_raw(app.clone(), &uri, body, Some(&login.access_token)).await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_code(response).await?, "invalid_operation");

        // Workspace not found -> workspace_not_found.
        let body = serde_json::json!({
            "op_id": OpId::new_v4(),
            "base_cursor": 0,
            "kind": {
                "type": "create_node",
                "node_id": NodeId::new_v4(),
                "parent_id": workspace.root_node_id,
                "name": "ok",
                "kind": "directory",
                "initial_revision": null
            }
        });
        let bad_uri = format!("/v1/workspaces/{}/ops", WorkspaceId::new_v4());
        let response = post_json_raw(app, &bad_uri, body, Some(&login.access_token)).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_code(response).await?, "workspace_not_found");
        Ok(())
    }

    #[tokio::test]
    async fn commit_rejects_missing_initial_blob_and_duplicate_revision_id(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let state = AppState::dev_with_blob_root(
            RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?,
            dir.path(),
        );
        let app = app_with_state(state);
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "rev-laptop",
                "platform": {"os": "linux"},
                "public_key": "rev-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "rev-ws"}),
            Some(&login.access_token),
        )
        .await?;
        let uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);

        let missing_node_id = NodeId::new_v4();
        let missing_revision = file_revision(
            workspace.workspace_id,
            login.device_id,
            missing_node_id,
            "sha256:missing",
        )?;
        let response = post_json_raw(
            app.clone(),
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 0,
                "kind": {
                    "type": "create_node",
                    "node_id": missing_node_id,
                    "parent_id": workspace.root_node_id,
                    "name": "missing.txt",
                    "kind": "file",
                    "initial_revision": missing_revision
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_code(response).await?, "blob_missing");

        let file_bytes = b"revision bytes";
        let blob_id = format!("sha256:{}", hex_lower(&Sha256::digest(file_bytes)));
        post_json::<DevBlobUploadResponse>(
            app.clone(),
            "/v1/blobs/dev-upload",
            serde_json::json!({
                "blob_id": blob_id,
                "bytes_base64": URL_SAFE_NO_PAD.encode(file_bytes),
                "size": file_bytes.len(),
                "encryption_header": null
            }),
            Some(&login.access_token),
        )
        .await?;

        let file_node_id = NodeId::new_v4();
        post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 0,
                "kind": {
                    "type": "create_node",
                    "node_id": file_node_id,
                    "parent_id": workspace.root_node_id,
                    "name": "ok.txt",
                    "kind": "file",
                    "initial_revision": null
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        let revision = file_revision(
            workspace.workspace_id,
            login.device_id,
            file_node_id,
            &blob_id,
        )?;
        post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 1,
                "kind": {
                    "type": "put_file_revision",
                    "node_id": file_node_id,
                    "base_revision_id": null,
                    "revision": revision
                }
            }),
            Some(&login.access_token),
        )
        .await?;

        let stale_revision = file_revision(
            workspace.workspace_id,
            login.device_id,
            file_node_id,
            &blob_id,
        )?;
        let response = post_json_raw(
            app.clone(),
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 2,
                "kind": {
                    "type": "put_file_revision",
                    "node_id": file_node_id,
                    "base_revision_id": null,
                    "revision": stale_revision
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error = error_value(response).await?;
        assert_eq!(error["error"]["code"], "revision_conflict");
        assert_eq!(
            error["error"]["details"]["current_revision_id"],
            serde_json::Value::String(revision.revision_id.to_string())
        );
        assert_eq!(
            error["error"]["details"]["base_revision_id"],
            serde_json::Value::Null
        );

        let mut duplicate_revision = revision.clone();
        duplicate_revision.base_revision_id = Some(revision.revision_id);
        let response = post_json_raw(
            app,
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 2,
                "kind": {
                    "type": "put_file_revision",
                    "node_id": file_node_id,
                    "base_revision_id": revision.revision_id,
                    "revision": duplicate_revision
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_code(response).await?, "invalid_operation");
        Ok(())
    }

    #[tokio::test]
    async fn commit_ops_apply_rules_and_env_records() -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::dev(RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?);
        let app = app_with_state(state.clone());
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "env-laptop",
                "platform": {"os": "linux"},
                "public_key": "env-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "env-ws"}),
            Some(&login.access_token),
        )
        .await?;
        let uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);
        let env_var_id = EnvVarId::new_v4();
        let fake_plaintext = "sk_test_fake_backend_must_not_store";
        let encrypted_payload = "xchacha20poly1305-envelope-without-secret";

        post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 0,
                "kind": {
                    "type": "set_rule",
                    "path_pattern": "node_modules/**",
                    "rule": {"action": "dependency-cache", "manager": "node", "scope": null}
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 1,
                "kind": {
                    "type": "set_env_var",
                    "env_var_id": env_var_id,
                    "encrypted_payload": encrypted_payload,
                    "metadata": {
                        "env_name": "API_KEY",
                        "environment": "dev",
                        "scope": {"type": "workspace"},
                        "secret_kind": "secret"
                    }
                }
            }),
            Some(&login.access_token),
        )
        .await?;

        let workspaces = state.workspaces.read().await;
        let stored = workspaces
            .get(&workspace.workspace_id)
            .ok_or("created workspace missing")?;
        assert_eq!(stored.rules.len(), 1);
        assert_eq!(stored.env_vars.len(), 1);
        assert_eq!(
            stored
                .env_vars
                .get(&env_var_id)
                .ok_or("env record missing")?
                .encrypted_payload,
            encrypted_payload
        );
        let stored_json = serde_json::to_string(stored)?;
        assert!(!stored_json.contains(fake_plaintext));
        drop(workspaces);

        let fetched = get_json::<FetchOpsResponse>(
            app.clone(),
            &format!("{uri}?after=0"),
            Some(&login.access_token),
        )
        .await?;
        let fetched_json = serde_json::to_string(&fetched)?;
        assert!(fetched_json.contains(encrypted_payload));
        assert!(!fetched_json.contains(fake_plaintext));

        post_json::<CommitOperationResponse>(
            app,
            &uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 2,
                "kind": {"type": "delete_env_var", "env_var_id": env_var_id}
            }),
            Some(&login.access_token),
        )
        .await?;
        let workspaces = state.workspaces.read().await;
        let stored = workspaces
            .get(&workspace.workspace_id)
            .ok_or("created workspace missing")?;
        assert!(stored.env_vars.is_empty());
        drop(workspaces);
        Ok(())
    }

    #[tokio::test]
    async fn manifest_fetch_returns_metadata_depth_and_pages_without_bytes(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let state = AppState::dev_with_blob_root(
            RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?,
            dir.path(),
        );
        let app = app_with_state(state);
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "manifest-laptop",
                "platform": {"os": "linux"},
                "public_key": "manifest-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "manifest-ws"}),
            Some(&login.access_token),
        )
        .await?;
        let ops_uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);
        let docs_id = NodeId::new_v4();
        let src_id = NodeId::new_v4();
        for (node_id, name) in [(docs_id, "docs"), (src_id, "src")] {
            post_json::<CommitOperationResponse>(
                app.clone(),
                &ops_uri,
                serde_json::json!({
                    "op_id": OpId::new_v4(),
                    "base_cursor": 0,
                    "kind": {
                        "type": "create_node",
                        "node_id": node_id,
                        "parent_id": workspace.root_node_id,
                        "name": name,
                        "kind": "directory",
                        "initial_revision": null
                    }
                }),
                Some(&login.access_token),
            )
            .await?;
        }

        let file_bytes = b"manifest bytes stay in blob store";
        let blob_id = format!("sha256:{}", hex_lower(&Sha256::digest(file_bytes)));
        post_json::<DevBlobUploadResponse>(
            app.clone(),
            "/v1/blobs/dev-upload",
            serde_json::json!({
                "blob_id": blob_id,
                "bytes_base64": URL_SAFE_NO_PAD.encode(file_bytes),
                "size": file_bytes.len(),
                "encryption_header": "manifest-header"
            }),
            Some(&login.access_token),
        )
        .await?;
        let file_node_id = NodeId::new_v4();
        let initial_revision = file_revision(
            workspace.workspace_id,
            login.device_id,
            file_node_id,
            &blob_id,
        )?;
        post_json::<CommitOperationResponse>(
            app.clone(),
            &ops_uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 2,
                "kind": {
                    "type": "create_node",
                    "node_id": file_node_id,
                    "parent_id": docs_id,
                    "name": "readme.md",
                    "kind": "file",
                    "initial_revision": initial_revision
                }
            }),
            Some(&login.access_token),
        )
        .await?;

        let manifest_uri = format!("/v1/workspaces/{}/manifest", workspace.workspace_id);
        let root_only: ManifestResponse = get_json(
            app.clone(),
            &format!("{manifest_uri}?path=&depth=0"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(root_only.nodes.len(), 1);
        assert_eq!(root_only.nodes[0].path, "");

        let first_page: ManifestResponse = get_json(
            app.clone(),
            &format!("{manifest_uri}?path=&depth=1&limit=2"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(first_page.nodes.len(), 2);
        assert!(first_page.has_more);
        assert_eq!(first_page.next_offset, Some(2));
        let second_page: ManifestResponse = get_json(
            app.clone(),
            &format!("{manifest_uri}?path=&depth=1&limit=2&offset=2"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(second_page.nodes.len(), 1);
        assert!(!second_page.has_more);

        let docs_manifest: ManifestResponse = get_json(
            app.clone(),
            &format!("{manifest_uri}?path=docs&depth=1"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(docs_manifest.nodes.len(), 2);
        let file_entry = docs_manifest
            .nodes
            .iter()
            .find(|entry| entry.path == "docs/readme.md")
            .ok_or("manifest file entry missing")?;
        assert!(file_entry.current_revision.is_some());
        let raw = serde_json::to_string(&docs_manifest)?;
        assert!(!raw.contains("bytes_base64"));
        assert!(!raw.contains(&URL_SAFE_NO_PAD.encode(file_bytes)));
        Ok(())
    }

    #[tokio::test]
    async fn websocket_events_notify_and_reconnect_after_commits(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::dev(RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?);
        let app = app_with_state(state.clone());
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "ws-laptop",
                "platform": {"os": "linux"},
                "public_key": "ws-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "ws-ws"}),
            Some(&login.access_token),
        )
        .await?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app_with_state(server_state)).await;
        });
        let ws_uri = format!(
            "ws://{addr}/v1/workspaces/{}/events/ws?access_token={}",
            workspace.workspace_id, login.access_token
        );
        let (mut socket, _) = connect_async(&ws_uri).await?;
        let ops_uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);

        post_json::<CommitOperationResponse>(
            app.clone(),
            &ops_uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 0,
                "kind": {
                    "type": "create_node",
                    "node_id": NodeId::new_v4(),
                    "parent_id": workspace.root_node_id,
                    "name": "first",
                    "kind": "directory",
                    "initial_revision": null
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        let event = recv_workspace_event(&mut socket).await?;
        assert_eq!(
            event,
            WorkspaceEvent::WorkspaceOpsAvailable {
                workspace_id: workspace.workspace_id,
                from_cursor: Cursor::new(1)?,
                to_cursor: Cursor::new(1)?,
            }
        );
        drop(socket);

        let (mut reconnected, _) = connect_async(&ws_uri).await?;
        post_json::<CommitOperationResponse>(
            app,
            &ops_uri,
            serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 1,
                "kind": {
                    "type": "create_node",
                    "node_id": NodeId::new_v4(),
                    "parent_id": workspace.root_node_id,
                    "name": "second",
                    "kind": "directory",
                    "initial_revision": null
                }
            }),
            Some(&login.access_token),
        )
        .await?;
        let event = recv_workspace_event(&mut reconnected).await?;
        assert_eq!(
            event,
            WorkspaceEvent::WorkspaceOpsAvailable {
                workspace_id: workspace.workspace_id,
                from_cursor: Cursor::new(2)?,
                to_cursor: Cursor::new(2)?,
            }
        );
        drop(reconnected);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn fetch_ops_pagination_has_more_and_next_cursor(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::dev(RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?);
        let app = app_with_state(state);
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "page-laptop",
                "platform": {"os": "linux"},
                "public_key": "page-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "page-ws"}),
            Some(&login.access_token),
        )
        .await?;
        let uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);

        // Commit three CreateNode operations.
        for idx in 0..3u32 {
            let body = serde_json::json!({
                "op_id": OpId::new_v4(),
                "base_cursor": 0,
                "kind": {
                    "type": "create_node",
                    "node_id": NodeId::new_v4(),
                    "parent_id": workspace.root_node_id,
                    "name": format!("dir{idx}"),
                    "kind": "directory",
                    "initial_revision": null
                }
            });
            post_json::<CommitOperationResponse>(
                app.clone(),
                &uri,
                body,
                Some(&login.access_token),
            )
            .await?;
        }

        // Page with limit=2 from since=0.
        let page: FetchOpsResponse = get_json(
            app.clone(),
            &format!("{uri}?since=0&limit=2"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(page.operations.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.next_cursor, Some(page.operations[1].cursor));
        // Cursors are ascending.
        assert!(page.operations[0].cursor < page.operations[1].cursor);

        // Next page from the next_cursor.
        let since = page.next_cursor.ok_or("next cursor missing")?.value();
        let page2: FetchOpsResponse = get_json(
            app,
            &format!("{uri}?since={since}&limit=2"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(page2.operations.len(), 1);
        assert!(!page2.has_more);
        assert_eq!(page2.next_cursor, None);
        Ok(())
    }

    #[tokio::test]
    async fn replay_from_cursor_zero_reconstructs_metadata_in_order(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let state = AppState::dev_with_blob_root(
            RedactedSecret::new(DEFAULT_DEV_SECRET.to_owned())?,
            dir.path(),
        );
        let app = app_with_state(state.clone());
        let login = post_json::<DevLoginResponse>(
            app.clone(),
            "/v1/auth/dev-login",
            serde_json::json!({
                "device_name": "replay-laptop",
                "platform": {"os": "linux"},
                "public_key": "replay-key"
            }),
            None,
        )
        .await?;
        let workspace = post_json::<CreateWorkspaceResponse>(
            app.clone(),
            "/v1/workspaces",
            serde_json::json!({"name": "replay-ws"}),
            Some(&login.access_token),
        )
        .await?;
        let uri = format!("/v1/workspaces/{}/ops", workspace.workspace_id);

        // Upload a blob so PutFileRevision can reference it.
        let file_bytes = b"hello replay";
        let blob_id = format!("sha256:{}", hex_lower(&Sha256::digest(file_bytes)));
        post_json::<DevBlobUploadResponse>(
            app.clone(),
            "/v1/blobs/dev-upload",
            serde_json::json!({
                "blob_id": blob_id,
                "bytes_base64": URL_SAFE_NO_PAD.encode(file_bytes),
                "size": file_bytes.len(),
                "encryption_header": null
            }),
            Some(&login.access_token),
        )
        .await?;

        // 1. Create a file node.
        let file_node_id = NodeId::new_v4();
        let create_body = serde_json::json!({
            "op_id": OpId::new_v4(),
            "base_cursor": 0,
            "kind": {
                "type": "create_node",
                "node_id": file_node_id,
                "parent_id": workspace.root_node_id,
                "name": "notes.txt",
                "kind": "file",
                "initial_revision": null
            }
        });
        let created = post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            create_body,
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(created.cursor.value(), 1);

        // 2. Put a file revision.
        let revision = file_revision(
            workspace.workspace_id,
            login.device_id,
            file_node_id,
            &blob_id,
        )?;
        let put_body = serde_json::json!({
            "op_id": OpId::new_v4(),
            "base_cursor": created.cursor.value(),
            "kind": {
                "type": "put_file_revision",
                "node_id": file_node_id,
                "base_revision_id": null,
                "revision": revision
            }
        });
        let put = post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            put_body,
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(put.cursor.value(), 2);

        // 3. Move the file.
        let move_body = serde_json::json!({
            "op_id": OpId::new_v4(),
            "base_cursor": put.cursor.value(),
            "kind": {
                "type": "move_node",
                "node_id": file_node_id,
                "old_parent_id": workspace.root_node_id,
                "old_name": "notes.txt",
                "new_parent_id": workspace.root_node_id,
                "new_name": "renamed.txt"
            }
        });
        let moved = post_json::<CommitOperationResponse>(
            app.clone(),
            &uri,
            move_body,
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(moved.cursor.value(), 3);

        // Fetch all ops from cursor 0 and verify ordering and reconstruction.
        let all: FetchOpsResponse = get_json(
            app.clone(),
            &format!("{uri}?since=0&limit=100"),
            Some(&login.access_token),
        )
        .await?;
        assert_eq!(all.operations.len(), 3);
        let cursors: Vec<i64> = all
            .operations
            .iter()
            .map(|committed| committed.cursor.value())
            .collect();
        assert_eq!(cursors, vec![1, 2, 3]);

        // Replay from cursor 0: rebuild a fresh in-memory tree by re-applying the
        // fetched operations in cursor order, seeded with the original root node so
        // node IDs line up. This proves the op log carries enough metadata to
        // reconstruct state and that ordering is preserved.
        let original = state.workspaces.read().await;
        let orig_ws = original
            .get(&workspace.workspace_id)
            .ok_or("original workspace missing")?;
        let mut replay_nodes = HashMap::new();
        replay_nodes.insert(orig_ws.root_node_id, orig_ws.root_node.clone());
        let mut replay_revisions: HashMap<RevisionId, NodeRevision> = HashMap::new();
        let mut replay_cursor = Cursor::new(0)?;
        drop(original);

        for committed in &all.operations {
            let replay_root = replay_nodes
                .get(&workspace.root_node_id)
                .ok_or("replay root missing")?
                .clone();
            let mut replay_ws = WorkspaceRecord {
                workspace_id: workspace.workspace_id,
                user_id: login.user_id,
                name: String::new(),
                root_node_id: workspace.root_node_id,
                current_cursor: replay_cursor,
                root_node: replay_root,
                nodes: std::mem::take(&mut replay_nodes),
                revisions: std::mem::take(&mut replay_revisions),
                rules: HashMap::new(),
                env_vars: HashMap::new(),
            };
            let known_blob = blob_id.clone();
            let blob_exists = move |id: &str| id == known_blob;
            apply_operation(&mut replay_ws, &committed.operation, &blob_exists)
                .map_err(|_| std::io::Error::other("replay apply should succeed"))?;
            replay_cursor = Cursor::new(replay_cursor.value() + 1)?;
            replay_nodes = std::mem::take(&mut replay_ws.nodes);
            replay_revisions = std::mem::take(&mut replay_ws.revisions);
        }

        // The replayed tree must match the backend-applied tree.
        let original = state.workspaces.read().await;
        let orig_ws = original
            .get(&workspace.workspace_id)
            .ok_or("original workspace missing")?;
        assert_eq!(&orig_ws.nodes, &replay_nodes);
        let replay_file = replay_nodes
            .get(&file_node_id)
            .ok_or("replay file missing")?;
        assert_eq!(replay_file.name, "renamed.txt");
        assert_eq!(
            replay_file.current_rev,
            orig_ws
                .nodes
                .get(&file_node_id)
                .ok_or("original file missing")?
                .current_rev
        );
        assert_eq!(replay_revisions.len(), orig_ws.revisions.len());
        drop(original);
        Ok(())
    }

    async fn recv_workspace_event<S>(
        socket: &mut S,
    ) -> Result<WorkspaceEvent, Box<dyn std::error::Error>>
    where
        S: Stream<Item = Result<TungsteniteMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await?
            .ok_or("websocket closed before event")??;
        let TungsteniteMessage::Text(text) = message else {
            return Err(std::io::Error::other("unexpected websocket message").into());
        };
        serde_json::from_str(&text).map_err(Into::into)
    }

    async fn post_json<T: DeserializeOwned>(
        app: Router,
        uri: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(builder.body(Body::from(body.to_string()))?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        decode_body(response).await
    }

    async fn get_json<T: DeserializeOwned>(
        app: Router,
        uri: &str,
        bearer: Option<&str>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app.oneshot(builder.body(Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        decode_body(response).await
    }

    async fn decode_body<T: DeserializeOwned>(
        response: Response,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        serde_json::from_slice(&bytes).map_err(Into::into)
    }

    struct DockerPostgres {
        id: String,
        database_url: String,
    }

    impl DockerPostgres {
        fn start() -> Result<Self, std::io::Error> {
            let id = docker_output(&[
                "run",
                "--rm",
                "--detach",
                "--publish",
                "127.0.0.1::5432",
                "--env",
                "POSTGRES_PASSWORD=fs2",
                "--env",
                "POSTGRES_USER=fs2",
                "--env",
                "POSTGRES_DB=fs2",
                "postgres:16-alpine",
            ])?;
            let port_args = ["port", id.as_str(), "5432/tcp"];
            let port_output = docker_output(&port_args)?;
            let port_line = port_output
                .lines()
                .next()
                .ok_or_else(|| std::io::Error::other("docker did not report postgres port"))?;
            let port = port_line
                .rsplit_once(':')
                .map_or(port_line, |(_, port)| port);
            Ok(Self {
                id,
                database_url: format!("postgres://fs2:fs2@127.0.0.1:{port}/fs2"),
            })
        }
    }

    impl Drop for DockerPostgres {
        fn drop(&mut self) {
            drop(
                std::process::Command::new("docker")
                    .args(["kill", self.id.as_str()])
                    .status(),
            );
        }
    }

    fn docker_output(args: &[&str]) -> Result<String, std::io::Error> {
        let output = std::process::Command::new("docker").args(args).output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "docker {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    async fn wait_for_postgres(
        database_url: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut last_error = String::new();
        for _ in 0..120 {
            match sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(database_url)
                .await
            {
                Ok(pool) => {
                    pool.close().await;
                    return Ok(());
                }
                Err(error) => {
                    last_error = error.to_string();
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        Err(std::io::Error::other(format!("postgres did not become ready: {last_error}")).into())
    }
}
