//! Dev-only auth: login endpoint, token issuance, and extraction middleware.
//!
//! **NOT FOR PRODUCTION.** The dev auth flow creates a test user and issues a
//! simple signed token. Real auth will use OAuth/passphrase + device key
//! enrollment. This is clearly marked as dev-only.

use std::sync::Arc;

use axum::extract::{FromRef, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use fs2_core::{DeviceId, UserId};
use serde::{Deserialize, Serialize};

use crate::store::{MemoryStore, StoreError};

/// App state shared across handlers.
#[derive(Clone, Debug)]
pub struct AppState {
    /// In-memory metadata store.
    pub store: Arc<MemoryStore>,
    /// Whether dev mode is enabled.
    pub dev_mode: bool,
}

impl FromRef<AppState> for Arc<MemoryStore> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.store)
    }
}

/// A dev-only login request.
#[derive(Debug, Deserialize)]
pub struct DevLoginRequest {
    /// Email for the test user. A new user is created if none exists.
    pub email: String,
    /// Device name.
    pub device_name: String,
    /// Device public key (raw bytes, hex-encoded).
    pub public_key_hex: String,
    /// Platform metadata.
    #[serde(default)]
    pub platform: serde_json::Value,
}

/// A dev-only login response.
#[derive(Debug, Serialize)]
pub struct DevLoginResponse {
    /// User ID.
    pub user_id: UserId,
    /// Device ID.
    pub device_id: DeviceId,
    /// Dev access token (opaque, not a real JWT).
    pub access_token: String,
}

/// Dev-only login handler.
///
/// Creates a test user (or reuses one by email), registers a device, and
/// issues a dev token. This endpoint is only mounted when `dev_mode` is true.
pub async fn dev_login(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<DevLoginRequest>,
) -> Response {
    if !state.dev_mode {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "unauthorized",
                    "message": "dev auth is not enabled"
                }
            })),
        )
            .into_response();
    }

    let Ok(public_key) = hex::decode(&req.public_key_hex) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "invalid_operation",
                    "message": "public_key_hex is not valid hex"
                }
            })),
        )
            .into_response();
    };

    // Create user (dev mode always creates a new user for simplicity).
    let user_id = match state.store.create_user(&req.email) {
        Ok(id) => id,
        Err(e) => return store_error_response(&e),
    };

    let device_id =
        match state
            .store
            .register_device(user_id, &req.device_name, public_key, req.platform)
        {
            Ok(id) => id,
            Err(e) => return store_error_response(&e),
        };

    // Dev token: base64(user_id:device_id). Not secure, dev only.
    let token = dev_token(user_id, device_id);

    (
        StatusCode::OK,
        axum::Json(DevLoginResponse {
            user_id,
            device_id,
            access_token: token,
        }),
    )
        .into_response()
}

/// Decode a dev token back to (`user_id`, `device_id`).
///
/// Returns `None` if the token is malformed.
#[must_use]
pub fn decode_dev_token(token: &str) -> Option<(UserId, DeviceId)> {
    let decoded = base64_decode(token)?;
    let s = String::from_utf8(decoded).ok()?;
    let (user_str, dev_str) = s.split_once('|')?;
    let user = user_str.parse::<UserId>().ok()?;
    let dev = dev_str.parse::<DeviceId>().ok()?;
    Some((user, dev))
}

/// Create a dev token from user and device IDs.
#[must_use]
pub fn dev_token(user_id: UserId, device_id: DeviceId) -> String {
    // Use `|` as separator since IDs display as `prefix:UUID` (containing `:`).
    let raw = format!("{user_id}|{device_id}");
    base64_encode(raw.as_bytes())
}

/// Extract device claims from the `Authorization: Bearer <token>` header.
///
/// In dev mode, the token is the dev token. In production, this would verify
/// a JWT. Returns `None` if the header is missing or the token is invalid.
pub fn extract_device_from_headers(
    headers: &HeaderMap,
    store: &MemoryStore,
) -> Option<(UserId, DeviceId)> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    let (user_id, device_id) = decode_dev_token(token)?;
    if !store.user_exists(&user_id) {
        return None;
    }
    if store.is_device_revoked(device_id) {
        return None;
    }
    Some((user_id, device_id))
}

fn store_error_response(e: &StoreError) -> Response {
    let fs2_err = e.to_fs2_error();
    let status =
        StatusCode::from_u16(fs2_err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let payload = fs2_err.payload();
    (status, axum::Json(serde_json::json!({ "error": payload }))).into_response()
}

// Minimal base64 helpers to avoid pulling in a base64 crate.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[((b0 & 0x03) << 4 | b1 >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0x0f) << 2 | b2 >> 6) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let val = u32::try_from(TABLE.iter().position(|&t| t == c)?).ok()?;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from(buf >> bits).ok()?);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_token_roundtrip() {
        let user = UserId::new();
        let dev = DeviceId::new();
        let token = dev_token(user, dev);
        let (u, d) = decode_dev_token(&token).unwrap();
        assert_eq!(u, user);
        assert_eq!(d, dev);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_dev_token("not-a-token").is_none());
        assert!(decode_dev_token("").is_none());
    }

    #[test]
    fn base64_roundtrip() {
        for input in [b"hello".as_slice(), b"x", b"ab", b"abc", b"\xff\x00\x7f"] {
            let encoded = base64_encode(input);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, input);
        }
    }
}
