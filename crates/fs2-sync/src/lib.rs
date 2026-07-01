#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Sync-side helpers that prepare local data for backend upload.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use fs2_core::{
    BlobId, BlobStatusResponse, CommitOperationRequest, CommitOperationResponse, Cursor,
    DevBlobDownloadRequest, DevBlobDownloadResponse, DevBlobUploadRequest, DevBlobUploadResponse,
    ErrorEnvelope, FetchOpsResponse, ManifestResponse, Operation, RuleAction, WorkspaceEvent,
    WorkspaceId, WorkspacePath,
};
use fs2_crypto::{encrypt_blob, CryptoError, EncryptionHeader, WorkspaceContentKey};
use fs2_rules::{EvaluationPurpose, RuleEngine, RulePathKind};
use serde::{de::DeserializeOwned, Deserialize};
use std::{fmt, io::Read as _, io::Write as _, net::TcpStream, thread, time::Duration};
use tungstenite::{stream::MaybeTlsStream, WebSocket};
use url::Url;

#[derive(Debug)]
pub enum UploadPlanningError {
    Rules(fs2_rules::RuleError),
    Crypto(CryptoError),
}

impl std::fmt::Display for UploadPlanningError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rules(error) => write!(formatter, "rule evaluation failed: {error}"),
            Self::Crypto(error) => write!(formatter, "blob encryption failed: {error}"),
        }
    }
}

impl std::error::Error for UploadPlanningError {}

impl From<fs2_rules::RuleError> for UploadPlanningError {
    fn from(error: fs2_rules::RuleError) -> Self {
        Self::Rules(error)
    }
}

impl From<CryptoError> for UploadPlanningError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

pub fn plan_file_upload(
    path: &WorkspacePath,
    plaintext: &[u8],
    content_key: &WorkspaceContentKey,
    rules: &RuleEngine,
) -> Result<Option<BlobUploadPlan>, UploadPlanningError> {
    if contains_git_segment(path) {
        return Ok(None);
    }
    let resolution = rules.resolve(
        path,
        RulePathKind::File,
        EvaluationPurpose::NewLocalCreate,
        None,
    )?;
    if !is_uploadable_action(resolution.effective_rule.action) {
        return Ok(None);
    }
    BlobUploadPlan::from_plaintext(plaintext, content_key)
        .map(Some)
        .map_err(UploadPlanningError::from)
}

const fn is_uploadable_action(action: RuleAction) -> bool {
    matches!(
        action,
        RuleAction::Normal | RuleAction::Lazy | RuleAction::Pin
    )
}

fn contains_git_segment(path: &WorkspacePath) -> bool {
    path.segments().any(|segment| segment == ".git")
}
/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-sync"
}

/// Ciphertext-only payload that may be sent to the object store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobUploadPlan {
    pub blob_id: BlobId,
    pub ciphertext: Vec<u8>,
    pub plaintext_size: u64,
    pub encryption_header: EncryptionHeader,
}

impl BlobUploadPlan {
    /// Encrypts plaintext before producing any uploadable object-store payload.
    pub fn from_plaintext(
        plaintext: &[u8],
        content_key: &WorkspaceContentKey,
    ) -> Result<Self, CryptoError> {
        let encrypted = encrypt_blob(plaintext, content_key)?;
        Ok(Self {
            blob_id: encrypted.blob_id,
            ciphertext: encrypted.ciphertext,
            plaintext_size: plaintext.len() as u64,
            encryption_header: encrypted.header,
        })
    }
}

/// Retry settings for transient API failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub initial_backoff: Duration,
}

impl RetryPolicy {
    #[must_use]
    pub const fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff: Duration::ZERO,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(25),
        }
    }
}

/// Blocking API client used by the daemon sync loops.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiClient {
    base_url: Url,
    access_token: String,
    retry_policy: RetryPolicy,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiClient")
            .field("base_url", &self.base_url)
            .field("access_token", &"<redacted>")
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl ApiClient {
    pub fn new(base_url: &str, access_token: impl Into<String>) -> Result<Self, ApiClientError> {
        Self::with_retry_policy(base_url, access_token, RetryPolicy::default())
    }

    pub fn with_retry_policy(
        base_url: &str,
        access_token: impl Into<String>,
        retry_policy: RetryPolicy,
    ) -> Result<Self, ApiClientError> {
        let base_url = Url::parse(base_url).map_err(|error| ApiClientError::InvalidUrl {
            url: base_url.to_owned(),
            message: error.to_string(),
        })?;
        match base_url.scheme() {
            "http" => {}
            scheme => return Err(ApiClientError::UnsupportedScheme(scheme.to_owned())),
        }
        let max_attempts = retry_policy.max_attempts.max(1);
        Ok(Self {
            base_url,
            access_token: access_token.into(),
            retry_policy: RetryPolicy {
                max_attempts,
                initial_backoff: retry_policy.initial_backoff,
            },
        })
    }

    pub fn submit_operation(
        &self,
        workspace_id: WorkspaceId,
        operation: &Operation,
    ) -> Result<CommitOperationResponse, ApiClientError> {
        if operation.workspace_id != workspace_id {
            return Err(ApiClientError::WorkspaceMismatch {
                argument: workspace_id,
                operation: operation.workspace_id,
            });
        }
        let request = CommitOperationRequest {
            op_id: operation.op_id,
            base_cursor: operation.base_cursor,
            kind: operation.kind.clone(),
            created_at: operation.created_at,
        };
        self.post_json(
            &["v1", "workspaces", &workspace_id.to_string(), "ops"],
            &request,
        )
    }

    pub fn fetch_operations(
        &self,
        workspace_id: WorkspaceId,
        since: Cursor,
        limit: Option<u32>,
    ) -> Result<FetchOpsResponse, ApiClientError> {
        let mut query = vec![("since", since.value().to_string())];
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        self.get_json(
            &["v1", "workspaces", &workspace_id.to_string(), "ops"],
            &query,
        )
    }

    pub fn fetch_manifest(
        &self,
        workspace_id: WorkspaceId,
        path: &WorkspacePath,
        depth: Option<u32>,
        limit: Option<u32>,
        offset: Option<usize>,
    ) -> Result<ManifestResponse, ApiClientError> {
        let mut query = vec![("path", path.as_str().to_owned())];
        if let Some(depth) = depth {
            query.push(("depth", depth.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(offset) = offset {
            query.push(("offset", offset.to_string()));
        }
        self.get_json(
            &["v1", "workspaces", &workspace_id.to_string(), "manifest"],
            &query,
        )
    }

    pub fn upload_blob(
        &self,
        workspace_id: WorkspaceId,
        plan: &BlobUploadPlan,
    ) -> Result<DevBlobUploadResponse, ApiClientError> {
        let encryption_header = serde_json::to_string(&plan.encryption_header)
            .map_err(|error| ApiClientError::Json(error.to_string()))?;
        let request = DevBlobUploadRequest {
            blob_id: plan.blob_id.to_string(),
            workspace_id,
            bytes_base64: URL_SAFE_NO_PAD.encode(&plan.ciphertext),
            size: plan.ciphertext.len() as u64,
            encryption_header: Some(encryption_header),
        };
        self.post_json(&["v1", "blobs", "dev-upload"], &request)
    }

    pub fn download_blob(&self, blob_id: &BlobId) -> Result<DownloadedBlob, ApiClientError> {
        let response: DevBlobDownloadResponse = self.post_json(
            &["v1", "blobs", "dev-download"],
            &DevBlobDownloadRequest {
                blob_id: blob_id.to_string(),
            },
        )?;
        let bytes = URL_SAFE_NO_PAD
            .decode(response.bytes_base64.as_bytes())
            .map_err(|error| ApiClientError::InvalidResponse(error.to_string()))?;
        Ok(DownloadedBlob {
            blob_id: response.blob_id,
            bytes,
            size: response.size,
            encryption_header: response.encryption_header,
        })
    }

    pub fn blob_status(&self, blob_id: &BlobId) -> Result<BlobStatusResponse, ApiClientError> {
        self.get_json(&["v1", "blobs", &blob_id.to_string(), "status"], &[])
    }

    pub fn connect_workspace_events(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceEventListener, ApiClientError> {
        let mut url = self.endpoint_url(
            &[
                "v1",
                "workspaces",
                &workspace_id.to_string(),
                "events",
                "ws",
            ],
            &[],
        )?;
        url.set_scheme("ws")
            .map_err(|()| ApiClientError::UnsupportedScheme("ws".to_owned()))?;
        url.query_pairs_mut()
            .append_pair("access_token", &self.access_token);
        let (socket, _) = tungstenite::connect(url.as_str())
            .map_err(|error| ApiClientError::Transport(error.to_string()))?;
        Ok(WorkspaceEventListener { socket })
    }

    pub fn next_workspace_event(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceEvent, ApiClientError> {
        self.connect_workspace_events(workspace_id)?.next_event()
    }

    fn get_json<T: DeserializeOwned>(
        &self,
        path: &[&str],
        query: &[(&str, String)],
    ) -> Result<T, ApiClientError> {
        self.request_json("GET", path, query, None::<&()>)
    }

    fn post_json<T: DeserializeOwned>(
        &self,
        path: &[&str],
        body: &impl serde::Serialize,
    ) -> Result<T, ApiClientError> {
        self.request_json("POST", path, &[] as &[(&str, String)], Some(body))
    }

    fn request_json<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &[&str],
        query: &[(&str, String)],
        body: Option<&impl serde::Serialize>,
    ) -> Result<T, ApiClientError> {
        let body = body
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| ApiClientError::Json(error.to_string()))?;
        let mut attempt = 0;
        let mut backoff = self.retry_policy.initial_backoff;
        loop {
            attempt += 1;
            match self.request_once(method, path, query, body.as_deref()) {
                Ok(response) => {
                    return serde_json::from_str(&response)
                        .map_err(|error| ApiClientError::InvalidResponse(error.to_string()))
                }
                Err(error) if error.is_transient() && attempt < self.retry_policy.max_attempts => {
                    if !backoff.is_zero() {
                        thread::sleep(backoff);
                        backoff = backoff.saturating_mul(2);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn request_once(
        &self,
        method: &str,
        path: &[&str],
        query: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<String, ApiClientError> {
        let url = self.endpoint_url(path, query)?;
        let host = url.host_str().ok_or_else(|| ApiClientError::InvalidUrl {
            url: url.to_string(),
            message: "missing host".to_owned(),
        })?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ApiClientError::InvalidUrl {
                url: url.to_string(),
                message: "missing port".to_owned(),
            })?;
        let mut request_target = url.path().to_owned();
        if let Some(query) = url.query() {
            request_target.push('?');
            request_target.push_str(query);
        }
        let body = body.unwrap_or_default();
        let mut stream = TcpStream::connect((host, port))
            .map_err(|error| ApiClientError::Transport(error.to_string()))?;
        write!(
            stream,
            "{method} {request_target} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {}\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.access_token,
            body.len()
        )
        .map_err(|error| ApiClientError::Transport(error.to_string()))?;
        if body.is_empty() {
            stream
                .write_all(b"\r\n")
                .map_err(|error| ApiClientError::Transport(error.to_string()))?;
        } else {
            stream
                .write_all(b"Content-Type: application/json\r\n\r\n")
                .and_then(|()| stream.write_all(body.as_bytes()))
                .map_err(|error| ApiClientError::Transport(error.to_string()))?;
        }
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(|error| ApiClientError::Transport(error.to_string()))?;
        let (head, payload) = response.split_once("\r\n\r\n").ok_or_else(|| {
            ApiClientError::InvalidResponse("missing HTTP response body".to_owned())
        })?;
        let status = parse_status(head)?;
        if (200..300).contains(&status) {
            Ok(payload.to_owned())
        } else {
            Err(ApiClientError::HttpStatus {
                status,
                structured: parse_structured_error(payload),
                legacy: parse_legacy_error(payload),
                raw_body: payload.to_owned(),
            })
        }
    }

    fn endpoint_url(&self, path: &[&str], query: &[(&str, String)]) -> Result<Url, ApiClientError> {
        let mut url = self.base_url.clone();
        {
            let mut segments =
                url.path_segments_mut()
                    .map_err(|()| ApiClientError::InvalidUrl {
                        url: self.base_url.to_string(),
                        message: "base URL cannot be a base".to_owned(),
                    })?;
            segments.clear();
            for segment in path {
                segments.push(segment);
            }
        }
        url.query_pairs_mut().clear();
        for (key, value) in query {
            url.query_pairs_mut().append_pair(key.as_ref(), value);
        }
        Ok(url)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedBlob {
    pub blob_id: String,
    pub bytes: Vec<u8>,
    pub size: u64,
    pub encryption_header: Option<String>,
}

pub struct WorkspaceEventListener {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WorkspaceEventListener {
    pub fn next_event(&mut self) -> Result<WorkspaceEvent, ApiClientError> {
        loop {
            let message = self
                .socket
                .read()
                .map_err(|error| ApiClientError::Transport(error.to_string()))?;
            if message.is_text() {
                return serde_json::from_str(
                    message
                        .to_text()
                        .map_err(|error| ApiClientError::InvalidResponse(error.to_string()))?,
                )
                .map_err(|error| ApiClientError::InvalidResponse(error.to_string()));
            }
        }
    }
}

impl fmt::Debug for WorkspaceEventListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceEventListener")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiClientError {
    InvalidUrl {
        url: String,
        message: String,
    },
    UnsupportedScheme(String),
    Transport(String),
    Json(String),
    InvalidResponse(String),
    WorkspaceMismatch {
        argument: WorkspaceId,
        operation: WorkspaceId,
    },
    HttpStatus {
        status: u16,
        structured: Option<Box<ErrorEnvelope>>,
        legacy: Option<(String, String)>,
        raw_body: String,
    },
}

impl ApiClientError {
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transport(_)
                | Self::HttpStatus {
                    status: 429 | 500..=599,
                    ..
                }
        )
    }
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl { url, message } => {
                write!(formatter, "invalid API URL {url}: {message}")
            }
            Self::UnsupportedScheme(scheme) => {
                write!(formatter, "unsupported API URL scheme: {scheme}")
            }
            Self::Transport(message) => write!(formatter, "API transport error: {message}"),
            Self::Json(message) => write!(formatter, "API JSON encode error: {message}"),
            Self::InvalidResponse(message) => write!(formatter, "invalid API response: {message}"),
            Self::WorkspaceMismatch {
                argument,
                operation,
            } => write!(
                formatter,
                "operation workspace_id {operation} does not match request workspace_id {argument}"
            ),
            Self::HttpStatus {
                status,
                structured,
                legacy,
                raw_body,
            } => {
                if let Some(envelope) = structured {
                    write!(
                        formatter,
                        "API returned HTTP {status}: {}",
                        envelope.error.message
                    )
                } else if let Some((code, message)) = legacy {
                    write!(formatter, "API returned HTTP {status}: {code}: {message}")
                } else {
                    write!(formatter, "API returned HTTP {status}: {raw_body}")
                }
            }
        }
    }
}

impl std::error::Error for ApiClientError {}

#[derive(Debug, Deserialize)]
struct LegacyErrorEnvelope {
    error: LegacyErrorBody,
}

#[derive(Debug, Deserialize)]
struct LegacyErrorBody {
    code: String,
    message: String,
}

fn parse_structured_error(payload: &str) -> Option<Box<ErrorEnvelope>> {
    serde_json::from_str(payload).map(Box::new).ok()
}

fn parse_status(head: &str) -> Result<u16, ApiClientError> {
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| ApiClientError::InvalidResponse("missing HTTP status".to_owned()))?;
    status
        .parse::<u16>()
        .map_err(|error| ApiClientError::InvalidResponse(error.to_string()))
}

#[must_use]
pub fn parse_legacy_error(payload: &str) -> Option<(String, String)> {
    let parsed: LegacyErrorEnvelope = serde_json::from_str(payload).ok()?;
    Some((parsed.error.code, parsed.error.message))
}
#[cfg(test)]
mod tests {
    use super::*;
    use fs2_crypto::{decrypt_blob, EncryptedBlob};

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-sync");
    }

    #[test]
    fn api_client_debug_redacts_token_and_rejects_workspace_mismatch(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client = ApiClient::with_retry_policy(
            "http://127.0.0.1:1",
            "super-secret-token",
            RetryPolicy::no_retry(),
        )?;
        let debug = format!("{client:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(debug.contains("<redacted>"));

        let operation_workspace = WorkspaceId::new_v4();
        let argument_workspace = WorkspaceId::new_v4();
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: operation_workspace,
            device_id: fs2_core::DeviceId::new_v4(),
            base_cursor: Cursor::new(0)?,
            kind: fs2_core::OperationKind::DeleteNode {
                node_id: fs2_core::NodeId::new_v4(),
                recursive: false,
            },
            created_at: chrono::Utc::now(),
        };

        let result = client.submit_operation(argument_workspace, &operation);
        assert!(matches!(
            result,
            Err(ApiClientError::WorkspaceMismatch { argument, operation })
                if argument == argument_workspace && operation == operation_workspace
        ));
        Ok(())
    }

    #[test]
    fn api_client_error_parsing_and_retry_classification() -> Result<(), Box<dyn std::error::Error>>
    {
        let structured = parse_structured_error(
            r#"{"error":{"code":"offline","message":"Backend is offline.","details":{"retry":"true"}}}"#,
        )
        .ok_or("structured error should parse")?;
        assert_eq!(structured.error.code, fs2_core::Fs2Error::Offline);
        assert_eq!(structured.error.details["retry"], "true");

        let legacy = parse_legacy_error(
            r#"{"error":{"code":"unauthorized","message":"missing access token"}}"#,
        );
        assert_eq!(
            legacy,
            Some(("unauthorized".to_owned(), "missing access token".to_owned()))
        );

        let legacy_status = ApiClientError::HttpStatus {
            status: 401,
            structured: None,
            legacy,
            raw_body: "legacy body".to_owned(),
        };
        assert_eq!(
            legacy_status.to_string(),
            "API returned HTTP 401: unauthorized: missing access token"
        );

        assert!(ApiClientError::Transport("connection reset".to_owned()).is_transient());
        assert!(ApiClientError::HttpStatus {
            status: 500,
            structured: None,
            legacy: None,
            raw_body: String::new(),
        }
        .is_transient());
        assert!(!ApiClientError::HttpStatus {
            status: 400,
            structured: None,
            raw_body: String::new(),
            legacy: None,
        }
        .is_transient());
        Ok(())
    }

    #[test]
    fn blob_upload_plan_never_contains_plaintext_payload() -> Result<(), CryptoError> {
        let key = WorkspaceContentKey::from_bytes([7; 32]);
        let plaintext = b"known secret source bytes";

        let plan = BlobUploadPlan::from_plaintext(plaintext, &key)?;

        assert_ne!(plan.ciphertext, plaintext);
        assert!(!plan
            .ciphertext
            .windows(plaintext.len())
            .any(|window| window == plaintext));
        assert_eq!(plan.plaintext_size, plaintext.len() as u64);
        assert_eq!(plan.encryption_header.aad, "fs2:blob:v1");
        assert_eq!(plan.blob_id.as_str(), plan.blob_id.to_string());
        let decrypted = decrypt_blob(
            &EncryptedBlob {
                blob_id: plan.blob_id,
                header: plan.encryption_header,
                ciphertext: plan.ciphertext,
            },
            &key,
        )?;
        assert_eq!(decrypted, plaintext);
        Ok(())
    }

    #[test]
    fn upload_planning_skips_git_internals_by_default() -> Result<(), Box<dyn std::error::Error>> {
        let key = WorkspaceContentKey::from_bytes([9; 32]);
        let rules = RuleEngine::new(fs2_rules::Config::default(), Vec::new())?;
        for path in [
            ".git/index",
            ".git/objects/pack/pack-0123456789abcdef0123456789abcdef01234567.pack",
            "vendor/lib/.git/index",
            ".git",
            "vendor/lib/.git",
        ] {
            let path = WorkspacePath::parse(path)?;
            assert_eq!(plan_file_upload(&path, b"git bytes", &key, &rules)?, None);
        }
        Ok(())
    }

    #[test]
    fn upload_planning_allows_normal_files() -> Result<(), Box<dyn std::error::Error>> {
        let key = WorkspaceContentKey::from_bytes([10; 32]);
        let rules = RuleEngine::new(fs2_rules::Config::default(), Vec::new())?;
        let path = WorkspacePath::parse("src/lib.rs")?;

        let plan = plan_file_upload(&path, b"source bytes", &key, &rules)?;

        assert!(plan.is_some());
        Ok(())
    }

    #[derive(Debug, Deserialize)]
    struct TestLoginResponse {
        access_token: String,
        device_id: String,
    }

    #[derive(Debug)]
    struct TestBackend {
        base_url: String,
        _blob_dir: tempfile::TempDir,
    }

    fn spawn_backend() -> Result<TestBackend, Box<dyn std::error::Error>> {
        let blob_dir = tempfile::tempdir()?;
        let state = fs2_backend::AppState::dev_with_blob_root(
            fs2_backend::RedactedSecret::new("test-dev-secret".to_owned())?,
            blob_dir.path(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, fs2_backend::app_with_state(state)).await;
        });
        Ok(TestBackend {
            base_url: format!("http://{addr}"),
            _blob_dir: blob_dir,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn api_client_replays_operations_and_blobs_from_backend(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let backend = spawn_backend()?;
        let bootstrap = ApiClient::with_retry_policy(
            &backend.base_url,
            "bootstrap-token",
            RetryPolicy::no_retry(),
        )?;
        let login: TestLoginResponse = bootstrap.post_json(
            &["v1", "auth", "dev-login"],
            &serde_json::json!({
                "device_name": "sync-test",
                "platform": {"os": "linux", "arch": "x86_64"},
                "public_key": "test-public-key"
            }),
        )?;
        let client = ApiClient::with_retry_policy(
            &backend.base_url,
            login.access_token.clone(),
            RetryPolicy::no_retry(),
        )?;
        let workspace: fs2_backend::CreateWorkspaceResponse = client.post_json(
            &["v1", "workspaces"],
            &serde_json::json!({"name": "api-client"}),
        )?;
        let workspace_id = workspace.workspace_id;
        let device_id = login.device_id.parse()?;

        let content_key = WorkspaceContentKey::from_bytes([42; 32]);
        let plaintext = b"backend must only receive ciphertext";
        let plan = BlobUploadPlan::from_plaintext(plaintext, &content_key)?;
        client.upload_blob(workspace_id, &plan)?;
        let status = client.blob_status(&plan.blob_id)?;
        assert!(status.exists);
        assert_eq!(status.size, Some(plan.ciphertext.len() as u64));
        let downloaded = client.download_blob(&plan.blob_id)?;
        assert_eq!(downloaded.bytes, plan.ciphertext);
        assert_ne!(downloaded.bytes, plaintext);

        let first_node = fs2_core::NodeId::new_v4();
        let first = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id,
            device_id,
            base_cursor: Cursor::new(0)?,
            kind: fs2_core::OperationKind::CreateNode {
                node_id: first_node,
                parent_id: workspace.root_node_id,
                name: "src".to_owned(),
                kind: fs2_core::NodeKind::Directory,
                initial_revision: None,
            },
            created_at: chrono::Utc::now(),
        };
        let committed = client.submit_operation(workspace_id, &first)?;
        assert_eq!(committed.cursor.value(), 1);

        let fetched = client.fetch_operations(workspace_id, Cursor::new(0)?, Some(100))?;
        assert_eq!(fetched.operations.len(), 1);
        assert_eq!(fetched.operations[0].operation.op_id, first.op_id);

        let mut store = fs2_daemon::LocalStore::in_memory()?;
        store.initialize_workspace(workspace_id, "api-client", workspace.root_node_id)?;
        for committed in &fetched.operations {
            store.apply_committed_operation(&committed.operation, committed.cursor)?;
        }
        assert!(store.get_node_by_path(workspace_id, "src")?.is_some());

        let manifest = client.fetch_manifest(
            workspace_id,
            &WorkspacePath::parse("")?,
            Some(1),
            Some(100),
            None,
        )?;
        assert!(manifest.nodes.iter().any(|entry| entry.path == "src"));

        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut listener = client.connect_workspace_events(workspace_id)?;
        std::thread::spawn(move || {
            let result = listener.next_event();
            let _ = event_tx.send(result);
        });
        let second = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id,
            device_id,
            base_cursor: Cursor::new(1)?,
            kind: fs2_core::OperationKind::CreateNode {
                node_id: fs2_core::NodeId::new_v4(),
                parent_id: workspace.root_node_id,
                name: "docs".to_owned(),
                kind: fs2_core::NodeKind::Directory,
                initial_revision: None,
            },
            created_at: chrono::Utc::now(),
        };
        client.submit_operation(workspace_id, &second)?;
        let event = event_rx.recv_timeout(Duration::from_secs(5))??;
        assert_eq!(
            event,
            WorkspaceEvent::WorkspaceOpsAvailable {
                workspace_id,
                from_cursor: Cursor::new(2)?,
                to_cursor: Cursor::new(2)?,
            }
        );
        Ok(())
    }
}
