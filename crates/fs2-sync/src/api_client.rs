//! API client for the fs2 backend.
//!
//! Provides authenticated HTTP requests, operation submit/fetch, and blob
//! upload/download. Used by the sync engine to communicate with the backend.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use fs2_core::{Cursor, Operation, WorkspaceId};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// API client for the fs2 backend.
#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    token: Option<String>,
    client: Client,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("base_url", &self.base_url)
            .field("has_token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

/// Response from dev login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevLoginResponse {
    /// User id.
    pub user_id: uuid::Uuid,
    /// Device id.
    pub device_id: uuid::Uuid,
    /// JWT token.
    pub token: String,
}

/// Response from workspace creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceResponse {
    /// Workspace id.
    pub id: uuid::Uuid,
    /// Workspace name.
    pub name: String,
    /// Root node id.
    pub root_node_id: uuid::Uuid,
    /// Current cursor.
    pub cursor: i64,
}

/// Response from operation commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitOpResponse {
    /// Assigned cursor.
    pub cursor: i64,
    /// Operation id.
    pub op_id: uuid::Uuid,
}

/// Response from operation fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchOpsResponse {
    /// Operations in cursor order.
    pub operations: Vec<Operation>,
    /// Whether more operations are available.
    pub has_more: bool,
    /// Cursor of the last returned operation.
    pub next_cursor: i64,
}

/// Error response from the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// The error payload.
    pub error: fs2_core::Fs2Error,
}

impl ApiClient {
    /// Create a new API client.
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: None,
            client,
        }
    }

    /// Set the auth token.
    #[must_use]
    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    /// Create a shared (Arc) client.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn build_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(ref token) = self.token {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {token}")) {
                headers.insert(AUTHORIZATION, val);
            }
        }
        headers
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Dev login: create a test user and device, get a token.
    ///
    /// # Errors
    /// Returns an error if the request fails or the response is not OK.
    pub async fn dev_login(
        &self,
        email: &str,
        device_name: &str,
        public_key: &str,
    ) -> Result<DevLoginResponse> {
        let resp = self
            .client
            .post(self.url("/v1/auth/dev-login"))
            .json(&serde_json::json!({
                "email": email,
                "device_name": device_name,
                "public_key": public_key,
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("dev_login failed: {status} {body}"));
        }
        let login: DevLoginResponse = resp.json().await?;
        Ok(login)
    }

    /// Create a workspace.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub async fn create_workspace(
        &self,
        user_id: uuid::Uuid,
        device_id: uuid::Uuid,
        name: &str,
    ) -> Result<WorkspaceResponse> {
        let resp = self
            .client
            .post(self.url("/v1/workspaces"))
            .headers(self.build_headers())
            .json(&serde_json::json!({
                "user_id": user_id,
                "device_id": device_id,
                "name": name,
            }))
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    /// List workspaces for a user.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub async fn list_workspaces(&self, user_id: uuid::Uuid) -> Result<Vec<WorkspaceResponse>> {
        let resp = self
            .client
            .get(self.url("/v1/workspaces"))
            .headers(self.build_headers())
            .query(&[("user_id", user_id.to_string())])
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    /// Submit an operation to the backend.
    ///
    /// # Errors
    /// Returns an error if the request fails or the backend rejects the operation.
    pub async fn commit_operation(
        &self,
        workspace_id: WorkspaceId,
        op: &Operation,
    ) -> Result<CommitOpResponse> {
        let resp = self
            .client
            .post(self.url(&format!("/v1/workspaces/{workspace_id}/ops")))
            .headers(self.build_headers())
            .json(&serde_json::json!({ "operation": op }))
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    /// Fetch operations since a cursor.
    ///
    /// # Errors
    /// Returns an error if the request fails.
    pub async fn fetch_operations(
        &self,
        workspace_id: WorkspaceId,
        since: Cursor,
        limit: usize,
    ) -> Result<FetchOpsResponse> {
        let resp = self
            .client
            .get(self.url(&format!("/v1/workspaces/{workspace_id}/ops")))
            .headers(self.build_headers())
            .query(&[
                ("since", since.as_i64().to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    /// Upload a blob (direct upload for dev mode).
    ///
    /// # Errors
    /// Returns an error if the upload fails.
    pub async fn upload_blob(&self, blob_id: &str, data: Vec<u8>) -> Result<()> {
        let resp = self
            .client
            .post(self.url("/v1/blobs/upload"))
            .headers(self.build_headers())
            .header("content-type", "application/octet-stream")
            .header("x-blob-id", blob_id)
            .body(data)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("blob upload failed: {status} {body}"));
        }
        Ok(())
    }

    /// Download a blob by ID.
    ///
    /// # Errors
    /// Returns an error if the download fails.
    pub async fn download_blob(&self, blob_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(self.url("/v1/blobs/download"))
            .headers(self.build_headers())
            .query(&[("blob_id", blob_id)])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("blob download failed: {status} {body}"));
        }
        let bytes = resp.bytes().await?.to_vec();
        Ok(bytes)
    }

    /// Check backend health.
    ///
    /// # Errors
    /// Returns an error if the health check fails.
    pub async fn health(&self) -> Result<bool> {
        let resp = self.client.get(self.url("/healthz")).send().await?;
        Ok(resp.status().is_success())
    }

    async fn parse_response<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            let result = resp.json().await?;
            Ok(result)
        } else {
            let body = resp.text().await.unwrap_or_default();
            debug!("API error: {status} {body}");
            Err(anyhow!("API error: {status} {body}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_construction() {
        let client = ApiClient::new("http://localhost:8787/");
        assert_eq!(client.base_url, "http://localhost:8787");
        assert!(client.token.is_none());
    }

    #[test]
    fn client_with_token() {
        let client = ApiClient::new("http://localhost:8787").with_token("test-token".to_owned());
        assert_eq!(client.token.as_deref(), Some("test-token"));
    }

    #[test]
    fn url_building() {
        let client = ApiClient::new("http://localhost:8787/");
        assert_eq!(client.url("/healthz"), "http://localhost:8787/healthz");
        assert_eq!(
            client.url("/v1/workspaces"),
            "http://localhost:8787/v1/workspaces"
        );
    }
}
