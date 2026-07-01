#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Local end-to-end test harnesses for FS2 sync flows.

use fs2_core::{Cursor, NodeId, NodeKind, Operation, OperationKind, RevisionContent, WorkspaceId};
use fs2_crypto::{decrypt_blob, EncryptedBlob, WorkspaceContentKey};
use fs2_daemon::LocalStore;
use fs2_sync::{ApiClient, BlobUploadPlan, InboundSync, RetryPolicy};
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};
use tempfile::TempDir;

#[derive(Debug)]
pub struct LocalBackend {
    pub base_url: String,
    blob_dir: TempDir,
}

impl LocalBackend {
    pub fn spawn() -> Result<Self, Box<dyn std::error::Error>> {
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
        Ok(Self {
            base_url: format!("http://{addr}"),
            blob_dir,
        })
    }

    #[must_use]
    pub fn blob_root(&self) -> &std::path::Path {
        self.blob_dir.path()
    }
}

#[derive(Debug)]
pub struct TwoClientHarness {
    pub backend: LocalBackend,
    pub workspace_id: WorkspaceId,
    pub root_node_id: NodeId,
    pub client_a: ApiClient,
    pub client_b: ApiClient,
    pub device_a_id: fs2_core::DeviceId,
    pub device_b_id: fs2_core::DeviceId,
    pub content_key: WorkspaceContentKey,
    origin_dir: TempDir,
    mirror_dir: TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedFile {
    pub path: String,
    pub operation: Operation,
    pub plan: BlobUploadPlan,
    pub plaintext: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct TestLoginResponse {
    access_token: String,
    device_id: fs2_core::DeviceId,
}

impl TwoClientHarness {
    pub fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let backend = LocalBackend::spawn()?;
        let bootstrap = ApiClient::with_retry_policy(
            &backend.base_url,
            "bootstrap-token",
            RetryPolicy::no_retry(),
        )?;
        let login_a = dev_login(&bootstrap, "client-a")?;
        let login_b = dev_login(&bootstrap, "client-b")?;
        let client_a = ApiClient::with_retry_policy(
            &backend.base_url,
            login_a.access_token,
            RetryPolicy::no_retry(),
        )?;
        let client_b = ApiClient::with_retry_policy(
            &backend.base_url,
            login_b.access_token,
            RetryPolicy::no_retry(),
        )?;
        let workspace: fs2_backend::CreateWorkspaceResponse = client_a.post_json(
            &["v1", "workspaces"],
            &serde_json::json!({"name": "two-client"}),
        )?;
        let origin_dir = tempfile::tempdir()?;
        let mirror_dir = tempfile::tempdir()?;
        let mut origin_store = LocalStore::open(state_path(&origin_dir))?;
        let mut mirror_store = LocalStore::open(state_path(&mirror_dir))?;
        origin_store.initialize_workspace(
            workspace.workspace_id,
            "two-client",
            workspace.root_node_id,
        )?;
        mirror_store.initialize_workspace(
            workspace.workspace_id,
            "two-client",
            workspace.root_node_id,
        )?;
        Ok(Self {
            backend,
            workspace_id: workspace.workspace_id,
            root_node_id: workspace.root_node_id,
            client_a,
            client_b,
            device_a_id: login_a.device_id,
            device_b_id: login_b.device_id,
            content_key: WorkspaceContentKey::from_bytes([11; 32]),
            origin_dir,
            mirror_dir,
        })
    }

    pub fn open_client_a_store(&self) -> Result<LocalStore, fs2_daemon::LocalStoreError> {
        LocalStore::open(state_path(&self.origin_dir))
    }

    pub fn open_client_b_store(&self) -> Result<LocalStore, fs2_daemon::LocalStoreError> {
        LocalStore::open(state_path(&self.mirror_dir))
    }

    pub fn simulate_file_creation_on_a(
        &self,
        path: &str,
        plaintext: &[u8],
    ) -> Result<SimulatedFile, Box<dyn std::error::Error>> {
        let plan = BlobUploadPlan::from_plaintext(plaintext, &self.content_key)?;
        self.client_a.upload_blob(self.workspace_id, &plan)?;
        let operation = file_create_operation(
            self.workspace_id,
            self.device_a_id,
            self.root_node_id,
            path,
            Cursor::new(0)?,
            &plan,
        )?;
        self.client_a
            .submit_operation(self.workspace_id, &operation)?;
        Ok(SimulatedFile {
            path: path.to_owned(),
            operation,
            plan,
            plaintext: plaintext.to_vec(),
        })
    }

    pub fn sync_b(
        &self,
        store: &mut LocalStore,
    ) -> Result<fs2_sync::InboundSyncReport, fs2_sync::ApiClientError> {
        InboundSync::new(&self.client_b, self.workspace_id, Duration::ZERO).sync_startup(store)
    }

    pub fn hydrate_b(
        &self,
        store: &mut LocalStore,
        file: &SimulatedFile,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let node = store
            .get_node_by_path(self.workspace_id, &file.path)?
            .ok_or_else(|| format!("metadata for {} is missing before hydration", file.path))?;
        let revision_id = node
            .current_rev
            .ok_or_else(|| format!("metadata for {} has no current revision", file.path))?;
        let revision = store
            .get_revision(revision_id)?
            .ok_or_else(|| format!("revision {revision_id} is missing before hydration"))?;
        let RevisionContent::File {
            blob_id,
            encryption_header,
            ..
        } = revision.content
        else {
            return Err(format!("metadata for {} is not a file revision", file.path).into());
        };
        let encryption_header = encryption_header
            .ok_or_else(|| format!("metadata for {} has no encryption header", file.path))?;
        let header = serde_json::from_str(&encryption_header)?;
        let downloaded = self.client_b.download_blob(&blob_id)?;
        let plaintext = decrypt_blob(
            &EncryptedBlob {
                blob_id,
                header,
                ciphertext: downloaded.bytes,
            },
            &self.content_key,
        )?;
        store.set_hydration_state(
            node.node_id,
            fs2_daemon::HydrationState::Hydrated,
            Some(&format!("hydrated/{}", file.path)),
            false,
        )?;
        Ok(plaintext)
    }
}

fn dev_login(
    client: &ApiClient,
    device_name: &str,
) -> Result<TestLoginResponse, fs2_sync::ApiClientError> {
    client.post_json(
        &["v1", "auth", "dev-login"],
        &serde_json::json!({
            "device_name": device_name,
            "platform": {"os": "linux", "arch": "x86_64"},
            "public_key": format!("{device_name}-public-key")
        }),
    )
}

fn state_path(dir: &TempDir) -> PathBuf {
    dir.path().join("metadata.sqlite")
}

fn file_create_operation(
    workspace_id: WorkspaceId,
    device_id: fs2_core::DeviceId,
    root_node_id: NodeId,
    name: &str,
    base_cursor: Cursor,
    plan: &BlobUploadPlan,
) -> Result<Operation, Box<dyn std::error::Error>> {
    let node_id = NodeId::new_v4();
    let revision_id = fs2_core::RevisionId::new_v4();
    let encryption_header = serde_json::to_string(&plan.encryption_header)?;
    Ok(Operation {
        op_id: fs2_core::OpId::new_v4(),
        workspace_id,
        device_id,
        base_cursor,
        kind: OperationKind::CreateNode {
            node_id,
            parent_id: root_node_id,
            name: name.to_owned(),
            kind: NodeKind::File,
            initial_revision: Some(fs2_core::NodeRevision {
                revision_id,
                node_id,
                workspace_id,
                device_id,
                base_revision_id: None,
                content: RevisionContent::File {
                    blob_id: plan.blob_id.clone(),
                    chunk_ids: Vec::new(),
                    content_hash: "plaintext-sha256:testkit".to_owned(),
                    encryption_header: Some(encryption_header),
                },
                posix_mode: 0o100_644,
                mtime: chrono::Utc::now(),
                size: plan.plaintext_size,
                executable: false,
                created_at: chrono::Utc::now(),
            }),
        },
        created_at: chrono::Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_client_metadata_and_blob_sync_works_before_fuse(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let harness = TwoClientHarness::start()?;
        let simulated = harness.simulate_file_creation_on_a("hello.txt", b"hello from A")?;
        let mut client_b = harness.open_client_b_store()?;

        let report = harness.sync_b(&mut client_b)?;
        assert_eq!(report.applied, 1);
        assert!(client_b
            .get_node_by_path(harness.workspace_id, "hello.txt")?
            .is_some());

        let hydrated = harness.hydrate_b(&mut client_b, &simulated)?;
        assert_eq!(hydrated, b"hello from A");
        let node = client_b
            .get_node_by_path(harness.workspace_id, "hello.txt")?
            .map(|node| node.node_id);
        assert_eq!(
            node.and_then(|node_id| client_b.node_state(node_id).transpose())
                .transpose()?
                .map(|state| state.hydration_state),
            Some(fs2_daemon::HydrationState::Hydrated)
        );
        Ok(())
    }
}
