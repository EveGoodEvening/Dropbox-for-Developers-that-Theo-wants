//! Axum application: router, health endpoint, and server runner.

use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use crate::auth::{dev_login, AppState};
use crate::config::BackendConfig;
use crate::device;
use crate::store::MemoryStore;

/// Health check response.
#[derive(Debug, serde::Serialize)]
pub struct HealthResponse {
    /// Service status.
    pub status: &'static str,
    /// Schema version.
    pub version: u32,
    /// Whether the in-memory store is in use.
    pub using_memory_store: bool,
}

/// `GET /v1/health` handler.
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: 1,
        using_memory_store: state.store.is_memory(),
    })
}

/// Build the Axum router from configuration.
///
/// # Panics
/// Does not panic; returns a ready-to-serve router.
pub fn build_router(config: &BackendConfig) -> Router {
    let state = AppState {
        store: MemoryStore::shared(),
        dev_mode: config.dev_mode,
    };

    let mut app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/devices/enroll", post(device::enroll_device))
        .route("/v1/devices", get(device::list_devices))
        .route(
            "/v1/devices/{device_id}/revoke",
            post(device::revoke_device),
        );

    if config.dev_mode {
        app = app.route("/v1/auth/dev-login", post(dev_login));
        info!("dev auth endpoints mounted at /v1/auth/dev-login");
    }

    app.with_state(state)
}

/// Run the backend server until shutdown.
///
/// # Errors
/// Returns an error if the server fails to bind or encounters a fatal error.
pub async fn run_server(config: BackendConfig) -> Result<(), Box<dyn std::error::Error>> {
    let addr = config.bind.clone();
    info!(%addr, dev_mode = config.dev_mode, "starting fs2-backend");

    let app = build_router(&config);
    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("server stopped");
    Ok(())
}

/// Wait for Ctrl+C or SIGTERM to trigger graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received Ctrl+C, shutting down"),
        () = terminate => info!("received SIGTERM, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let config = BackendConfig::default();
        let app = build_router(&config);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], 1);
        assert_eq!(json["using_memory_store"], true);
    }

    #[tokio::test]
    async fn dev_login_endpoint_works() {
        let config = BackendConfig::default();
        let app = build_router(&config);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/dev-login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "email": "test@example.com",
                            "device_name": "macbook",
                            "public_key_hex": "01020304",
                            "platform": {"os": "macos"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["user_id"].is_string());
        assert!(json["device_id"].is_string());
        assert!(json["access_token"].is_string());
    }

    #[tokio::test]
    async fn dev_login_disabled_when_dev_mode_false() {
        let config = BackendConfig {
            dev_mode: false,
            ..BackendConfig::default()
        };
        let app = build_router(&config);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/dev-login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "email": "test@example.com",
                            "device_name": "macbook",
                            "public_key_hex": "01020304"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Route is not mounted when dev_mode is false.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
