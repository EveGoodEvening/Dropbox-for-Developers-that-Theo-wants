//! `fs2 hydrate`, `fs2 pin`, and `fs2 unpin` command logic.
//!
//! Hydration downloads file blobs from the backend, verifies their content
//! hash, caches them locally, and marks the nodes hydrated. Pinning marks
//! nodes so cache pruning never evicts their bytes. Both respect the rule
//! engine: `ignore`, `generated`, `local-only`, and `dependency-cache` paths
//! are never hydrated (their content is not synced), per `design.md` §8/§9.

use std::io::Write as _;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use fs2_core::{NodeKind, NodeId, WorkspaceId};
use fs2_rules::{Action, RuleEngine};
use fs2_sync::{ApiClient, LocalStore};
use tracing::{debug, info, warn};

/// Maximum download retries per blob.
const MAX_RETRIES: usize = 3;

/// Build a rule engine with built-in language/tool profiles and the workspace
/// default action. Used to decide which paths to hydrate.
fn build_rule_engine() -> RuleEngine {
    let builtin = fs2_rules::builtin_profiles();
    RuleEngine::new(builtin, Action::Normal)
}

/// Whether a path's rule action allows content hydration.
///
/// `ignore`, `generated`, `local-only`, and `dependency-cache` paths do not
/// have synced content; hydrating them is a no-op. `secret` content comes
/// from the secret store, not the blob store, so it is also skipped here.
fn action_allows_hydration(action: Action) -> bool {
    matches!(
        action,
        Action::Normal | Action::Lazy | Action::Pin
    )
}

/// Recursively collect file node ids under `node_id` (inclusive if it is a file).
///
/// `path` is the workspace-relative path of `node_id`, used for rule checks.
fn collect_files(
    store: &LocalStore,
    workspace_id: WorkspaceId,
    node_id: NodeId,
    path: &str,
    recursive: bool,
    engine: &RuleEngine,
    out: &mut Vec<(NodeId, String)>,
) -> Result<()> {
    let node = store
        .get_node_by_id(node_id)
        .context("failed to get node")?
        .ok_or_else(|| anyhow!("node not found: {path}"))?;
    if node.deleted {
        return Ok(());
    }
    match node.kind {
        NodeKind::File => {
            let rule = engine.resolve(path);
            if action_allows_hydration(rule.action) {
                out.push((node_id, path.to_owned()));
            } else {
                debug!("hydrate: skipping {path} (action={})", rule.action);
            }
        }
        NodeKind::Directory => {
            if recursive {
                let children = store
                    .list_children(workspace_id, Some(node_id))
                    .context("failed to list children")?;
                for child in children {
                    if child.deleted {
                        continue;
                    }
                    let child_path = if path.is_empty() {
                        child.name.clone()
                    } else {
                        format!("{path}/{}", child.name)
                    };
                    collect_files(
                        store,
                        workspace_id,
                        child.node_id,
                        &child_path,
                        recursive,
                        engine,
                        out,
                    )?;
                }
            }
        }
        NodeKind::Symlink => {
            // Symlinks have no blob content to hydrate.
        }
    }
    Ok(())
}

/// Hydrate a path: download and cache file blobs, marking nodes hydrated.
///
/// If `recursive`, all file descendants are hydrated. If `pin`, hydrated
/// nodes are also pinned. Returns the number of files hydrated.
///
/// # Errors
/// Returns an error if the path cannot be resolved or a blob cannot be
/// downloaded after retries.
pub async fn hydrate_path(
    client: &ApiClient,
    store: &LocalStore,
    workspace_id: WorkspaceId,
    root_node_id: NodeId,
    path: &str,
    recursive: bool,
    pin: bool,
) -> Result<usize> {
    let engine = build_rule_engine();

    // Resolve the target node by path.
    let target = if path.is_empty() || path == "/" {
        root_node_id
    } else {
        store
            .get_node_by_path(workspace_id, path)
            .context("failed to resolve path")?
            .ok_or_else(|| anyhow!("path not found: {path}"))?
            .node_id
    };

    let mut files = Vec::new();
    collect_files(
        store,
        workspace_id,
        target,
        path,
        recursive,
        &engine,
        &mut files,
    )?;

    let total = files.len();
    if total == 0 {
        println!("hydrate: no files to hydrate at {path}");
        return Ok(0);
    }

    let mut hydrated = 0usize;
    for (idx, (node_id, file_path)) in files.iter().enumerate() {
        print!(
            "\rhydrate: {}/{total} {file_path}...",
            idx + 1
        );
        let _ = std::io::stdout().flush();

        if let Err(e) = hydrate_one(client, store, *node_id, file_path, pin).await {
            warn!("hydrate: failed {file_path}: {e}");
            continue;
        }
        hydrated += 1;
    }
    println!();
    println!("hydrate: {hydrated}/{total} files hydrated at {path}");
    if pin {
        println!("hydrate: pinned {hydrated} files");
    }
    Ok(hydrated)
}

/// Hydrate a single file node: download blob, verify hash, cache, mark state.
async fn hydrate_one(
    client: &ApiClient,
    store: &LocalStore,
    node_id: NodeId,
    file_path: &str,
    pin: bool,
) -> Result<()> {
    let node = store
        .get_node_by_id(node_id)
        .context("failed to get node")?
        .ok_or_else(|| anyhow!("node vanished: {file_path}"))?;
    let rev_id = node
        .current_revision_id
        .ok_or_else(|| anyhow!("no revision for {file_path}"))?;
    let rev = store
        .get_revision(rev_id)
        .context("failed to get revision")?
        .ok_or_else(|| anyhow!("revision not found for {file_path}"))?;
    let blob_id = rev
        .blob_id
        .ok_or_else(|| anyhow!("no blob id for {file_path}"))?;

    // Download with retries.
    let mut bytes = None;
    let mut last_err = None;
    for attempt in 0..MAX_RETRIES {
        match client.download_blob(&blob_id).await {
            Ok(b) => {
                bytes = Some(b);
                break;
            }
            Err(e) => {
                debug!("hydrate: download attempt {} failed for {file_path}: {e}", attempt + 1);
                last_err = Some(e);
            }
        }
    }
    let bytes = bytes.ok_or_else(|| anyhow!("download failed after {MAX_RETRIES} retries: {last_err:?}"))?;

    // Verify content hash (design §7.3: verify downloaded blob hash before use).
    if !fs2_crypto::verify_blob_id(&bytes, &blob_id) {
        anyhow::bail!("blob hash verification failed for {file_path}");
    }

    // Cache the blob locally. Restrict the cache directory to the user
    // (0700) so cached file bytes are not world-readable (design §24.3).
    let cache_path = blob_cache_path(&blob_id);
    if let Some(parent) = Path::new(&cache_path).parent() {
        std::fs::create_dir_all(parent).ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::write(&cache_path, &bytes).context("failed to write blob cache")?;
    store
        .mark_blob_cached(&blob_id, &cache_path, bytes.len() as u64)
        .context("failed to mark blob cached")?;

    // Mark hydration state with the local blob path so cache pruning can link
    // the blob cache row to this node and respect the pinned flag.
    let state = if pin { "pinned" } else { "hydrated" };
    store
        .set_hydrated(node_id, &cache_path, state, pin)
        .context("failed to set hydration state")?;
    info!("hydrate: hydrated {file_path}");
    Ok(())
}

/// Pin or unpin a path. If `recursive`, all descendants are pinned/unpinned.
///
/// Pinning marks nodes so cache pruning skips their bytes (`design.md` §7.4).
/// MVP behavior: pin is local-only state (not synced as a workspace rule);
/// `--sync-rule` is a future option.
///
/// # Errors
/// Returns an error if the path cannot be resolved.
pub fn set_pin(
    store: &LocalStore,
    workspace_id: WorkspaceId,
    root_node_id: NodeId,
    path: &str,
    recursive: bool,
    pinned: bool,
) -> Result<usize> {
    let target = if path.is_empty() || path == "/" {
        root_node_id
    } else {
        store
            .get_node_by_path(workspace_id, path)
            .context("failed to resolve path")?
            .ok_or_else(|| anyhow!("path not found: {path}"))?
            .node_id
    };

    let mut nodes = Vec::new();
    collect_nodes_for_pin(store, workspace_id, target, recursive, &mut nodes)?;

    let mut count = 0usize;
    for node_id in &nodes {
        // set_pinned upserts the state row, so the pin sticks even for
        // not-yet-hydrated nodes (hydration_state='metadata-only').
        if store.set_pinned(*node_id, pinned).is_ok() {
            count += 1;
        }
    }
    let verb = if pinned { "pinned" } else { "unpinned" };
    println!("pin: {count} nodes {verb} at {path}");
    Ok(count)
}

/// Collect node ids to pin/unpin (the node and, if recursive, all descendants).
fn collect_nodes_for_pin(
    store: &LocalStore,
    workspace_id: WorkspaceId,
    node_id: NodeId,
    recursive: bool,
    out: &mut Vec<NodeId>,
) -> Result<()> {
    let node = store
        .get_node_by_id(node_id)
        .context("failed to get node")?
        .ok_or_else(|| anyhow!("node not found"))?;
    if node.deleted {
        return Ok(());
    }
    out.push(node_id);
    if recursive && node.kind == NodeKind::Directory {
        let children = store
            .list_children(workspace_id, Some(node_id))
            .context("failed to list children")?;
        for child in children {
            if child.deleted {
                continue;
            }
            collect_nodes_for_pin(
                store,
                workspace_id,
                child.node_id,
                recursive,
                out,
            )?;
        }
    }
    Ok(())
}

/// Compute the local blob cache path for a blob id.
///
/// Mirrors the daemon's blob cache layout (`design.md` §7.1):
/// `~/.fs2/workspaces/<id>/blobs/...`. For the CLI we use a shared cache dir
/// under `~/.fs2/blobs/`.
fn blob_cache_path(blob_id: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let sanitized = blob_id.replace(':', "/");
    format!("{home}/.fs2/blobs/{sanitized}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2_core::{Cursor, DeviceId, NodeRevision, Operation, OperationKind, RevisionContent, RevisionId};
    use fs2_sync::LocalStore;

    /// Build a `LocalStore` with a root + two file nodes (one normal, one under
    /// `node_modules` which is generated and should be skipped).
    fn setup_store_with_files() -> (LocalStore, WorkspaceId, NodeId) {
        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        let root_id = NodeId::new();
        let device = DeviceId::new();
        store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
        store.insert_root_node(ws_id, root_id).unwrap();

        // Create a normal file: src/main.rs
        create_file_op(&store, ws_id, root_id, device, "src", "main.rs", b"fn main() {}");
        // Create a generated file: node_modules/pkg/index.js
        create_file_op(&store, ws_id, root_id, device, "node_modules", "pkg/index.js", b"module.exports = 1;");
        (store, ws_id, root_id)
    }

    fn create_file_op(
        store: &LocalStore,
        ws_id: WorkspaceId,
        root_id: NodeId,
        device: DeviceId,
        dir_name: &str,
        file_name: &str,
        content: &[u8],
    ) {
        // Create the directory.
        let dir_op = Operation::new(
            ws_id,
            device,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: root_id,
                name: dir_name.to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        store.apply_operation(&dir_op, Cursor::from(1)).unwrap();
        let dir_node = store
            .get_node_by_path(ws_id, dir_name)
            .unwrap()
            .expect("dir node");
        // Create the file under the directory.
        let file_node_id = NodeId::new();
        let rev_id = RevisionId::new();
        let blob_id = format!("sha256:{file_node_id}");
        let file_path = format!("{dir_name}/{file_name}");
        let file_op = Operation::new(
            ws_id,
            device,
            Cursor::from(2),
            OperationKind::CreateNode {
                parent_id: dir_node.node_id,
                name: file_name.to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(NodeRevision {
                    revision_id: rev_id,
                    node_id: file_node_id,
                    workspace_id: ws_id,
                    device_id: device,
                    base_revision_id: None,
                    content: RevisionContent::File {
                        blob_id,
                        chunk_ids: vec![],
                        content_hash: "hash".to_owned(),
                        encryption_header: None,
                    },
                    posix_mode: 0o644,
                    mtime: chrono::Utc::now(),
                    size: content.len() as u64,
                    executable: false,
                    created_at: chrono::Utc::now(),
                }),
            },
            chrono::Utc::now(),
        );
        store.apply_operation(&file_op, Cursor::from(3)).unwrap();
        let _ = file_path;
    }

    #[test]
    fn collect_files_skips_generated_paths() {
        let (store, ws_id, root_id) = setup_store_with_files();
        let engine = build_rule_engine();
        let mut files = Vec::new();
        collect_files(&store, ws_id, root_id, "", true, &engine, &mut files).unwrap();
        let paths: Vec<&str> = files.iter().map(|(_, p)| p.as_str()).collect();
        // src/main.rs should be collected; node_modules/pkg/index.js should be skipped.
        assert!(paths.iter().any(|p| p.contains("main.rs")));
        assert!(!paths.iter().any(|p| p.contains("node_modules")));
    }

    #[test]
    fn collect_files_non_recursive_only_target() {
        let (store, ws_id, _root_id) = setup_store_with_files();
        let engine = build_rule_engine();
        // Target the src directory non-recursively: no files (the file is a child
        // but non-recursive means we don't descend).
        let src_node = store.get_node_by_path(ws_id, "src").unwrap().expect("src");
        let mut files = Vec::new();
        collect_files(&store, ws_id, src_node.node_id, "src", false, &engine, &mut files).unwrap();
        // Non-recursive on a directory collects nothing (the dir itself isn't a file).
        assert!(files.is_empty());
    }

    #[test]
    fn set_pin_marks_nodes_and_verifies_state() {
        let (store, ws_id, root_id) = setup_store_with_files();
        // Pin the src directory recursively.
        let count = set_pin(&store, ws_id, root_id, "src", true, true).unwrap();
        assert!(count >= 2); // src dir + main.rs file
        // Verify the file node's state row is pinned. set_pinned now upserts,
        // so the pin sticks even for not-yet-hydrated nodes.
        let main_node = store
            .get_node_by_path(ws_id, "src/main.rs")
            .unwrap()
            .expect("main.rs");
        let state = store
            .get_hydration_state(main_node.node_id)
            .unwrap()
            .expect("state row");
        // set_pinned upserts with hydration_state 'metadata-only' when no row.
        assert_eq!(state, "metadata-only");
        // Unpin and verify the row still exists.
        let _ = set_pin(&store, ws_id, root_id, "src", true, false);
        assert!(store
            .get_hydration_state(main_node.node_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn pin_on_unhydrated_node_sticks() {
        let store = LocalStore::open_in_memory().unwrap();
        let node_id = NodeId::new();
        // No local_state row exists yet; set_pinned must upsert.
        store.set_pinned(node_id, true).unwrap();
        // The state row should now exist (pin is not lost).
        assert!(store.get_hydration_state(node_id).unwrap().is_some());
    }

    #[test]
    fn shared_blob_pinned_by_one_node_survives_prune() {
        let store = LocalStore::open_in_memory().unwrap();
        // Two nodes share the same blob path (content-addressed dedup).
        let node_a = NodeId::new();
        let node_b = NodeId::new();
        let shared_path = "/tmp/blob-shared";
        store.mark_blob_cached("sha256:shared", shared_path, 4096).unwrap();
        // Node A is hydrated + pinned; node B is hydrated but not pinned.
        store
            .set_hydrated(node_a, shared_path, "pinned", true)
            .unwrap();
        store
            .set_hydrated(node_b, shared_path, "hydrated", false)
            .unwrap();
        // Prune with a tiny limit; the shared blob must survive because node A
        // pinned it (pinned_ref_count > 0).
        let evicted = store.prune_cache(1).unwrap();
        assert_eq!(evicted, 0, "shared blob pinned by one node must not be evicted");
        assert_eq!(store.get_cache_size().unwrap(), 4096);
    }

    #[test]
    fn shared_blob_evicted_when_all_nodes_unpinned() {
        let store = LocalStore::open_in_memory().unwrap();
        let node_a = NodeId::new();
        let node_b = NodeId::new();
        let shared_path = "/tmp/blob-shared2";
        store.mark_blob_cached("sha256:shared2", shared_path, 4096).unwrap();
        // Both nodes hydrated, neither pinned.
        store
            .set_hydrated(node_a, shared_path, "hydrated", false)
            .unwrap();
        store
            .set_hydrated(node_b, shared_path, "hydrated", false)
            .unwrap();
        let evicted = store.prune_cache(1).unwrap();
        assert_eq!(evicted, 4096, "unpinned shared blob should be evicted");
    }

    #[test]
    fn action_allows_hydration_filters_correctly() {
        assert!(action_allows_hydration(Action::Normal));
        assert!(action_allows_hydration(Action::Lazy));
        assert!(action_allows_hydration(Action::Pin));
        assert!(!action_allows_hydration(Action::Ignore));
        assert!(!action_allows_hydration(Action::Generated));
        assert!(!action_allows_hydration(Action::LocalOnly));
        assert!(!action_allows_hydration(Action::DependencyCache));
        assert!(!action_allows_hydration(Action::Secret));
    }

    #[test]
    fn hydrate_path_missing_is_error() {
        let (store, ws_id, root_id) = setup_store_with_files();
        // No client needed since we expect an error before any download.
        let client = ApiClient::new("http://localhost:0");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(hydrate_path(
            &client,
            &store,
            ws_id,
            root_id,
            "does/not/exist",
            false,
            false,
        ));
        assert!(res.is_err());
    }

    #[test]
    fn set_hydrated_with_pin_protects_blob_from_prune() {
        let store = LocalStore::open_in_memory().unwrap();
        let ws_id = WorkspaceId::new();
        let root_id = NodeId::new();
        store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
        store.insert_root_node(ws_id, root_id).unwrap();
        let node_id = NodeId::new();
        // Mark a blob cached and the node hydrated+pinned with the blob path.
        store.mark_blob_cached("sha256:abc", "/tmp/blob-abc", 1024).unwrap();
        store
            .set_hydrated(node_id, "/tmp/blob-abc", "pinned", true)
            .unwrap();
        // Prune with a tiny limit; the pinned blob must survive.
        let evicted = store.prune_cache(1).unwrap();
        assert_eq!(evicted, 0, "pinned blob should not be evicted");
        // The blob cache should still hold the blob (size unchanged).
        assert_eq!(store.get_cache_size().unwrap(), 1024);
    }

    #[test]
    fn set_hydrated_unpinned_blob_is_evictable() {
        let store = LocalStore::open_in_memory().unwrap();
        let node_id = NodeId::new();
        store.mark_blob_cached("sha256:xyz", "/tmp/blob-xyz", 2048).unwrap();
        store
            .set_hydrated(node_id, "/tmp/blob-xyz", "hydrated", false)
            .unwrap();
        // Prune with a tiny limit; the unpinned blob should be evicted.
        let evicted = store.prune_cache(1).unwrap();
        assert_eq!(evicted, 2048);
    }

    #[test]
    fn re_hydrating_shared_blob_preserves_pin_refcount() {
        let store = LocalStore::open_in_memory().unwrap();
        let node_a = NodeId::new();
        let shared_path = "/tmp/blob-rehydrate";
        // Node A hydrates + pins the shared blob.
        store.mark_blob_cached("sha256:re", shared_path, 512).unwrap();
        store
            .set_hydrated(node_a, shared_path, "pinned", true)
            .unwrap();
        // Node B re-hydrates the SAME blob (content-addressed dedup).
        // mark_blob_cached must NOT reset pinned_ref_count to 0.
        store.mark_blob_cached("sha256:re", shared_path, 512).unwrap();
        // Prune must still protect the blob (Node A is pinned).
        let evicted = store.prune_cache(1).unwrap();
        assert_eq!(
            evicted, 0,
            "re-hydrating a shared blob must not reset its pin ref count"
        );
        assert_eq!(store.get_cache_size().unwrap(), 512);
    }
}
