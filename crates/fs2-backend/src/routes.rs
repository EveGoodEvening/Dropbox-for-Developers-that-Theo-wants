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
        .with_state(state)
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
    let state = AppState {
        store,
        auth,
        config: Arc::new(config),
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
        AppState {
            store,
            auth,
            config,
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
        let app = app(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/workspaces")
                    .header("content-type", "application/json")
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
}
