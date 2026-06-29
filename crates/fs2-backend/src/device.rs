//! Device enrollment HTTP handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use fs2_core::DeviceId;
use serde::{Deserialize, Serialize};

use crate::auth::{extract_device_from_headers, AppState};
use crate::store::{DeviceInfo, StoreError};

/// Request to enroll a new device.
#[derive(Debug, Deserialize)]
pub struct EnrollDeviceRequest {
    /// Device display name.
    pub name: String,
    /// Device public key (hex-encoded).
    pub public_key_hex: String,
    /// Platform metadata.
    #[serde(default)]
    pub platform: serde_json::Value,
}

/// Response from device enrollment.
#[derive(Debug, Serialize)]
pub struct EnrollDeviceResponse {
    /// Newly assigned device ID.
    pub device_id: DeviceId,
}

/// Enroll a new device for the authenticated user.
pub async fn enroll_device(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<EnrollDeviceRequest>,
) -> Response {
    let Some((user_id, _)) = require_auth(&state, &headers) else {
        return unauthorized_response();
    };

    let Ok(public_key) = hex::decode(&req.public_key_hex) else {
        return invalid_response("public_key_hex is not valid hex");
    };

    match state
        .store
        .register_device(user_id, &req.name, public_key, req.platform)
    {
        Ok(device_id) => (StatusCode::OK, Json(EnrollDeviceResponse { device_id })).into_response(),
        Err(e) => store_error_response(&e),
    }
}

/// List devices for the authenticated user.
pub async fn list_devices(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some((user_id, _)) = require_auth(&state, &headers) else {
        return unauthorized_response();
    };

    let devices: Vec<DeviceInfo> = state.store.list_devices(user_id);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "devices": devices })),
    )
        .into_response()
}

/// Revoke a device.
pub async fn revoke_device(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(device_id_str): Path<String>,
) -> Response {
    let Some(_) = require_auth(&state, &headers) else {
        return unauthorized_response();
    };

    let Ok(device_id) = device_id_str.parse::<DeviceId>() else {
        return invalid_response("invalid device_id in path");
    };

    match state.store.revoke_device(device_id) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "revoked": true, "device_id": device_id })),
        )
            .into_response(),
        Err(e) => store_error_response(&e),
    }
}

fn require_auth(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Option<(fs2_core::UserId, DeviceId)> {
    extract_device_from_headers(headers, &state.store)
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": {
                "code": "unauthorized",
                "message": "missing or invalid auth token"
            }
        })),
    )
        .into_response()
}

fn invalid_response(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": {
                "code": "invalid_operation",
                "message": msg
            }
        })),
    )
        .into_response()
}

fn store_error_response(e: &StoreError) -> Response {
    let fs2_err = e.to_fs2_error();
    let status =
        StatusCode::from_u16(fs2_err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let payload = fs2_err.payload();
    (status, Json(serde_json::json!({ "error": payload }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{dev_token, AppState};
    use crate::store::MemoryStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn setup_app() -> AppState {
        AppState {
            store: MemoryStore::shared(),
            dev_mode: true,
        }
    }

    fn dev_login(state: &AppState, email: &str) -> (fs2_core::UserId, DeviceId, String) {
        let store = Arc::clone(&state.store);
        let user = store.create_user(email).unwrap();
        let dev = store
            .register_device(user, "dev1", vec![1, 2, 3], serde_json::json!({}))
            .unwrap();
        let token = dev_token(user, dev);
        (user, dev, token)
    }

    #[tokio::test]
    async fn enroll_and_list_devices() {
        let state = setup_app();
        let (_, _, token) = dev_login(&state, "a@example.com");

        let app = axum::Router::new()
            .route("/v1/devices/enroll", axum::routing::post(enroll_device))
            .route("/v1/devices", axum::routing::get(list_devices))
            .with_state(state.clone());

        // Enroll a second device.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/devices/enroll")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "dev2",
                            "public_key_hex": "040506",
                            "platform": {"os": "macos"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // List devices.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/devices")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["devices"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn revoke_device_works() {
        let state = setup_app();
        let (user, _, token) = dev_login(&state, "b@example.com");

        // Enroll a second device to revoke.
        let dev2 = state
            .store
            .register_device(user, "dev2", vec![], serde_json::json!({}))
            .unwrap();

        let app = axum::Router::new()
            .route(
                "/v1/devices/{device_id}/revoke",
                axum::routing::post(revoke_device),
            )
            .with_state(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/devices/{dev2}/revoke"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK, "response body: {body_str}");
        assert!(state.store.is_device_revoked(dev2));
    }

    #[tokio::test]
    async fn missing_auth_returns_401() {
        let state = setup_app();
        let app = axum::Router::new()
            .route("/v1/devices", axum::routing::get(list_devices))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
