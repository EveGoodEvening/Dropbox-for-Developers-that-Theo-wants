#![allow(
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate
)]
//! Backend HTTP server skeleton with development-only auth/device endpoints.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bytes::Bytes;
use fs2_core::{DeviceId, UserId};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
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
use tokio::{net::TcpListener, sync::RwLock};
use tracing::info;
use tracing_subscriber::{fmt as tracing_fmt, EnvFilter};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:3000";
pub const DEFAULT_DATABASE_URL: &str = "postgres://fs2:fs2@localhost:5432/fs2";
pub const DEFAULT_OBJECT_STORE: &str = "local:./.fs2-dev/blobs";
const DEFAULT_DEV_SECRET: &str = "dev-only-secret-change-before-production";
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
        let object_store = ObjectStoreConfig::parse(
            lookup("FS2_OBJECT_STORE")
                .as_deref()
                .unwrap_or(DEFAULT_OBJECT_STORE),
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
    Local { root: PathBuf },
    S3 { bucket: String, endpoint: String },
}

impl ObjectStoreConfig {
    fn parse(value: &str) -> Result<Self, ConfigError> {
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
            return Ok(Self::S3 {
                bucket: bucket.to_owned(),
                endpoint: endpoint.to_owned(),
            });
        }

        Err(ConfigError::ObjectStore(
            "object store must start with local: or s3:".to_owned(),
        ))
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
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "blob store I/O error: {error}"),
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
}

impl AppState {
    pub fn dev(jwt_secret: RedactedSecret) -> Self {
        Self {
            jwt_secret,
            dev_user_id: UserId::new_v4(),
            devices: Arc::new(RwLock::new(HashMap::new())),
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
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Unauthorized(message) => {
                (StatusCode::UNAUTHORIZED, "unauthorized", message.to_owned())
            }
            Self::DeviceRevoked => (
                StatusCode::UNAUTHORIZED,
                "device_revoked",
                "device token has been revoked".to_owned(),
            ),
            Self::InvalidRequest(message) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                message.to_owned(),
            ),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", message),
        };
        (
            status,
            Json(ErrorResponse {
                error: ErrorBody { code, message },
            }),
        )
            .into_response()
    }
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
        .with_state(state)
}

pub async fn serve(
    config: BackendConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(bind_addr = %config.bind_addr, "fs2-backend listening");
    let state = AppState::dev(config.jwt_secret);
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

fn validate_device_fields(name: &str, public_key: &str) -> Result<(), ApiError> {
    if name.trim().is_empty() {
        return Err(ApiError::InvalidRequest("device name must not be empty"));
    }
    if public_key.trim().is_empty() {
        return Err(ApiError::InvalidRequest("public key must not be empty"));
    }
    Ok(())
}

async fn authenticate(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedDevice, ApiError> {
    let token = bearer_token(headers)?;
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
    use super::*;
    use axum::{body::Body, http::Request};
    use serde::de::DeserializeOwned;
    use tempfile::TempDir;
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
    fn config_parses_object_store_and_rejects_bad_values() {
        let s3 = ObjectStoreConfig::parse("s3:bucket@http://localhost:9000");
        assert!(matches!(s3, Ok(ObjectStoreConfig::S3 { .. })));
        assert!(ObjectStoreConfig::parse("s3:bucket").is_err());
        assert!(ObjectStoreConfig::parse("local:").is_err());
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
}
