//! HTTP routes and server setup.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fs2_core::{Cursor, DeviceId, Operation, WorkspaceId};

use crate::auth::AuthState;
use crate::config::BackendConfig;
use crate::error::{BackendError, BackendResult};
use crate::store::MemoryStore;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// In-memory metadata store.
    pub store: Arc<MemoryStore>,
    /// Auth state.
    pub auth: Arc<AuthState>,
    /// Backend config.
    pub config: Arc<BackendConfig>,
    /// Blob store.
    pub blob_store: Arc<dyn crate::blob_store::BlobStore>,
}

/// Health response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Server status.
    pub status: &'static str,
    /// Server version.
    pub version: &'static str,
}

/// Build the router for the backend.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        // Dev auth
        .route("/v1/auth/dev-login", post(dev_login))
        // Devices
        .route("/v1/devices", get(list_devices).post(enroll_device))
        .route("/v1/devices/:device_id/revoke", post(revoke_device))
        // Workspaces
        .route("/v1/workspaces", get(list_workspaces).post(create_workspace))
        .route("/v1/workspaces/:workspace_id", get(get_workspace))
        // Operations
        .route(
            "/v1/workspaces/:workspace_id/ops",
            get(fetch_operations).post(commit_operation),
        )
        // Manifest
        .route(
            "/v1/workspaces/:workspace_id/manifest",
            get(fetch_manifest),
        )
        // Blobs (dev direct upload/download)
        .route("/v1/blobs/upload", post(upload_blob).layer(axum::extract::DefaultBodyLimit::disable()))
        .route("/v1/blobs/download", get(download_blob))
        .route("/v1/blobs/:blob_id/status", get(blob_status))
        // Env vars
        .route(
            "/v1/workspaces/:workspace_id/env",
            get(list_env_vars).post(set_env_var),
        )
        .route("/v1/workspaces/:workspace_id/env/:env_var_id", axum::routing::delete(delete_env_var))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, crate::auth::auth_middleware))
}

/// GET /healthz
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Dev login request.
#[derive(Debug, Deserialize)]
pub struct DevLoginRequest {
    /// Email for the test user.
    pub email: String,
    /// Device name.
    pub device_name: String,
    /// Device public key (hex).
    pub public_key: String,
}

/// Dev login response.
#[derive(Debug, Serialize)]
pub struct DevLoginResponse {
    /// User id.
    pub user_id: Uuid,
    /// Device id.
    pub device_id: Uuid,
    /// JWT access token.
    pub token: String,
}

/// POST /v1/auth/dev-login — dev-only endpoint that creates a test user and
/// device, and issues a JWT.
async fn dev_login(
    State(state): State<AppState>,
    Json(req): Json<DevLoginRequest>,
) -> BackendResult<Json<DevLoginResponse>> {
    if !state.config.dev_auth {
        return Err(BackendError::Domain(fs2_core::Fs2Error::new(
            fs2_core::Fs2ErrorCode::Unauthorized,
            "dev auth is disabled",
        )));
    }
    let user_id = state.store.create_dev_user(&req.email)?;
    let device = state
        .store
        .register_device(user_id, &req.device_name, &req.public_key)?;
    let token = state.auth.issue_token(user_id, device.id.as_uuid())?;
    Ok(Json(DevLoginResponse {
        user_id,
        device_id: device.id.as_uuid(),
        token,
    }))
}

/// POST /v1/devices — enroll a new device.
#[derive(Debug, Deserialize)]
pub struct EnrollDeviceRequest {
    /// User id (in production this comes from the token).
    pub user_id: Uuid,
    /// Device name.
    pub name: String,
    /// Device public key (hex).
    pub public_key: String,
}

/// Device response.
#[derive(Debug, Serialize)]
pub struct DeviceResponse {
    /// Device id.
    pub id: Uuid,
    /// Device name.
    pub name: String,
    /// Whether revoked.
    pub revoked: bool,
}

/// GET /v1/devices — list devices for a user.
#[derive(Debug, Deserialize)]
pub struct ListDevicesQuery {
    /// User id.
    pub user_id: Uuid,
}

async fn list_devices(
    State(state): State<AppState>,
    Query(q): Query<ListDevicesQuery>,
) -> BackendResult<Json<Vec<DeviceResponse>>> {
    let devices = state.store.list_devices(q.user_id)?;
    Ok(Json(
        devices
            .into_iter()
            .map(|d| DeviceResponse {
                id: d.id.as_uuid(),
                name: d.name,
                revoked: d.revoked,
            })
            .collect(),
    ))
}

/// POST /v1/devices — enroll a device.
async fn enroll_device(
    State(state): State<AppState>,
    Json(req): Json<EnrollDeviceRequest>,
) -> BackendResult<(StatusCode, Json<DeviceResponse>)> {
    let device = state
        .store
        .register_device(req.user_id, &req.name, &req.public_key)?;
    Ok((
        StatusCode::CREATED,
        Json(DeviceResponse {
            id: device.id.as_uuid(),
            name: device.name,
            revoked: device.revoked,
        }),
    ))
}

/// POST `/v1/devices/:device_id/revoke`
async fn revoke_device(
    State(state): State<AppState>,
    Path(device_id): Path<Uuid>,
) -> BackendResult<StatusCode> {
    state.store.revoke_device(DeviceId::from_uuid(device_id))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Create workspace request.
#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    /// User id (from token in production).
    pub user_id: Uuid,
    /// Device id.
    pub device_id: Uuid,
    /// Workspace name.
    pub name: String,
}

/// Workspace response.
#[derive(Debug, Serialize)]
pub struct WorkspaceResponse {
    /// Workspace id.
    pub id: Uuid,
    /// Workspace name.
    pub name: String,
    /// Root node id.
    pub root_node_id: Uuid,
    /// Current cursor.
    pub cursor: i64,
}

async fn create_workspace(
    State(state): State<AppState>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> BackendResult<(StatusCode, Json<WorkspaceResponse>)> {
    let ws =
        state
            .store
            .create_workspace(req.user_id, &req.name, DeviceId::from_uuid(req.device_id))?;
    Ok((
        StatusCode::CREATED,
        Json(WorkspaceResponse {
            id: ws.id.as_uuid(),
            name: ws.name,
            root_node_id: ws.root_node_id.as_uuid(),
            cursor: ws.cursor.as_i64(),
        }),
    ))
}

async fn list_workspaces(
    State(state): State<AppState>,
    Query(q): Query<ListDevicesQuery>,
) -> BackendResult<Json<Vec<WorkspaceResponse>>> {
    let workspaces = state.store.list_workspaces(q.user_id)?;
    Ok(Json(
        workspaces
            .into_iter()
            .map(|w| WorkspaceResponse {
                id: w.id.as_uuid(),
                name: w.name,
                root_node_id: w.root_node_id.as_uuid(),
                cursor: w.cursor.as_i64(),
            })
            .collect(),
    ))
}

async fn get_workspace(
    State(state): State<AppState>,
    Path(workspace_id): Path<Uuid>,
) -> BackendResult<Json<WorkspaceResponse>> {
    let ws = state
        .store
        .get_workspace(WorkspaceId::from_uuid(workspace_id))?;
    Ok(Json(WorkspaceResponse {
        id: ws.id.as_uuid(),
        name: ws.name,
        root_node_id: ws.root_node_id.as_uuid(),
        cursor: ws.cursor.as_i64(),
    }))
}

/// Commit operation request.
#[derive(Debug, Deserialize)]
pub struct CommitOpRequest {
    /// The operation to commit.
    pub operation: Operation,
}

/// Commit operation response.
#[derive(Debug, Serialize)]
pub struct CommitOpResponse {
    /// Assigned cursor.
    pub cursor: i64,
    /// The operation id.
    pub op_id: Uuid,
}

async fn commit_operation(
    State(state): State<AppState>,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<CommitOpRequest>,
) -> BackendResult<(StatusCode, Json<CommitOpResponse>)> {
    let mut op = req.operation;
    // Ensure workspace_id matches the path.
    op.workspace_id = WorkspaceId::from_uuid(workspace_id);
    let committed = state.store.commit_operation(op)?;
    Ok((
        StatusCode::OK,
        Json(CommitOpResponse {
            cursor: committed.cursor.as_i64(),
            op_id: committed.op.op_id.as_uuid(),
        }),
    ))
}

/// Fetch operations query.
#[derive(Debug, Deserialize)]
pub struct FetchOpsQuery {
    /// Fetch ops after this cursor.
    pub since: Option<i64>,
    /// Maximum number of ops to return.
    pub limit: Option<usize>,
}

/// Fetch operations response.
#[derive(Debug, Serialize)]
pub struct FetchOpsResponse {
    /// Operations in cursor order.
    pub operations: Vec<Operation>,
    /// Whether more operations are available.
    pub has_more: bool,
    /// Cursor of the last returned operation (for pagination).
    pub next_cursor: i64,
}

async fn fetch_operations(
    State(state): State<AppState>,
    Path(workspace_id): Path<Uuid>,
    Query(q): Query<FetchOpsQuery>,
) -> BackendResult<Json<FetchOpsResponse>> {
    let since = Cursor::from(q.since.unwrap_or(0));
    let limit = q.limit.unwrap_or(100);
    let (ops, has_more, next) =
        state
            .store
            .fetch_operations(WorkspaceId::from_uuid(workspace_id), since, limit)?;
    Ok(Json(FetchOpsResponse {
        operations: ops.into_iter().map(|c| c.op).collect(),
        has_more,
        next_cursor: next.as_i64(),
    }))
}

/// Manifest query parameters.
#[derive(Debug, Deserialize)]
pub struct ManifestQuery {
    /// Path within the workspace (empty for root).
    pub path: Option<String>,
    /// Depth: 0 for just the node, 1 for immediate children, etc.
    pub depth: Option<usize>,
}

/// GET `/v1/workspaces/:workspace_id/manifest` — fetch subtree metadata.
async fn fetch_manifest(
    State(state): State<AppState>,
    Path(workspace_id): Path<Uuid>,
    Query(q): Query<ManifestQuery>,
) -> BackendResult<Json<Vec<crate::store::ManifestEntry>>> {
    let path = q.path.unwrap_or_default();
    let depth = q.depth.unwrap_or(1);
    let entries = state
        .store
        .fetch_manifest(WorkspaceId::from_uuid(workspace_id), &path, depth)?;
    Ok(Json(entries))
}

/// POST /v1/blobs/upload — direct blob upload (dev mode).
async fn upload_blob(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> BackendResult<StatusCode> {
    let blob_id = headers
        .get("x-blob-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            BackendError::Domain(fs2_core::Fs2Error::new(
                fs2_core::Fs2ErrorCode::InvalidOperation,
                "missing x-blob-id header",
            ))
        })?;
    let computed_blob_id = fs2_crypto::compute_blob_id(&body);
    if computed_blob_id != blob_id {
        return Err(BackendError::Domain(fs2_core::Fs2Error::new(
            fs2_core::Fs2ErrorCode::InvalidOperation,
            "blob hash mismatch: x-blob-id does not match computed hash",
        )));
    }
    state
        .blob_store
        .put(blob_id, body)
        .await
        .map_err(|e| BackendError::internal(format!("blob upload failed: {e}")))?;
    Ok(StatusCode::CREATED)
}

/// GET /v1/blobs/download — direct blob download (dev mode).
async fn download_blob(
    State(state): State<AppState>,
    Query(q): Query<DownloadBlobQuery>,
) -> BackendResult<axum::body::Body> {
    // Check if blob exists first to return proper error code.
    let exists = state
        .blob_store
        .exists(&q.blob_id)
        .await
        .map_err(|e| BackendError::internal(format!("blob status check failed: {e}")))?;
    if !exists {
        return Err(BackendError::Domain(fs2_core::Fs2Error::new(
            fs2_core::Fs2ErrorCode::BlobMissing,
            "blob not found in store",
        )));
    }
    let data = state
        .blob_store
        .get(&q.blob_id)
        .await
        .map_err(|e| BackendError::internal(format!("blob download failed: {e}")))?;
    // Verify blob hash matches the blob ID.
    if !fs2_crypto::verify_blob_id(&data, &q.blob_id) {
        return Err(BackendError::Domain(fs2_core::Fs2Error::new(
            fs2_core::Fs2ErrorCode::BlobMissing,
            "blob hash verification failed: data does not match blob ID",
        )));
    }
    Ok(axum::body::Body::from(data))
}

/// Query params for blob download.
#[derive(Debug, Deserialize)]
pub struct DownloadBlobQuery {
    /// Blob ID to download.
    pub blob_id: String,
}

/// GET `/v1/blobs/:blob_id/status` — check if a blob exists.
async fn blob_status(
    State(state): State<AppState>,
    Path(blob_id): Path<String>,
) -> BackendResult<Json<serde_json::Value>> {
    let exists = state
        .blob_store
        .exists(&blob_id)
        .await
        .map_err(|e| BackendError::internal(format!("blob status failed: {e}")))?;
    Ok(Json(serde_json::json!({ "exists": exists })))
}

/// Env var response (value redacted).
#[derive(Debug, Serialize)]
pub struct EnvVarResponse {
    /// Env var id.
    pub id: Uuid,
    /// Variable name.
    pub name: String,
    /// Environment.
    pub environment: String,
    /// Project path.
    pub project_path: Option<String>,
    /// Redacted value.
    pub value_display: String,
}

/// Query params for listing env vars.
#[derive(Debug, Deserialize)]
pub struct ListEnvVarsQuery {
    /// Filter by project path.
    pub project_path: Option<String>,
    /// Filter by environment.
    pub environment: Option<String>,
}

/// GET `/v1/workspaces/:workspace_id/env` — list env vars.
async fn list_env_vars(
    State(state): State<AppState>,
    Path(workspace_id): Path<Uuid>,
    Query(q): Query<ListEnvVarsQuery>,
) -> BackendResult<Json<Vec<EnvVarResponse>>> {
    let vars = state.store.list_env_vars(
        WorkspaceId::from_uuid(workspace_id),
        q.project_path.as_deref(),
        q.environment.as_deref(),
    )?;
    Ok(Json(
        vars.into_iter()
            .map(|v| EnvVarResponse {
                id: v.id,
                name: v.name,
                environment: v.environment,
                project_path: v.project_path,
                value_display: "********".to_owned(),
            })
            .collect(),
    ))
}

/// Set env var request.
#[derive(Debug, Deserialize)]
pub struct SetEnvVarRequest {
    /// Project path (optional).
    pub project_path: Option<String>,
    /// Environment name.
    pub environment: String,
    /// Variable name.
    pub name: String,
    /// Encrypted value (base64).
    pub encrypted_value: String,
}

/// POST `/v1/workspaces/:workspace_id/env` — set an env var.
async fn set_env_var(
    State(state): State<AppState>,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<SetEnvVarRequest>,
) -> BackendResult<(StatusCode, Json<EnvVarResponse>)> {
    let var = state.store.set_env_var(
        WorkspaceId::from_uuid(workspace_id),
        req.project_path.as_deref(),
        &req.environment,
        &req.name,
        &req.encrypted_value,
    )?;
    Ok((
        StatusCode::CREATED,
        Json(EnvVarResponse {
            id: var.id,
            name: var.name,
            environment: var.environment,
            project_path: var.project_path,
            value_display: "********".to_owned(),
        }),
    ))
}

/// DELETE `/v1/workspaces/:workspace_id/env/:env_var_id` — delete an env var.
async fn delete_env_var(
    State(state): State<AppState>,
    Path((workspace_id, env_var_id)): Path<(Uuid, Uuid)>,
) -> BackendResult<StatusCode> {
    let _ = workspace_id; // workspace_id is in the path for REST consistency
    state.store.delete_env_var(env_var_id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Run the backend server.
///
/// # Errors
/// Returns an error if the server fails to bind or start.
pub async fn run_server(config: BackendConfig) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fs2_backend=debug,tower_http=debug".into()),
        )
        .init();

    let bind = config.bind.clone();
    let store = MemoryStore::shared();
    let auth = Arc::new(AuthState::new(&config.jwt_secret, config.dev_auth)?);
    let blob_store: Arc<dyn crate::blob_store::BlobStore> = match &config.object_store {
        crate::config::ObjectStoreConfig::Local { path } => {
            std::fs::create_dir_all(path).ok();
            Arc::new(crate::blob_store::LocalBlobStore::new(path))
        }
        crate::config::ObjectStoreConfig::S3 { .. } => {
            return Err(anyhow::anyhow!("S3 blob store not yet implemented"));
        }
    };
    let state = AppState {
        store,
        auth,
        config: Arc::new(config),
        blob_store,
    };

    let app = app(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("fs2-backend listening on {bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthState;
    use crate::config::BackendConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let store = MemoryStore::shared();
        let auth = Arc::new(AuthState::new("test-secret", true).unwrap());
        let config = Arc::new(BackendConfig::default());
        let tmp = tempfile::TempDir::new().unwrap();
        let blob_store: Arc<dyn crate::blob_store::BlobStore> =
            Arc::new(crate::blob_store::LocalBlobStore::new(tmp.path()));
        // Leak the temp dir so it persists for the test. (The test is short-lived.)
        std::mem::forget(tmp);
        AppState {
            store,
            auth,
            config,
            blob_store,
        }
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = app(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn dev_login_creates_user_and_device() {
        let app = app(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/dev-login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "email": "test@example.com",
                            "device_name": "laptop",
                            "public_key": "fake-key"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["token"].as_str().is_some());
        assert!(json["user_id"].as_str().is_some());
        assert!(json["device_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn create_workspace_returns_root_node() {
        let state = test_state();
        // First create a user and device.
        let user_id = state.store.create_dev_user("test@example.com").unwrap();
        let device = state
            .store
            .register_device(user_id, "laptop", "fake-key")
            .unwrap();
        // Issue a token for auth.
        let token = state
            .auth
            .issue_token(user_id, device.id.as_uuid())
            .unwrap();
        let app = app(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/workspaces")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(
                        serde_json::json!({
                            "user_id": user_id,
                            "device_id": device.id.as_uuid(),
                            "name": "test-ws"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"], "test-ws");
        assert!(json["root_node_id"].as_str().is_some());
        assert_eq!(json["cursor"], 0);
    }

    #[tokio::test]
    async fn blob_upload_download_roundtrip() {
        let state = test_state();
        let user_id = state.store.create_dev_user("test@example.com").unwrap();
        let device = state
            .store
            .register_device(user_id, "laptop", "fake-key")
            .unwrap();
        let token = state
            .auth
            .issue_token(user_id, device.id.as_uuid())
            .unwrap();
        let blob_id = fs2_crypto::compute_blob_id(b"test blob content");
        // Upload via HTTP with auth and correct hash.
        let app = app(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/blobs/upload")
                    .header("x-blob-id", &blob_id)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(b"test blob content".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        // Download and verify hash.
        let data = state.blob_store.get(&blob_id).await.unwrap();
        assert!(fs2_crypto::verify_blob_id(&data, &blob_id));
        assert_eq!(data.as_ref(), b"test blob content");
    }

    #[tokio::test]
    async fn corrupt_blob_rejected() {
        let state = test_state();
        let blob_id = fs2_crypto::compute_blob_id(b"original content");
        // Upload correct blob.
        state
            .blob_store
            .put(&blob_id, bytes::Bytes::from(b"original content".to_vec()))
            .await
            .unwrap();
        // Corrupt the blob by overwriting with different data.
        state
            .blob_store
            .put(&blob_id, bytes::Bytes::from(b"corrupted content".to_vec()))
            .await
            .unwrap();
        // Download should fail hash verification.
        let data = state.blob_store.get(&blob_id).await.unwrap();
        assert!(!fs2_crypto::verify_blob_id(&data, &blob_id));
    }
}
