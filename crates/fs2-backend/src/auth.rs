//! Dev-only authentication: JWT issuance and middleware.
//!
//! This is clearly marked as non-production. It provides a simple dev login
//! endpoint that creates a test user and issues a signed access token.

use std::sync::Arc;

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{BackendError, BackendResult};
use crate::routes::AppState;
use axum::extract::State;

/// JWT claims for an authenticated device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (user id).
    pub sub: String,
    /// Device id.
    pub device_id: String,
    /// Expiration time (UNIX timestamp).
    pub exp: usize,
    /// Issued at (UNIX timestamp).
    pub iat: usize,
}

/// Authentication state shared across handlers.
#[derive(Clone)]
pub struct AuthState {
    encoding_key: Arc<EncodingKey>,
    decoding_key: Arc<DecodingKey>,
    /// Whether dev-only auth is enabled.
    pub dev_auth: bool,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("dev_auth", &self.dev_auth)
            .finish_non_exhaustive()
    }
}

impl AuthState {
    /// Create a new auth state from a JWT secret string.
    ///
    /// # Errors
    /// Returns an error if the key cannot be parsed.
    pub fn new(secret: &str, dev_auth: bool) -> BackendResult<Self> {
        let encoding_key = EncodingKey::from_secret(secret.as_bytes());
        let decoding_key = DecodingKey::from_secret(secret.as_bytes());
        Ok(Self {
            encoding_key: Arc::new(encoding_key),
            decoding_key: Arc::new(decoding_key),
            dev_auth,
        })
    }

    /// Issue a JWT for the given user and device.
    ///
    /// # Errors
    /// Returns an error if token signing fails.
    pub fn issue_token(&self, user_id: Uuid, device_id: Uuid) -> BackendResult<String> {
        let now = Utc::now();
        let claims = Claims {
            sub: user_id.to_string(),
            device_id: device_id.to_string(),
            exp: (now + Duration::hours(1)).timestamp() as usize,
            iat: now.timestamp() as usize,
        };
        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| BackendError::internal(format!("jwt signing failed: {e}")))
    }

    /// Verify a JWT and return the claims.
    ///
    /// # Errors
    /// Returns an error if the token is invalid or expired.
    pub fn verify_token(&self, token: &str) -> BackendResult<Claims> {
        decode::<Claims>(token, &self.decoding_key, &Validation::default())
            .map(|data| data.claims)
            .map_err(|e| {
                BackendError::Domain(fs2_core::Fs2Error::new(
                    fs2_core::Fs2ErrorCode::Unauthorized,
                    format!("invalid token: {e}"),
                ))
            })
    }
}

/// Axum extractor that reads claims from request extensions.
/// This is set by the auth middleware.
#[derive(Debug, Clone)]
pub struct AuthClaims(pub Claims);

/// Axum middleware that extracts and verifies the JWT from the Authorization header.
/// On success, inserts `AuthClaims` into request extensions.
/// On failure, returns 401.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Skip auth for health, dev-login, and WebSocket events endpoints.
    // WebSocket clients cannot easily set Authorization headers, so the WS
    // handler authenticates via a `token` query parameter instead.
    let path = req.uri().path();
    if path == "/healthz" || path == "/v1/auth/dev-login" || path.ends_with("/events/ws") {
        return next.run(req).await;
    }
    // Extract Bearer token from Authorization header.
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let token = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            return axum::response::Response::builder()
                .status(axum::http::StatusCode::UNAUTHORIZED)
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "error": {
                            "code": "unauthorized",
                            "message": "missing or invalid Authorization header"
                        }
                    })
                    .to_string(),
                ))
                .unwrap();
        }
    };
    match state.auth.verify_token(token) {
        Ok(claims) => {
            // Reject revoked devices at the middleware so every authenticated
            // endpoint honors revocation, not just operation commit.
            let device_id = Uuid::parse_str(&claims.device_id).ok();
            if let Some(did) = device_id {
                if let Ok(device) = state.store.device_valid(fs2_core::DeviceId::from_uuid(did)) {
                    // device_valid returns Err(DeviceRevoked) for revoked
                    // devices, so Ok means the device is still active.
                    let _ = device;
                } else {
                    return axum::response::Response::builder()
                        .status(axum::http::StatusCode::UNAUTHORIZED)
                        .body(axum::body::Body::from(
                            serde_json::json!({
                                "error": {
                                    "code": "device_revoked",
                                    "message": "device has been revoked"
                                }
                            })
                            .to_string(),
                        ))
                        .unwrap();
                }
            }
            req.extensions_mut().insert(AuthClaims(claims));
            next.run(req).await
        }
        Err(e) => axum::response::Response::builder()
            .status(axum::http::StatusCode::UNAUTHORIZED)
            .body(axum::body::Body::from(
                serde_json::json!({
                    "error": {
                        "code": "unauthorized",
                        "message": e.to_string()
                    }
                })
                .to_string(),
            ))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let auth = AuthState::new("test-secret", true).unwrap();
        let user = Uuid::new_v4();
        let device = Uuid::new_v4();
        let token = auth.issue_token(user, device).unwrap();
        let claims = auth.verify_token(&token).unwrap();
        assert_eq!(claims.sub, user.to_string());
        assert_eq!(claims.device_id, device.to_string());
    }

    #[test]
    fn invalid_token_rejected() {
        let auth = AuthState::new("test-secret", true).unwrap();
        assert!(auth.verify_token("not.a.valid.token").is_err());
    }

    #[test]
    fn wrong_secret_rejected() {
        let auth1 = AuthState::new("secret1", true).unwrap();
        let auth2 = AuthState::new("secret2", true).unwrap();
        let token = auth1.issue_token(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        assert!(auth2.verify_token(&token).is_err());
    }
}
