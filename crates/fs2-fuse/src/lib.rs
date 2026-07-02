#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Read-only FUSE adapter skeleton for FS2 workspaces.

use fs2_core::{
    names_collide, BlobId, CasePolicy, DeviceId, Node, NodeId, NodeKind, NodeName, NodeRevision,
    Operation, OperationKind, RevisionContent, RuleAction, WorkspaceId, WorkspacePath,
};
use fs2_crypto::{decrypt_blob, encrypt_blob, EncryptedBlob, WorkspaceContentKey};
use fs2_daemon::{HydrationState, LocalStore};
use fs2_sync::ApiClient;
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const ROOT_INO: u64 = 1;
const TTL: Duration = Duration::from_secs(1);

pub const MANUAL_TEST_INSTRUCTIONS: &str = "Linux: create an empty directory, run the fs2-fuse empty-workspace mount helper against it, then `ls <mount>` and unmount with `fusermount3 -u <mount>` or `umount <mount>`. macOS: install macFUSE, create an empty directory, mount with the same helper, verify `ls <mount>`, then unmount with `umount <mount>`.";

/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-fuse"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub inode: u64,
    pub kind: FileType,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootDirectoryEntry {
    pub inode: u64,
    pub kind: FileType,
    pub name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InodeMap {
    node_to_inode: HashMap<NodeId, u64>,
    inode_to_node: HashMap<u64, NodeId>,
    next_inode: u64,
}

impl InodeMap {
    fn new(root_node_id: NodeId) -> Self {
        let mut node_to_inode = HashMap::new();
        node_to_inode.insert(root_node_id, ROOT_INO);
        let mut inode_to_node = HashMap::new();
        inode_to_node.insert(ROOT_INO, root_node_id);
        Self {
            node_to_inode,
            inode_to_node,
            next_inode: ROOT_INO + 1,
        }
    }

    fn inode_for(&mut self, node_id: NodeId) -> u64 {
        if let Some(inode) = self.node_to_inode.get(&node_id) {
            return *inode;
        }
        let inode = self.next_inode;
        self.next_inode = self.next_inode.saturating_add(1);
        self.node_to_inode.insert(node_id, inode);
        self.inode_to_node.insert(inode, node_id);
        inode
    }

    fn node_for(&self, inode: u64) -> Option<NodeId> {
        self.inode_to_node.get(&inode).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyWorkspaceFs {
    root_attr: FileAttr,
}

impl EmptyWorkspaceFs {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            root_attr: root_attr(SystemTime::UNIX_EPOCH),
        }
    }

    #[must_use]
    pub const fn root_inode(&self) -> u64 {
        ROOT_INO
    }

    #[must_use]
    pub const fn root_entries(&self) -> [RootDirectoryEntry; 2] {
        [
            RootDirectoryEntry {
                inode: ROOT_INO,
                kind: FileType::Directory,
                name: ".",
            },
            RootDirectoryEntry {
                inode: ROOT_INO,
                kind: FileType::Directory,
                name: "..",
            },
        ]
    }

    #[must_use]
    pub const fn root_attr(&self) -> FileAttr {
        self.root_attr
    }
}

impl Default for EmptyWorkspaceFs {
    fn default() -> Self {
        Self::new()
    }
}

impl Filesystem for EmptyWorkspaceFs {
    fn lookup(&mut self, _req: &Request<'_>, _parent: u64, _name: &OsStr, reply: ReplyEntry) {
        reply.error(libc::ENOENT);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        if ino == ROOT_INO {
            reply.attr(&TTL, &self.root_attr);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != ROOT_INO {
            reply.error(libc::ENOENT);
            return;
        }
        add_entries(offset, self.root_entries(), &mut reply);
        reply.ok();
    }
}

#[derive(Debug, Clone)]
pub struct HydrationConfig {
    pub client: ApiClient,
    pub content_key: WorkspaceContentKey,
    pub cache_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct WriteHandle {
    node_id: NodeId,
    path: PathBuf,
    base_revision_id: Option<fs2_core::RevisionId>,
    dirty: bool,
    must_commit: bool,
    posix_mode: u32,
    executable: bool,
}

#[derive(Debug)]
pub struct MetadataWorkspaceFs {
    store: LocalStore,
    workspace_id: WorkspaceId,
    hydration: Option<HydrationConfig>,
    device_id: DeviceId,
    inodes: InodeMap,
    write_handles: HashMap<u64, WriteHandle>,
    next_write_handle: u64,
    write_cache_dir: PathBuf,
}

impl MetadataWorkspaceFs {
    #[must_use]
    pub fn new(store: LocalStore, workspace_id: WorkspaceId, root_node_id: NodeId) -> Self {
        Self {
            store,
            workspace_id,
            hydration: None,
            device_id: DeviceId::new_v4(),
            inodes: InodeMap::new(root_node_id),
            write_handles: HashMap::new(),
            next_write_handle: 1,
            write_cache_dir: std::env::temp_dir().join("fs2-fuse-writes"),
        }
    }

    #[must_use]
    pub fn with_hydration(mut self, hydration: HydrationConfig) -> Self {
        self.hydration = Some(hydration);
        self
    }

    #[must_use]
    pub const fn with_device_id(mut self, device_id: DeviceId) -> Self {
        self.device_id = device_id;
        self
    }

    #[must_use]
    pub fn with_write_cache_dir(mut self, write_cache_dir: PathBuf) -> Self {
        self.write_cache_dir = write_cache_dir;
        self
    }

    pub fn resolve_path(&self, path: &str) -> Result<Option<Node>, fs2_daemon::LocalStoreError> {
        self.store.get_node_by_path(self.workspace_id, path)
    }

    pub fn inode_for_node(&mut self, node_id: NodeId) -> u64 {
        self.inodes.inode_for(node_id)
    }

    fn node_workspace_path(&self, node_id: NodeId) -> std::io::Result<String> {
        let mut components = Vec::new();
        let mut current = Some(node_id);
        while let Some(node_id) = current {
            let node = self
                .store
                .get_node_by_id(node_id)
                .map_err(io_other)?
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
            current = node.parent_id;
            if !node.name.is_empty() {
                components.push(node.name);
            }
        }
        components.reverse();
        Ok(components.join("/"))
    }

    fn child_workspace_path(&self, parent_id: NodeId, name: &str) -> std::io::Result<String> {
        let parent_path = self.node_workspace_path(parent_id)?;
        if parent_path.is_empty() {
            Ok(name.to_owned())
        } else {
            Ok(format!("{parent_path}/{name}"))
        }
    }

    fn path_sync_suppressed(&self, path: &str, kind: NodeKind) -> std::io::Result<bool> {
        if let Some(rule) = self
            .store
            .get_effective_rule_for_kind(self.workspace_id, path, kind)
            .map_err(io_other)?
        {
            return Ok(rule_action_suppresses_upload(rule.action));
        }
        let engine = fs2_rules::RuleEngine::new(fs2_rules::Config::default(), Vec::new())
            .map_err(io_other)?;
        if path.rsplit('/').next().is_some_and(is_editor_temp_name) {
            return default_ancestor_suppresses_upload(path, &engine);
        }
        default_path_suppresses_upload(path, kind, &engine)
    }

    fn node_sync_suppressed(&self, node: &Node) -> std::io::Result<bool> {
        self.path_sync_suppressed(&self.node_workspace_path(node.node_id)?, node.kind)
    }

    fn parent_sync_suppressed(&self, parent_id: NodeId) -> std::io::Result<bool> {
        let parent = self
            .store
            .get_node_by_id(parent_id)
            .map_err(io_other)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "parent missing"))?;
        if parent.parent_id.is_none() {
            return Ok(false);
        }
        self.node_sync_suppressed(&parent)
    }

    fn subtree_contains_git_component(&self, node_id: NodeId) -> std::io::Result<bool> {
        if path_has_git_component(&self.node_workspace_path(node_id)?) {
            return Ok(true);
        }
        for child in self.store.list_children(node_id).map_err(io_other)? {
            if self.subtree_contains_git_component(child.node_id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn move_suppression_state(
        &self,
        node: &Node,
        new_parent_id: NodeId,
        new_name: &str,
    ) -> std::io::Result<(bool, bool)> {
        Ok((
            self.node_sync_suppressed(node)?,
            self.path_sync_suppressed(
                &self.child_workspace_path(new_parent_id, new_name)?,
                node.kind,
            )?,
        ))
    }

    fn reject_move_below_suppressed_parent(
        &self,
        new_parent_id: NodeId,
        new_path_suppressed: bool,
    ) -> std::io::Result<()> {
        if new_path_suppressed || !self.parent_sync_suppressed(new_parent_id)? {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "moves below generated/local-only parents must keep the target suppressed",
        ))
    }

    fn reject_git_internal_path(path: &str) -> std::io::Result<()> {
        if path_has_git_component(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                ".git internals are local-only and are not synced through FUSE",
            ));
        }
        Ok(())
    }

    pub fn attr_for_node(
        &self,
        node: &Node,
        inode: u64,
    ) -> Result<FileAttr, fs2_daemon::LocalStoreError> {
        let mut attr = node_attr(node, inode);
        if let Some(revision_id) = node.current_rev {
            if let Some(revision) = self.store.get_revision(revision_id)? {
                attr.size = revision.size;
                attr.perm = u16::try_from(revision.posix_mode & 0o7777).unwrap_or(attr.perm);
                let mtime = datetime_to_system_time(revision.mtime);
                attr.mtime = mtime;
                attr.atime = mtime;
            }
        }
        Ok(attr)
    }

    pub fn directory_entries(
        &mut self,
        node_id: NodeId,
    ) -> Result<Vec<DirectoryEntry>, fs2_daemon::LocalStoreError> {
        let node = self.store.get_node_by_id(node_id)?.ok_or_else(|| {
            fs2_daemon::LocalStoreError::Invalid("directory node missing".to_owned())
        })?;
        if node.kind != NodeKind::Directory {
            return Err(fs2_daemon::LocalStoreError::Invalid(
                "node is not a directory".to_owned(),
            ));
        }
        let own_inode = self.inode_for_node(node.node_id);
        let parent_inode = node
            .parent_id
            .map_or(ROOT_INO, |parent_id| self.inode_for_node(parent_id));
        let mut entries = vec![
            DirectoryEntry {
                inode: own_inode,
                kind: FileType::Directory,
                name: ".".to_owned(),
            },
            DirectoryEntry {
                inode: parent_inode,
                kind: FileType::Directory,
                name: "..".to_owned(),
            },
        ];
        for child in self.store.list_children(node_id)? {
            entries.push(DirectoryEntry {
                inode: self.inode_for_node(child.node_id),
                kind: file_type(child.kind),
                name: child.name,
            });
        }
        Ok(entries)
    }

    pub fn create_local_node(
        &mut self,
        parent_id: NodeId,
        name: &str,
        kind: NodeKind,
    ) -> std::io::Result<(Node, FileAttr, fs2_core::OpId)> {
        let new_name = NodeName::parse(name).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        let child_path = self.child_workspace_path(parent_id, name)?;
        Self::reject_git_internal_path(&child_path)?;
        let suppress_upload = self.path_sync_suppressed(&child_path, kind)?;
        if !suppress_upload && self.parent_sync_suppressed(parent_id)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "creates below generated/local-only parents must keep the parent suppressed",
            ));
        }
        for sibling in self.store.list_children(parent_id).map_err(io_other)? {
            let sibling_name = NodeName::parse(&sibling.name).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?;
            if names_collide(&new_name, &sibling_name, CasePolicy::Portable) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "portable sibling name collision",
                ));
            }
        }
        let node_id = NodeId::new_v4();
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::CreateNode {
                node_id,
                parent_id,
                name: name.to_owned(),
                kind,
                initial_revision: None,
            },
            created_at: chrono::Utc::now(),
        };
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)?;
        if suppress_upload {
            self.store
                .remove_pending_op(operation.op_id)
                .map_err(io_other)?;
        }
        let node = self
            .store
            .get_node_by_id(node_id)
            .map_err(io_other)?
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "created node missing")
            })?;
        let inode = self.inode_for_node(node.node_id);
        let attr = self.attr_for_node(&node, inode).map_err(io_other)?;
        Ok((node, attr, operation.op_id))
    }

    fn begin_write_handle(
        &mut self,
        node_id: NodeId,
        must_commit: bool,
        new_file_mode: Option<u32>,
    ) -> std::io::Result<u64> {
        if self.hydration.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "writes require workspace content key",
            ));
        }
        let node = self
            .store
            .get_node_by_id(node_id)
            .map_err(io_other)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
        if node.kind != NodeKind::File {
            return Err(std::io::Error::from_raw_os_error(libc::EISDIR));
        }
        Self::reject_git_internal_path(&self.node_workspace_path(node_id)?)?;
        let handle = self.next_write_handle;
        self.next_write_handle = self.next_write_handle.saturating_add(1).max(1);
        create_private_dir_all(&self.write_cache_dir)?;
        let path = self.write_cache_dir.join(format!(
            "write-{}-{}-{handle}.tmp",
            self.workspace_id, node.node_id
        ));
        let mut staged = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        if !must_commit && node.current_rev.is_some() {
            let existing = self.ensure_file_bytes(&node)?;
            staged.write_all(&existing)?;
        }
        let (posix_mode, executable) = if let Some(revision_id) = node.current_rev {
            let revision = self
                .store
                .get_revision(revision_id)
                .map_err(io_other)?
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "base revision missing")
                })?;
            (revision.posix_mode, revision.executable)
        } else {
            let mode = regular_file_mode(new_file_mode.unwrap_or(0o644));
            (mode, mode & 0o111 != 0)
        };
        self.write_handles.insert(
            handle,
            WriteHandle {
                node_id,
                path,
                base_revision_id: node.current_rev,
                dirty: false,
                must_commit,
                posix_mode,
                executable,
            },
        );
        Ok(handle)
    }

    pub fn write_to_handle(
        &mut self,
        handle: u64,
        offset: i64,
        data: &[u8],
    ) -> std::io::Result<u32> {
        if offset < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "negative write offset",
            ));
        }
        let write_handle = self.write_handles.get_mut(&handle).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "write handle missing")
        })?;
        let mut staged = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&write_handle.path)?;
        staged.seek(SeekFrom::Start(u64::try_from(offset).map_err(io_other)?))?;
        staged.write_all(data)?;
        write_handle.dirty = true;
        u32::try_from(data.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "write too large"))
    }

    fn truncate_write_handle(&mut self, handle: u64, size: u64) -> std::io::Result<()> {
        let write_handle = self.write_handles.get_mut(&handle).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "write handle missing")
        })?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&write_handle.path)?;
        file.set_len(size)?;
        write_handle.dirty = true;
        write_handle.must_commit = true;
        Ok(())
    }

    fn read_from_write_handle(
        &self,
        handle: u64,
        offset: i64,
        size: u32,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let Some(write_handle) = self.write_handles.get(&handle) else {
            return Ok(None);
        };
        if offset < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "negative read offset",
            ));
        }
        let bytes = fs::read(&write_handle.path)?;
        let start = usize::try_from(offset).map_err(io_other)?;
        let requested = usize::try_from(size).map_err(io_other)?;
        if start >= bytes.len() {
            return Ok(Some(Vec::new()));
        }
        let end = start.saturating_add(requested).min(bytes.len());
        Ok(Some(bytes[start..end].to_vec()))
    }

    fn truncate_node_now(&mut self, node_id: NodeId, size: u64) -> std::io::Result<()> {
        let handle = self.begin_write_handle(node_id, true, None)?;
        self.truncate_write_handle(handle, size)?;
        self.commit_write_handle(handle)?;
        Ok(())
    }

    pub fn commit_write_handle(&mut self, handle: u64) -> std::io::Result<Option<NodeRevision>> {
        let Some(write_handle) = self.write_handles.remove(&handle) else {
            return Ok(None);
        };
        if !write_handle.must_commit && !write_handle.dirty {
            fs::remove_file(write_handle.path)?;
            return Ok(None);
        }
        let revision = self.commit_write_snapshot(&write_handle)?;
        fs::remove_file(write_handle.path)?;
        Ok(Some(revision))
    }

    fn flush_write_handle(&mut self, handle: u64) -> std::io::Result<Option<NodeRevision>> {
        let Some(write_handle) = self.write_handles.get(&handle).cloned() else {
            return Ok(None);
        };
        if !write_handle.must_commit && !write_handle.dirty {
            return Ok(None);
        }
        let revision = self.commit_write_snapshot(&write_handle)?;
        if let Some(write_handle) = self.write_handles.get_mut(&handle) {
            write_handle.dirty = false;
            write_handle.must_commit = false;
            write_handle.base_revision_id = Some(revision.revision_id);
            write_handle.posix_mode = revision.posix_mode;
            write_handle.executable = revision.executable;
        }
        Ok(Some(revision))
    }

    fn commit_write_snapshot(
        &mut self,
        write_handle: &WriteHandle,
    ) -> std::io::Result<NodeRevision> {
        let hydration = self.hydration.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "writes require workspace content key",
            )
        })?;
        let plaintext = fs::read(&write_handle.path)?;
        let encrypted = encrypt_blob(&plaintext, &hydration.content_key).map_err(io_other)?;
        let encryption_header = serde_json::to_string(&encrypted.header).map_err(io_other)?;
        self.store
            .put_pending_blob_upload(
                &encrypted.blob_id,
                self.workspace_id,
                &encrypted.ciphertext,
                Some(&encryption_header),
            )
            .map_err(io_other)?;
        let cache_path = blob_cache_path(&self.write_cache_dir, &encrypted.blob_id);
        if let Some(parent) = cache_path.parent() {
            create_private_dir_all(parent)?;
        }
        fs::write(&cache_path, &plaintext)?;
        let cache_path_string = cache_path.display().to_string();
        self.store
            .mark_blob_cached(
                &encrypted.blob_id,
                &cache_path_string,
                plaintext.len() as u64,
                true,
            )
            .map_err(io_other)?;
        let now = chrono::Utc::now();
        let revision = NodeRevision {
            revision_id: fs2_core::RevisionId::new_v4(),
            node_id: write_handle.node_id,
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_revision_id: write_handle.base_revision_id,
            content: RevisionContent::File {
                blob_id: encrypted.blob_id.clone(),
                chunk_ids: Vec::new(),
                content_hash: format!(
                    "plaintext-sha256:{}",
                    hex_lower(&Sha256::digest(&plaintext))
                ),
                encryption_header: Some(encryption_header),
            },
            posix_mode: write_handle.posix_mode,
            mtime: now,
            size: plaintext.len() as u64,
            executable: write_handle.executable,
            created_at: now,
        };
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::PutFileRevision {
                node_id: write_handle.node_id,
                base_revision_id: write_handle.base_revision_id,
                revision: revision.clone(),
            },
            created_at: now,
        };
        let node = self
            .store
            .get_node_by_id(write_handle.node_id)
            .map_err(io_other)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
        let suppress_upload = self.node_sync_suppressed(&node)?;
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)?;
        let pinned = self
            .store
            .node_state(write_handle.node_id)
            .map_err(io_other)?
            .is_some_and(|state| state.pinned);
        self.finish_write_state(
            write_handle,
            &encrypted.blob_id,
            &cache_path_string,
            operation.op_id,
            suppress_upload,
            pinned,
        )?;
        Ok(revision)
    }

    fn finish_write_state(
        &mut self,
        write_handle: &WriteHandle,
        blob_id: &BlobId,
        cache_path: &str,
        op_id: fs2_core::OpId,
        suppress_upload: bool,
        pinned: bool,
    ) -> std::io::Result<()> {
        if suppress_upload {
            self.store.remove_pending_op(op_id).map_err(io_other)?;
            self.store
                .remove_pending_blob_upload(blob_id)
                .map_err(io_other)?;
            self.store
                .set_hydration_state(
                    write_handle.node_id,
                    HydrationState::Hydrated,
                    Some(cache_path),
                    pinned,
                )
                .map_err(io_other)?;
        } else {
            self.store
                .mark_node_dirty(
                    write_handle.node_id,
                    cache_path,
                    write_handle.base_revision_id,
                    pinned,
                )
                .map_err(io_other)?;
        }
        Ok(())
    }

    pub fn read_file(
        &mut self,
        node_id: NodeId,
        offset: i64,
        size: u32,
    ) -> std::io::Result<Vec<u8>> {
        if offset < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "negative read offset",
            ));
        }
        let node = self
            .store
            .get_node_by_id(node_id)
            .map_err(io_other)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
        if node.kind != NodeKind::File {
            return Err(std::io::Error::from_raw_os_error(libc::EISDIR));
        }
        let bytes = self.ensure_file_bytes(&node)?;
        self.store
            .mark_node_accessed(node.node_id)
            .map_err(io_other)?;
        let start = usize::try_from(offset).map_err(io_other)?;
        let requested = usize::try_from(size).map_err(io_other)?;
        if start >= bytes.len() {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(requested).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    fn set_open_write_handle_mode(&mut self, node_id: NodeId, mode: u32) -> bool {
        let posix_mode = regular_file_mode(mode);
        let executable = posix_mode & 0o111 != 0;
        let mut updated = false;
        for write_handle in self.write_handles.values_mut() {
            if write_handle.node_id == node_id {
                write_handle.posix_mode = posix_mode;
                write_handle.executable = executable;
                updated = true;
            }
        }
        updated
    }

    pub fn set_node_mode(&mut self, node_id: NodeId, mode: u32) -> std::io::Result<NodeRevision> {
        let node = self
            .store
            .get_node_by_id(node_id)
            .map_err(io_other)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
        if node.kind != NodeKind::File {
            return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP));
        }
        let base_revision_id = node.current_rev.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "file has no current revision",
            )
        })?;
        let base_revision = self
            .store
            .get_revision(base_revision_id)
            .map_err(io_other)?
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "base revision missing")
            })?;
        let now = chrono::Utc::now();
        let posix_mode = regular_file_mode(mode);
        let revision = NodeRevision {
            revision_id: fs2_core::RevisionId::new_v4(),
            node_id,
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_revision_id: Some(base_revision_id),
            content: base_revision.content,
            posix_mode,
            mtime: base_revision.mtime,
            size: base_revision.size,
            executable: posix_mode & 0o111 != 0,
            created_at: now,
        };
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::PutFileRevision {
                node_id,
                base_revision_id: Some(base_revision_id),
                revision: revision.clone(),
            },
            created_at: now,
        };
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)?;
        if self.node_sync_suppressed(&node)? {
            self.store
                .remove_pending_op(operation.op_id)
                .map_err(io_other)?;
        }
        for write_handle in self.write_handles.values_mut() {
            if write_handle.node_id == node_id
                && write_handle.base_revision_id == Some(base_revision_id)
            {
                write_handle.base_revision_id = Some(revision.revision_id);
                write_handle.posix_mode = revision.posix_mode;
                write_handle.executable = revision.executable;
            }
        }
        if let Some(state) = self.store.node_state(node_id).map_err(io_other)? {
            if state.hydration_state == HydrationState::Dirty {
                if let Some(path) = state.local_blob_path {
                    self.store
                        .mark_node_dirty(node_id, &path, Some(base_revision_id), state.pinned)
                        .map_err(io_other)?;
                }
            }
        }
        Ok(revision)
    }

    fn ensure_file_bytes(&mut self, node: &Node) -> std::io::Result<Vec<u8>> {
        if let Some(state) = self.store.node_state(node.node_id).map_err(io_other)? {
            match state.hydration_state {
                HydrationState::Dirty => {
                    if let Some(path) = state.local_blob_path {
                        match fs::read(&path) {
                            Ok(bytes) => return Ok(bytes),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                HydrationState::Hydrated => {
                    if let Some(path) = state.local_blob_path {
                        if self.cached_path_matches_current_revision(node, &path)? {
                            match fs::read(&path) {
                                Ok(bytes) => return Ok(bytes),
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error),
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(bytes) = self.cached_file_bytes(node)? {
            return Ok(bytes);
        }
        self.hydrate_file(node)
    }

    fn cached_path_matches_current_revision(
        &self,
        node: &Node,
        path: &str,
    ) -> std::io::Result<bool> {
        let (blob_id, _encryption_header, expected_size) = self.file_revision_blob(node)?;
        let Some(entry) = self.store.blob_cache_entry(&blob_id).map_err(io_other)? else {
            return Ok(false);
        };
        Ok(entry.verified && entry.size == expected_size && entry.path == path)
    }

    fn cached_file_bytes(&mut self, node: &Node) -> std::io::Result<Option<Vec<u8>>> {
        let (blob_id, _encryption_header, expected_size) = self.file_revision_blob(node)?;
        let Some(entry) = self.store.blob_cache_entry(&blob_id).map_err(io_other)? else {
            return Ok(None);
        };
        if !entry.verified || entry.size != expected_size {
            return Ok(None);
        }
        let bytes = match fs::read(&entry.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if bytes.len() as u64 != expected_size {
            return Ok(None);
        }
        let pinned = self
            .store
            .node_state(node.node_id)
            .map_err(io_other)?
            .is_some_and(|state| state.pinned);
        self.store
            .set_hydration_state(
                node.node_id,
                HydrationState::Hydrated,
                Some(&entry.path),
                pinned,
            )
            .map_err(io_other)?;
        Ok(Some(bytes))
    }

    fn file_revision_blob(&self, node: &Node) -> std::io::Result<(BlobId, Option<String>, u64)> {
        let revision_id = node.current_rev.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "file has no current revision",
            )
        })?;
        let revision = self
            .store
            .get_revision(revision_id)
            .map_err(io_other)?
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "revision missing")
            })?;
        let RevisionContent::File {
            blob_id,
            encryption_header,
            ..
        } = revision.content
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "revision is not file content",
            ));
        };
        Ok((blob_id, encryption_header, revision.size))
    }

    fn hydrate_file(&mut self, node: &Node) -> std::io::Result<Vec<u8>> {
        let hydration = self.hydration.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "file is metadata-only and backend hydration is unavailable",
            )
        })?;
        let (blob_id, encryption_header, expected_size) = self.file_revision_blob(node)?;
        let header = encryption_header
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "missing encryption header")
            })
            .and_then(|header| serde_json::from_str(&header).map_err(io_other))?;
        let downloaded = hydration.client.download_blob(&blob_id).map_err(io_other)?;
        let bytes = decrypt_blob(
            &EncryptedBlob {
                blob_id: blob_id.clone(),
                header,
                ciphertext: downloaded.bytes,
            },
            &hydration.content_key,
        )
        .map_err(io_other)?;
        if bytes.len() as u64 != expected_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "hydrated file size does not match revision metadata",
            ));
        }
        create_private_dir_all(&hydration.cache_dir)?;
        let cache_path = blob_cache_path(&hydration.cache_dir, &blob_id);
        let tmp_path = cache_path.with_extension("tmp");
        fs::write(&tmp_path, &bytes)?;
        fs::rename(&tmp_path, &cache_path)?;
        let pinned = self
            .store
            .node_state(node.node_id)
            .map_err(io_other)?
            .is_some_and(|state| state.pinned);
        let cache_path_string = cache_path.display().to_string();
        self.store
            .mark_blob_cached(&blob_id, &cache_path_string, bytes.len() as u64, true)
            .map_err(io_other)?;
        self.store
            .set_hydration_state(
                node.node_id,
                HydrationState::Hydrated,
                Some(&cache_path_string),
                pinned,
            )
            .map_err(io_other)?;
        Ok(bytes)
    }

    pub fn symlink_target(&self, node_id: NodeId) -> std::io::Result<String> {
        let node = self
            .store
            .get_node_by_id(node_id)
            .map_err(io_other)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
        if node.kind != NodeKind::Symlink {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }
        let revision_id = node.current_rev.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "symlink has no current revision",
            )
        })?;
        let revision = self
            .store
            .get_revision(revision_id)
            .map_err(io_other)?
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "revision missing")
            })?;
        match revision.content {
            RevisionContent::Symlink { target } => Ok(target),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "revision is not symlink content",
            )),
        }
    }

    fn collapse_editor_temp_replace(
        &mut self,
        source: &Node,
        target: &Node,
    ) -> std::io::Result<bool> {
        if target.kind != NodeKind::File || !is_editor_temp_name(&source.name) {
            return Ok(false);
        }
        let pending = self
            .store
            .list_pending_ops(self.workspace_id)
            .map_err(io_other)?;
        let create_op_id = pending
            .iter()
            .find_map(|pending| match pending.operation.kind {
                OperationKind::CreateNode { node_id, .. } if node_id == source.node_id => {
                    Some(pending.operation.op_id)
                }
                _ => None,
            });
        let Some(create_op_id) = create_op_id else {
            return Ok(false);
        };
        let source_writes = pending
            .iter()
            .filter_map(|pending| match &pending.operation.kind {
                OperationKind::PutFileRevision {
                    node_id, revision, ..
                } if *node_id == source.node_id => {
                    Some((pending.operation.op_id, revision.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some((_write_op_id, source_revision)) = source_writes.last().cloned() else {
            return Ok(false);
        };
        let now = chrono::Utc::now();
        let final_blob_id = file_blob_id(&source_revision).cloned();
        let mut revision = source_revision;
        revision.revision_id = fs2_core::RevisionId::new_v4();
        revision.node_id = target.node_id;
        revision.base_revision_id = target.current_rev;
        revision.created_at = now;
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::PutFileRevision {
                node_id: target.node_id,
                base_revision_id: target.current_rev,
                revision,
            },
            created_at: now,
        };
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)?;
        for (pending_write_op_id, pending_revision) in source_writes {
            self.store
                .remove_pending_op(pending_write_op_id)
                .map_err(io_other)?;
            if let Some(blob_id) = file_blob_id(&pending_revision) {
                if Some(blob_id) != final_blob_id.as_ref() {
                    self.store
                        .remove_pending_blob_upload(blob_id)
                        .map_err(io_other)?;
                }
            }
        }
        self.store
            .remove_optimistic_create(source.node_id, create_op_id)
            .map_err(io_other)?;
        Ok(true)
    }

    fn discard_uncommitted_editor_temp(&mut self, node: &Node) -> std::io::Result<bool> {
        if !is_editor_temp_name(&node.name) {
            return Ok(false);
        }
        let pending = self
            .store
            .list_pending_ops(self.workspace_id)
            .map_err(io_other)?;
        let create_op_id = pending
            .iter()
            .find_map(|pending| match pending.operation.kind {
                OperationKind::CreateNode { node_id, .. } if node_id == node.node_id => {
                    Some(pending.operation.op_id)
                }
                _ => None,
            });
        let Some(create_op_id) = create_op_id else {
            return Ok(false);
        };
        for pending in pending {
            if let OperationKind::PutFileRevision {
                node_id, revision, ..
            } = &pending.operation.kind
            {
                if *node_id == node.node_id {
                    self.store
                        .remove_pending_op(pending.operation.op_id)
                        .map_err(io_other)?;
                    if let Some(blob_id) = file_blob_id(revision) {
                        self.store
                            .remove_pending_blob_upload(blob_id)
                            .map_err(io_other)?;
                    }
                }
            }
        }
        self.store
            .remove_optimistic_create(node.node_id, create_op_id)
            .map_err(io_other)?;
        Ok(true)
    }

    fn collapse_editor_temp_create(
        &mut self,
        source: &Node,
        new_parent_id: NodeId,
        new_name: &str,
    ) -> std::io::Result<bool> {
        if source.kind != NodeKind::File || !is_editor_temp_name(&source.name) {
            return Ok(false);
        }
        let pending = self
            .store
            .list_pending_ops(self.workspace_id)
            .map_err(io_other)?;
        let create_op_id = pending
            .iter()
            .find_map(|pending| match pending.operation.kind {
                OperationKind::CreateNode { node_id, .. } if node_id == source.node_id => {
                    Some(pending.operation.op_id)
                }
                _ => None,
            });
        let Some(create_op_id) = create_op_id else {
            return Ok(false);
        };
        let source_writes = pending
            .iter()
            .filter_map(|pending| match &pending.operation.kind {
                OperationKind::PutFileRevision {
                    node_id, revision, ..
                } if *node_id == source.node_id => {
                    Some((pending.operation.op_id, revision.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some((_, source_revision)) = source_writes.last().cloned() else {
            return Ok(false);
        };
        let final_blob_id = file_blob_id(&source_revision).cloned();
        let mut revision = source_revision;
        revision.base_revision_id = None;
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::CreateNode {
                node_id: source.node_id,
                parent_id: new_parent_id,
                name: new_name.to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(revision),
            },
            created_at: chrono::Utc::now(),
        };
        for (pending_write_op_id, pending_revision) in source_writes {
            self.store
                .remove_pending_op(pending_write_op_id)
                .map_err(io_other)?;
            if let Some(blob_id) = file_blob_id(&pending_revision) {
                if Some(blob_id) != final_blob_id.as_ref() {
                    self.store
                        .remove_pending_blob_upload(blob_id)
                        .map_err(io_other)?;
                }
            }
        }
        self.store
            .remove_optimistic_create(source.node_id, create_op_id)
            .map_err(io_other)?;
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)?;
        Ok(true)
    }

    pub fn move_local_node(
        &mut self,
        parent_id: NodeId,
        name: &str,
        new_parent_id: NodeId,
        new_name: &str,
    ) -> std::io::Result<()> {
        let node = self
            .lookup_child(parent_id, name)
            .map_err(io_other)?
            .map(|(node, _)| node)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "source missing"))?;
        Self::reject_git_internal_path(&self.node_workspace_path(node.node_id)?)?;
        if self.subtree_contains_git_component(node.node_id)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                ".git internals are local-only and are not synced through FUSE",
            ));
        }
        Self::reject_git_internal_path(&self.child_workspace_path(new_parent_id, new_name)?)?;
        let mut ancestor = Some(new_parent_id);
        while let Some(ancestor_id) = ancestor {
            if ancestor_id == node.node_id {
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
            }
            ancestor = self
                .store
                .get_node_by_id(ancestor_id)
                .map_err(io_other)?
                .and_then(|ancestor_node| ancestor_node.parent_id);
        }
        let new_name_parsed = NodeName::parse(new_name).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
        })?;
        let mut replacement = None;
        for sibling in self.store.list_children(new_parent_id).map_err(io_other)? {
            if sibling.node_id == node.node_id {
                continue;
            }
            let sibling_name = NodeName::parse(&sibling.name).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?;
            if names_collide(&new_name_parsed, &sibling_name, CasePolicy::Portable) {
                if sibling.name == new_name {
                    replacement = Some(sibling);
                    continue;
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "portable sibling name collision",
                ));
            }
        }
        let (old_path_suppressed, new_path_suppressed) =
            self.move_suppression_state(&node, new_parent_id, new_name)?;
        reject_cross_boundary_move(old_path_suppressed, new_path_suppressed)?;
        self.reject_move_below_suppressed_parent(new_parent_id, new_path_suppressed)?;
        if let Some(replacement) = replacement.as_ref() {
            if self.collapse_editor_temp_replace(&node, replacement)? {
                return Ok(());
            }
        } else if self.collapse_editor_temp_create(&node, new_parent_id, new_name)? {
            return Ok(());
        }
        let mut move_created_at = chrono::Utc::now();
        if let Some(replacement) = replacement {
            match (
                node.kind == NodeKind::Directory,
                replacement.kind == NodeKind::Directory,
            ) {
                (true, true) => {
                    if !self
                        .store
                        .list_children(replacement.node_id)
                        .map_err(io_other)?
                        .is_empty()
                    {
                        return Err(std::io::Error::from_raw_os_error(libc::ENOTEMPTY));
                    }
                }
                (true, false) => return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR)),
                (false, true) => return Err(std::io::Error::from_raw_os_error(libc::EISDIR)),
                (false, false) => {}
            }
            let delete_created_at = move_created_at;
            self.delete_local_node_at(new_parent_id, new_name, false, delete_created_at)?;
            move_created_at = delete_created_at + chrono::Duration::microseconds(1);
        }
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::MoveNode {
                node_id: node.node_id,
                old_parent_id: parent_id,
                old_name: name.to_owned(),
                new_parent_id,
                new_name: new_name.to_owned(),
            },
            created_at: move_created_at,
        };
        self.apply_local_move(&operation, old_path_suppressed && new_path_suppressed)
    }

    fn apply_local_move(
        &mut self,
        operation: &Operation,
        suppress_upload: bool,
    ) -> std::io::Result<()> {
        self.store
            .apply_local_pending_op(operation)
            .map_err(io_other)?;
        if suppress_upload {
            self.store
                .remove_pending_op(operation.op_id)
                .map_err(io_other)?;
        }
        Ok(())
    }

    pub fn delete_local_node(
        &mut self,
        parent_id: NodeId,
        name: &str,
        recursive: bool,
    ) -> std::io::Result<()> {
        self.delete_local_node_at(parent_id, name, recursive, chrono::Utc::now())
    }

    fn delete_local_node_at(
        &mut self,
        parent_id: NodeId,
        name: &str,
        recursive: bool,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> std::io::Result<()> {
        let node = self
            .lookup_child(parent_id, name)
            .map_err(io_other)?
            .map(|(node, _)| node)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
        if self.discard_uncommitted_editor_temp(&node)? {
            return Ok(());
        }
        if self.node_sync_suppressed(&node)? {
            self.store
                .remove_local_subtree(self.workspace_id, node.node_id)
                .map_err(io_other)?;
            return Ok(());
        }
        let operation = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: self.workspace_id,
            device_id: self.device_id,
            base_cursor: self
                .store
                .last_cursor(self.workspace_id)
                .map_err(io_other)?,
            kind: OperationKind::DeleteNode {
                node_id: node.node_id,
                recursive,
            },
            created_at,
        };
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)
    }

    fn node_for_inode(&self, inode: u64) -> Result<Option<Node>, fs2_daemon::LocalStoreError> {
        self.inodes
            .node_for(inode)
            .map(|node_id| self.store.get_node_by_id(node_id))
            .transpose()
            .map(Option::flatten)
    }
}

impl Filesystem for MetadataWorkspaceFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_node_id) = self.inodes.node_for(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.lookup_child(parent_node_id, name) {
            Ok(Some((_node, attr))) => reply.entry(&TTL, &attr, 0),
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_id) = self.inodes.node_for(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.delete_local_node(parent_id, name, false) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_id) = self.inodes.node_for(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.lookup_child(parent_id, name) {
            Ok(Some((node, _))) if node.kind == NodeKind::Directory => {
                match self.store.list_children(node.node_id) {
                    Ok(children) if children.is_empty() => {}
                    Ok(_) => {
                        reply.error(libc::ENOTEMPTY);
                        return;
                    }
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                }
            }
            Ok(Some(_)) => {
                reply.error(libc::ENOTDIR);
                return;
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        }
        match self.delete_local_node(parent_id, name, false) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        target_name: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags != 0 {
            reply.error(libc::ENOTSUP);
            return;
        }
        let (Some(parent_id), Some(new_parent_id)) = (
            self.inodes.node_for(parent),
            self.inodes.node_for(newparent),
        ) else {
            reply.error(libc::ENOENT);
            return;
        };
        let (Some(source_name), Some(target_name_str)) = (name.to_str(), target_name.to_str())
        else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.move_local_node(parent_id, source_name, new_parent_id, target_name_str) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_id) = self.inodes.node_for(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.create_local_node(parent_id, name, NodeKind::Directory) {
            Ok((_node, attr, _op_id)) => reply.entry(&TTL, &attr, 0),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_id) = self.inodes.node_for(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.create_local_node(parent_id, name, NodeKind::File) {
            Ok((node, attr, op_id)) => {
                match self.begin_write_handle(node.node_id, true, Some(mode)) {
                    Ok(handle) => reply.created(&TTL, &attr, 0, handle, 0),
                    Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                        reply.created(&TTL, &attr, 0, 0, 0);
                    }
                    Err(error) => {
                        let _ = self.store.remove_optimistic_create(node.node_id, op_id);
                        reply.error(io_error_code(&error));
                    }
                }
            }
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.node_for_inode(ino) {
            Ok(Some(node)) => match self.attr_for_node(&node, ino) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(_) => reply.error(libc::EIO),
            },
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<u64>,
        created_time: Option<SystemTime>,
        changed_time: Option<SystemTime>,
        backup_time: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if uid.is_some()
            || gid.is_some()
            || atime.is_some()
            || mtime.is_some()
            || ctime.is_some()
            || created_time.is_some()
            || changed_time.is_some()
            || backup_time.is_some()
            || flags.is_some()
        {
            reply.error(libc::ENOTSUP);
            return;
        }
        let Some(node_id) = self.inodes.node_for(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if let Some(mode) = mode {
            if size.is_some() {
                reply.error(libc::ENOTSUP);
                return;
            }
            let result = (|| {
                let node = self
                    .store
                    .get_node_by_id(node_id)
                    .map_err(io_other)?
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::NotFound, "node missing")
                    })?;
                if node.current_rev.is_none() && self.set_open_write_handle_mode(node_id, mode) {
                    let mut attr = self.attr_for_node(&node, ino).map_err(io_other)?;
                    attr.perm =
                        u16::try_from(regular_file_mode(mode) & 0o7777).unwrap_or(attr.perm);
                    return Ok(attr);
                }
                self.set_node_mode(node_id, mode)?;
                let node = self
                    .store
                    .get_node_by_id(node_id)
                    .map_err(io_other)?
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::NotFound, "node missing")
                    })?;
                self.attr_for_node(&node, ino).map_err(io_other)
            })();
            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(io_error_code(&error)),
            }
            return;
        }
        let Some(size) = size else {
            match self.node_for_inode(ino) {
                Ok(Some(node)) => match self.attr_for_node(&node, ino) {
                    Ok(attr) => reply.attr(&TTL, &attr),
                    Err(error) => reply.error(io_error_code(&io_other(error))),
                },
                Ok(None) => reply.error(libc::ENOENT),
                Err(_) => reply.error(libc::EIO),
            }
            return;
        };
        if size != 0 {
            reply.error(libc::ENOTSUP);
            return;
        }

        let result = if let Some(handle) = fh {
            self.truncate_write_handle(handle, size)
        } else {
            self.truncate_node_now(node_id, size)
        };
        match result.and_then(|()| {
            let node = self
                .store
                .get_node_by_id(node_id)
                .map_err(io_other)?
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "node missing"))?;
            let mut attr = self.attr_for_node(&node, ino).map_err(io_other)?;
            attr.size = size;
            Ok(attr)
        }) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let Some(node_id) = self.inodes.node_for(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.symlink_target(node_id) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node_id) = self.inodes.node_for(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.directory_entries(node_id) {
            Ok(entries) => {
                add_entries(offset, entries, &mut reply);
                reply.ok();
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.node_for_inode(ino) {
            Ok(Some(node)) if node.kind == NodeKind::File => {
                let access_mode = flags & libc::O_ACCMODE;
                if access_mode == libc::O_RDONLY {
                    reply.opened(0, 0);
                    return;
                }
                match self.begin_write_handle(node.node_id, flags & libc::O_TRUNC != 0, None) {
                    Ok(handle) => reply.opened(handle, 0),
                    Err(error) => reply.error(io_error_code(&error)),
                }
            }
            Ok(Some(_)) => reply.error(libc::EISDIR),
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(node_id) = self.inodes.node_for(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.read_from_write_handle(fh, offset, size) {
            Ok(Some(bytes)) => {
                reply.data(&bytes);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                reply.error(io_error_code(&error));
                return;
            }
        }
        match self.read_file(node_id, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if self.inodes.node_for(ino).is_none() {
            reply.error(libc::ENOENT);
            return;
        }
        match self.write_to_handle(fh, offset, data) {
            Ok(written) => reply.written(written),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.flush_write_handle(fh) {
            Ok(_) => reply.ok(),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.commit_write_handle(fh) {
            Ok(_) => reply.ok(),
            Err(error) => reply.error(io_error_code(&error)),
        }
    }
}

impl MetadataWorkspaceFs {
    fn lookup_child(
        &mut self,
        parent_node_id: NodeId,
        name: &str,
    ) -> Result<Option<(Node, FileAttr)>, fs2_daemon::LocalStoreError> {
        for child in self.store.list_children(parent_node_id)? {
            if child.name == name {
                let inode = self.inode_for_node(child.node_id);
                let attr = self.attr_for_node(&child, inode)?;

                return Ok(Some((child, attr)));
            }
        }
        Ok(None)
    }
}

fn path_has_git_component(path: &str) -> bool {
    path.split('/').any(|component| component == ".git")
}

const fn rule_path_kind(kind: NodeKind) -> fs2_rules::RulePathKind {
    match kind {
        NodeKind::Directory => fs2_rules::RulePathKind::Directory,
        NodeKind::File | NodeKind::Symlink => fs2_rules::RulePathKind::File,
    }
}

const fn rule_action_suppresses_upload(action: RuleAction) -> bool {
    matches!(
        action,
        RuleAction::Ignore
            | RuleAction::LocalOnly
            | RuleAction::Generated
            | RuleAction::DependencyCache
    )
}

fn default_path_suppresses_upload(
    path: &str,
    kind: NodeKind,
    engine: &fs2_rules::RuleEngine,
) -> std::io::Result<bool> {
    let workspace_path = WorkspacePath::parse(path).map_err(io_other)?;
    let resolution = engine
        .resolve(
            &workspace_path,
            rule_path_kind(kind),
            fs2_rules::EvaluationPurpose::NewLocalCreate,
            None,
        )
        .map_err(io_other)?;
    Ok(rule_action_suppresses_upload(
        resolution.effective_rule.action,
    ))
}

fn default_ancestor_suppresses_upload(
    path: &str,
    engine: &fs2_rules::RuleEngine,
) -> std::io::Result<bool> {
    let Some((parent, _)) = path.rsplit_once('/') else {
        return Ok(false);
    };
    if parent.is_empty() {
        return Ok(false);
    }
    let mut current = Some(parent);
    while let Some(path) = current {
        if default_path_suppresses_upload(path, NodeKind::Directory, engine)? {
            return Ok(true);
        }
        current = path.rsplit_once('/').map(|(ancestor, _)| ancestor);
    }
    Ok(false)
}

const fn regular_file_mode(mode: u32) -> u32 {
    let permission_bits = mode & 0o7777;
    if mode & libc::S_IFMT == libc::S_IFREG {
        mode
    } else {
        libc::S_IFREG | permission_bits
    }
}

pub fn mount_empty_workspace(mountpoint: impl AsRef<Path>) -> std::io::Result<BackgroundSession> {
    let options = empty_mount_options();
    fuser::spawn_mount2(EmptyWorkspaceFs::new(), mountpoint, &options)
}

pub fn mount_metadata_workspace(
    store: LocalStore,
    workspace_id: WorkspaceId,
    root_node_id: NodeId,
    mountpoint: impl AsRef<Path>,
) -> std::io::Result<BackgroundSession> {
    let options = metadata_mount_options();
    fuser::spawn_mount2(
        MetadataWorkspaceFs::new(store, workspace_id, root_node_id),
        mountpoint,
        &options,
    )
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

pub fn mount_hydrated_metadata_workspace(
    store: LocalStore,
    workspace_id: WorkspaceId,
    root_node_id: NodeId,
    hydration: HydrationConfig,
    mountpoint: impl AsRef<Path>,
) -> std::io::Result<BackgroundSession> {
    let write_cache_dir = prepare_hydration_cache_dirs(&hydration.cache_dir)?;
    let options = metadata_mount_options();
    fuser::spawn_mount2(
        MetadataWorkspaceFs::new(store, workspace_id, root_node_id)
            .with_hydration(hydration)
            .with_write_cache_dir(write_cache_dir),
        mountpoint,
        &options,
    )
}

fn empty_mount_options() -> Vec<MountOption> {
    vec![MountOption::RO, MountOption::FSName("fs2-empty".to_owned())]
}

fn metadata_mount_options() -> Vec<MountOption> {
    vec![MountOption::FSName("fs2-metadata".to_owned())]
}

fn blob_cache_path(cache_dir: &Path, blob_id: &BlobId) -> PathBuf {
    let mut name = String::new();
    for character in blob_id.to_string().chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            name.push(character);
        } else {
            name.push('_');
        }
    }
    cache_dir.join(name)
}

fn prepare_hydration_cache_dirs(cache_dir: &Path) -> std::io::Result<PathBuf> {
    let write_cache_dir = cache_dir.join("writes");
    create_private_dir_all(cache_dir)?;
    create_private_dir_all(&write_cache_dir)?;
    Ok(write_cache_dir)
}

fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_dir_permissions(path)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn io_other(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

fn io_error_code(error: &std::io::Error) -> i32 {
    if let Some(code) = error.raw_os_error() {
        return code;
    }
    match error.kind() {
        std::io::ErrorKind::NotFound => libc::ENOENT,
        std::io::ErrorKind::PermissionDenied => libc::EACCES,
        std::io::ErrorKind::InvalidInput => libc::EINVAL,
        std::io::ErrorKind::AlreadyExists => libc::EEXIST,
        std::io::ErrorKind::Unsupported => libc::ENOTSUP,
        _ => libc::EIO,
    }
}

fn add_entries(
    offset: i64,
    entries: impl IntoIterator<Item = impl FuseDirectoryEntry>,
    reply: &mut ReplyDirectory,
) {
    let skip = usize::try_from(offset.max(0)).unwrap_or(usize::MAX);
    for (entry, next_offset) in entries.into_iter().zip(1_i64..).skip(skip) {
        if reply.add(entry.inode(), next_offset, entry.kind(), entry.name()) {
            break;
        }
    }
}

fn is_editor_temp_name(name: &str) -> bool {
    if name == "4913" || name.ends_with('~') {
        return true;
    }
    let lowercase = name.to_ascii_lowercase();
    [".tmp", ".temp", ".swp", ".swo", ".swx"]
        .iter()
        .any(|suffix| lowercase.ends_with(suffix))
}

fn reject_cross_boundary_move(old_suppressed: bool, new_suppressed: bool) -> std::io::Result<()> {
    if old_suppressed == new_suppressed {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "moves across generated/local-only boundaries must be copied explicitly",
    ))
}

const fn file_blob_id(revision: &NodeRevision) -> Option<&BlobId> {
    match &revision.content {
        RevisionContent::File { blob_id, .. } => Some(blob_id),
        _ => None,
    }
}

trait FuseDirectoryEntry {
    fn inode(&self) -> u64;
    fn kind(&self) -> FileType;
    fn name(&self) -> &str;
}

impl FuseDirectoryEntry for RootDirectoryEntry {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn kind(&self) -> FileType {
        self.kind
    }

    fn name(&self) -> &str {
        self.name
    }
}

impl FuseDirectoryEntry for DirectoryEntry {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn kind(&self) -> FileType {
        self.kind
    }

    fn name(&self) -> &str {
        &self.name
    }
}

const fn root_attr(timestamp: SystemTime) -> FileAttr {
    FileAttr {
        ino: ROOT_INO,
        size: 0,
        blocks: 0,
        atime: timestamp,
        mtime: timestamp,
        ctime: timestamp,
        crtime: timestamp,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

fn node_attr(node: &Node, inode: u64) -> FileAttr {
    let created = datetime_to_system_time(node.created_at);
    let updated = datetime_to_system_time(node.updated_at);
    let mut attr = root_attr(updated);
    attr.ino = inode;
    attr.kind = file_type(node.kind);
    attr.perm = match node.kind {
        NodeKind::Directory => 0o755,
        NodeKind::File => 0o644,
        NodeKind::Symlink => 0o777,
    };
    attr.nlink = if node.kind == NodeKind::Directory {
        2
    } else {
        1
    };
    attr.ctime = updated;
    attr.crtime = created;
    attr
}

fn datetime_to_system_time(datetime: chrono::DateTime<chrono::Utc>) -> SystemTime {
    let seconds = datetime.timestamp();
    let nanos = Duration::from_nanos(u64::from(datetime.timestamp_subsec_nanos()));
    if let Ok(seconds) = u64::try_from(seconds) {
        return SystemTime::UNIX_EPOCH + Duration::from_secs(seconds) + nanos;
    }
    let before_epoch = Duration::from_secs(seconds.unsigned_abs())
        .checked_sub(nanos)
        .unwrap_or(Duration::ZERO);
    SystemTime::UNIX_EPOCH - before_epoch
}

const fn file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::Directory => FileType::Directory,
        NodeKind::File => FileType::RegularFile,
        NodeKind::Symlink => FileType::Symlink,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2_core::{
        Cursor, FsRule, NodeRevision, Operation, OperationKind, RevisionContent, RevisionId,
    };
    use fs2_sync::{InboundSync, OutboundQueue};

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-fuse");
    }

    #[cfg(unix)]
    #[test]
    fn cache_directories_are_user_only() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("cache");

        let writes_dir = prepare_hydration_cache_dirs(&cache_dir)?;

        assert_eq!(
            fs::metadata(&cache_dir)?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&writes_dir)?.permissions().mode() & 0o777,
            0o700
        );
        Ok(())
    }

    #[test]
    fn empty_workspace_root_has_directory_metadata() {
        let fs = EmptyWorkspaceFs::new();
        let attr = fs.root_attr();
        assert_eq!(fs.root_inode(), ROOT_INO);
        assert_eq!(attr.ino, ROOT_INO);
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.perm, 0o755);
        assert_eq!(attr.nlink, 2);
    }

    #[test]
    fn empty_workspace_root_lists_dot_entries_only() {
        let fs = EmptyWorkspaceFs::new();
        assert_eq!(
            fs.root_entries(),
            [
                RootDirectoryEntry {
                    inode: ROOT_INO,
                    kind: FileType::Directory,
                    name: ".",
                },
                RootDirectoryEntry {
                    inode: ROOT_INO,
                    kind: FileType::Directory,
                    name: "..",
                },
            ]
        );
    }

    #[test]
    fn generated_paths_do_not_queue_upload_ops() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = LocalStore::in_memory()?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        store.initialize_workspace(workspace_id, "generated", root_id)?;
        let cache = tempfile::tempdir()?;
        let mut fs = MetadataWorkspaceFs::new(store, workspace_id, root_id)
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));

        let (dir, _, _) = fs.create_local_node(root_id, "node_modules", NodeKind::Directory)?;
        let (file, _, _) = fs.create_local_node(dir.node_id, "pkg.js", NodeKind::File)?;
        let handle = fs.begin_write_handle(file.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"generated")?;
        let revision = fs
            .commit_write_handle(handle)?
            .ok_or_else(|| "write did not commit".to_owned())?;

        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());
        if let Some(blob_id) = file_blob_id(&revision) {
            assert!(fs.store.pending_blob_upload(blob_id)?.is_none());
        }
        assert_eq!(fs.read_file(file.node_id, 0, 9)?, b"generated");
        fs.set_node_mode(file.node_id, 0o755)?;
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());

        fs.move_local_node(dir.node_id, "pkg.js", dir.node_id, "pkg-renamed.js")?;
        assert!(fs.resolve_path("node_modules/pkg-renamed.js")?.is_some());
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());
        let generated_to_normal =
            fs.move_local_node(dir.node_id, "pkg-renamed.js", root_id, "pkg.js");
        assert_eq!(
            generated_to_normal.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        assert!(fs.resolve_path("node_modules/pkg-renamed.js")?.is_some());
        assert!(fs.resolve_path("pkg.js")?.is_none());
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());

        let (temp_file, _, _) = fs.create_local_node(dir.node_id, "scratch.tmp", NodeKind::File)?;
        let handle = fs.begin_write_handle(temp_file.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"temp")?;
        fs.commit_write_handle(handle)?;
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());

        for index in 0..128 {
            let name = format!("pkg-{index}.js");
            let (stress_file, _, _) = fs.create_local_node(dir.node_id, &name, NodeKind::File)?;
            let handle = fs.begin_write_handle(stress_file.node_id, true, None)?;
            fs.write_to_handle(handle, 0, b"install output")?;
            fs.commit_write_handle(handle)?;
        }
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());

        let (target_dir, _, _) = fs.create_local_node(root_id, "target", NodeKind::Directory)?;
        for index in 0..128 {
            let name = format!("artifact-{index}.o");
            let (artifact, _, _) =
                fs.create_local_node(target_dir.node_id, &name, NodeKind::File)?;
            let handle = fs.begin_write_handle(artifact.node_id, true, None)?;
            fs.write_to_handle(handle, 0, b"cargo output")?;
            fs.commit_write_handle(handle)?;
        }
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());
        fs.delete_local_node(root_id, "target", true)?;
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());

        fs.delete_local_node(root_id, "node_modules", true)?;

        assert!(fs.resolve_path("node_modules")?.is_none());
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());
        Ok(())
    }

    #[test]
    fn normal_to_generated_move_is_rejected_without_stale_remote_state(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;
        let (generated_dir, _, _) =
            fs.create_local_node(ids.root, "node_modules", NodeKind::Directory)?;
        assert!(fs.store.list_pending_ops(ids.workspace)?.is_empty());

        let result =
            fs.move_local_node(ids.project, "README.md", generated_dir.node_id, "README.md");

        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        assert!(fs.resolve_path("project/README.md")?.is_some());
        assert!(fs.resolve_path("node_modules/README.md")?.is_none());
        assert!(fs.store.list_pending_ops(ids.workspace)?.is_empty());
        Ok(())
    }

    #[test]
    fn normal_override_below_generated_parent_is_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut store = LocalStore::in_memory()?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        let device_id = DeviceId::new_v4();
        store.initialize_workspace(workspace_id, "normal child override", root_id)?;
        let rule = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id,
            device_id,
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::SetRule {
                path_pattern: "target/app/".to_owned(),
                rule: FsRule {
                    action: RuleAction::Normal,
                    manager: None,
                    scope: None,
                },
            },
            created_at: chrono::Utc::now(),
        };
        store.apply_committed_operation(&rule, Cursor::new(1)?)?;
        let mut fs = MetadataWorkspaceFs::new(store, workspace_id, root_id);

        let (target, _, _) = fs.create_local_node(root_id, "target", NodeKind::Directory)?;
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());
        let app = fs.create_local_node(target.node_id, "app", NodeKind::Directory);

        assert_eq!(
            app.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        assert!(fs.resolve_path("target/app")?.is_none());
        assert!(fs.store.list_pending_ops(workspace_id)?.is_empty());

        let (normal_app, _, _) = fs.create_local_node(root_id, "app", NodeKind::Directory)?;
        assert_eq!(fs.store.list_pending_ops(workspace_id)?.len(), 1);
        let move_app = fs.move_local_node(root_id, "app", target.node_id, "app");
        assert_eq!(
            move_app.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        assert!(fs.resolve_path("app")?.is_some());
        assert!(fs.resolve_path("target/app")?.is_none());
        let pending = fs.store.list_pending_ops(workspace_id)?;
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].operation.kind,
            OperationKind::CreateNode { node_id, .. } if node_id == normal_app.node_id
        ));
        Ok(())
    }

    #[test]
    fn explicit_normal_rule_overrides_default_generated_suppression(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut store = LocalStore::in_memory()?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        let device_id = DeviceId::new_v4();
        store.initialize_workspace(workspace_id, "normal override", root_id)?;
        let rule = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id,
            device_id,
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::SetRule {
                path_pattern: "node_modules/".to_owned(),
                rule: FsRule {
                    action: RuleAction::Normal,
                    manager: None,
                    scope: None,
                },
            },
            created_at: chrono::Utc::now(),
        };
        store.apply_committed_operation(&rule, Cursor::new(1)?)?;
        let cache = tempfile::tempdir()?;
        let mut fs = MetadataWorkspaceFs::new(store, workspace_id, root_id)
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));

        let (dir, _, _) = fs.create_local_node(root_id, "node_modules", NodeKind::Directory)?;
        let (file, _, _) = fs.create_local_node(dir.node_id, "pkg.js", NodeKind::File)?;
        let handle = fs.begin_write_handle(file.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"upload")?;
        fs.commit_write_handle(handle)?;

        let pending = fs.store.list_pending_ops(workspace_id)?;
        assert_eq!(pending.len(), 3);
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::CreateNode { node_id, .. } if node_id == dir.node_id
        )));
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::PutFileRevision { node_id, .. } if node_id == file.node_id
        )));
        Ok(())
    }

    #[test]
    fn metadata_workspace_resolves_paths_and_lists_children_without_hydration(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;
        assert!(fs.resolve_path("project/README.md")?.is_some());
        let root_entries = fs.directory_entries(ids.root)?;
        assert!(root_entries.iter().any(|entry| entry.name == "project"));
        let project_entries = fs.directory_entries(ids.project)?;
        assert!(project_entries
            .iter()
            .any(|entry| entry.name == "README.md"));
        assert!(project_entries
            .iter()
            .any(|entry| entry.name == "README.link"));
        let readme_inode = fs.inode_for_node(ids.file);
        let readme = fs
            .resolve_path("project/README.md")?
            .ok_or_else(|| "file metadata missing".to_owned())?;
        let file_attr = fs.attr_for_node(&readme, readme_inode)?;
        assert_eq!(file_attr.kind, FileType::RegularFile);
        assert_eq!(file_attr.size, 12);
        assert_eq!(file_attr.perm, 0o755);
        assert_ne!(file_attr.mtime, SystemTime::UNIX_EPOCH);
        assert_ne!(file_attr.crtime, SystemTime::UNIX_EPOCH);
        let link_inode = fs.inode_for_node(ids.symlink);
        let link = fs
            .resolve_path("project/README.link")?
            .ok_or_else(|| "symlink metadata missing".to_owned())?;
        let link_attr = fs.attr_for_node(&link, link_inode)?;
        assert_eq!(link_attr.kind, FileType::Symlink);
        assert_eq!(link_attr.size, 9);
        let state = fs
            .store
            .node_state(ids.file)?
            .map(|state| state.hydration_state);
        assert_eq!(state, Some(fs2_daemon::HydrationState::MetadataOnly));
        Ok(())
    }

    #[test]
    fn create_local_nodes_adds_metadata_and_pending_ops() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut store = LocalStore::in_memory()?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        store.initialize_workspace(workspace_id, "create", root_id)?;
        let cache = tempfile::tempdir()?;
        let mut fs = MetadataWorkspaceFs::new(store, workspace_id, root_id)
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));
        let (dir, dir_attr, _dir_op_id) =
            fs.create_local_node(root_id, "src", NodeKind::Directory)?;
        let (file, file_attr, _file_op_id) =
            fs.create_local_node(root_id, "main.rs", NodeKind::File)?;

        assert_eq!(dir_attr.kind, FileType::Directory);
        assert_eq!(file_attr.kind, FileType::RegularFile);
        assert!(fs.resolve_path("src")?.is_some());
        assert!(fs.resolve_path("main.rs")?.is_some());
        let handle = fs.begin_write_handle(file.node_id, true, None)?;
        assert_eq!(
            fs.write_handles.get(&handle).map(|handle| handle.node_id),
            Some(file.node_id)
        );
        let pending = fs.store.list_pending_ops(workspace_id)?;
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::CreateNode { node_id, .. } if node_id == dir.node_id
        )));
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::CreateNode { node_id, .. } if node_id == file.node_id
        )));
        Ok(())
    }

    #[test]
    fn atomic_temp_replace_uploads_final_revision_once() -> Result<(), Box<dyn std::error::Error>> {
        let (fs, ids) = metadata_fixture()?;
        let cache = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));

        let (temp, _, _) = fs.create_local_node(ids.project, ".README.md.swp", NodeKind::File)?;
        let handle = fs.begin_write_handle(temp.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"updated")?;
        fs.commit_write_handle(handle)?;
        let handle = fs.begin_write_handle(temp.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"final!!")?;
        fs.commit_write_handle(handle)?;
        fs.set_node_mode(temp.node_id, 0o755)?;
        assert_eq!(fs.store.list_pending_ops(ids.workspace)?.len(), 4);

        fs.move_local_node(ids.project, ".README.md.swp", ids.project, "README.md")?;

        assert!(fs.resolve_path("project/.README.md.swp")?.is_none());
        assert_eq!(fs.read_file(ids.file, 0, 7)?, b"final!!");
        let pending = fs.store.list_pending_ops(ids.workspace)?;
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].operation.kind,
            OperationKind::PutFileRevision { node_id, .. } if node_id == ids.file
        ));
        if let OperationKind::PutFileRevision { revision, .. } = &pending[0].operation.kind {
            assert_eq!(revision.posix_mode, 0o100_755);
            if let Some(blob_id) = file_blob_id(revision) {
                assert!(fs.store.pending_blob_upload(blob_id)?.is_some());
            }
        }
        Ok(())
    }

    #[test]
    fn atomic_temp_rename_to_new_file_queues_final_create_only(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (fs, ids) = metadata_fixture()?;
        let cache = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));

        let (temp, _, _) = fs.create_local_node(ids.project, "new.txt.tmp", NodeKind::File)?;
        let handle = fs.begin_write_handle(temp.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"new file")?;
        fs.commit_write_handle(handle)?;
        fs.set_node_mode(temp.node_id, 0o755)?;
        assert_eq!(fs.store.list_pending_ops(ids.workspace)?.len(), 3);

        fs.move_local_node(ids.project, "new.txt.tmp", ids.project, "new.txt")?;

        assert!(fs.resolve_path("project/new.txt.tmp")?.is_none());
        let new_file = fs
            .resolve_path("project/new.txt")?
            .ok_or_else(|| "new file missing".to_owned())?;
        assert_eq!(fs.read_file(new_file.node_id, 0, 8)?, b"new file");
        let pending = fs.store.list_pending_ops(ids.workspace)?;
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            &pending[0].operation.kind,
            OperationKind::CreateNode { name, initial_revision: Some(revision), .. }
                if name == "new.txt" && revision.posix_mode == 0o100_755
        ));
        Ok(())
    }

    #[test]
    fn deleting_editor_temp_discards_noisy_pending_ops() -> Result<(), Box<dyn std::error::Error>> {
        let (fs, ids) = metadata_fixture()?;
        let cache = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));

        let (temp, _, _) = fs.create_local_node(ids.project, ".README.md.swp", NodeKind::File)?;
        let handle = fs.begin_write_handle(temp.node_id, true, None)?;
        fs.write_to_handle(handle, 0, b"swap")?;
        fs.commit_write_handle(handle)?;
        assert_eq!(fs.store.list_pending_ops(ids.workspace)?.len(), 2);

        fs.delete_local_node(ids.project, ".README.md.swp", false)?;

        assert!(fs.resolve_path("project/.README.md.swp")?.is_none());
        assert!(fs.store.list_pending_ops(ids.workspace)?.is_empty());
        Ok(())
    }

    #[test]
    fn git_internal_creates_and_moves_do_not_queue_ops() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;

        let root_git = fs.create_local_node(ids.root, ".git", NodeKind::Directory);
        assert_eq!(
            root_git.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        let nested_git = fs.create_local_node(ids.project, ".git", NodeKind::Directory);
        assert_eq!(
            nested_git.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        let move_into_git_name = fs.move_local_node(ids.project, "README.md", ids.root, ".git");
        assert_eq!(
            move_into_git_name.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        assert!(fs.store.list_pending_ops(ids.workspace)?.is_empty());
        assert!(fs.resolve_path(".git")?.is_none());
        assert!(fs.resolve_path("project/.git")?.is_none());
        Ok(())
    }

    #[test]
    fn moving_tree_with_git_descendant_does_not_queue_op() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut fs, ids) = metadata_fixture()?;
        let repo_id = NodeId::new_v4();
        let git_id = NodeId::new_v4();
        let index_id = NodeId::new_v4();
        for (cursor, node_id, parent_id, name, kind) in [
            (4, repo_id, ids.root, "repo", NodeKind::Directory),
            (5, git_id, repo_id, ".git", NodeKind::Directory),
            (6, index_id, git_id, "index", NodeKind::File),
        ] {
            let operation = Operation {
                op_id: fs2_core::OpId::new_v4(),
                workspace_id: ids.workspace,
                device_id: ids.device,
                base_cursor: Cursor::new(cursor - 1)?,
                kind: OperationKind::CreateNode {
                    node_id,
                    parent_id,
                    name: name.to_owned(),
                    kind,
                    initial_revision: None,
                },
                created_at: chrono::Utc::now(),
            };
            fs.store
                .apply_committed_operation(&operation, Cursor::new(cursor)?)?;
        }

        let result = fs.move_local_node(ids.root, "repo", ids.root, "repo2");

        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        assert!(fs.resolve_path("repo/.git/index")?.is_some());
        assert!(fs.resolve_path("repo2/.git/index")?.is_none());
        assert!(fs.store.list_pending_ops(ids.workspace)?.is_empty());
        Ok(())
    }

    #[test]
    fn create_local_node_rejects_portable_sibling_collision(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut store = LocalStore::in_memory()?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        store.initialize_workspace(workspace_id, "create-collision", root_id)?;
        let mut fs = MetadataWorkspaceFs::new(store, workspace_id, root_id);
        fs.create_local_node(root_id, "Readme.md", NodeKind::File)?;

        let Err(error) = fs.create_local_node(root_id, "README.md", NodeKind::File) else {
            return Err("case-folded duplicate unexpectedly succeeded".into());
        };

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs.store.list_pending_ops(workspace_id)?.len(), 1);
        assert!(fs.resolve_path("README.md")?.is_none());
        Ok(())
    }

    #[test]
    fn move_and_delete_local_nodes_queue_pending_ops() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;
        fs.move_local_node(ids.project, "README.md", ids.root, "README-renamed.md")?;
        assert!(fs.resolve_path("README-renamed.md")?.is_some());
        assert!(fs.resolve_path("project/README.md")?.is_none());
        fs.delete_local_node(ids.project, "README.link", false)?;
        assert!(fs.resolve_path("project/README.link")?.is_none());
        let pending = fs.store.list_pending_ops(ids.workspace)?;
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::MoveNode { node_id, .. } if node_id == ids.file
        )));
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::DeleteNode { node_id, .. } if node_id == ids.symlink
        )));
        Ok(())
    }

    #[test]
    fn set_node_mode_queues_metadata_revision() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;

        let revision = fs.set_node_mode(ids.file, 0o755)?;

        assert_eq!(revision.posix_mode, 0o100_755);
        assert!(revision.executable);
        let node = fs
            .store
            .get_node_by_id(ids.file)?
            .ok_or_else(|| "file missing after chmod".to_owned())?;
        let inode = fs.inode_for_node(ids.file);
        let attr = fs.attr_for_node(&node, inode)?;
        assert_eq!(attr.perm, 0o755);
        let pending = fs.store.list_pending_ops(ids.workspace)?;
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().any(|pending| matches!(
            &pending.operation.kind,
            OperationKind::PutFileRevision {
                node_id,
                revision,
                ..
            } if *node_id == ids.file && revision.executable && revision.posix_mode == 0o100_755
        )));
        Ok(())
    }

    #[test]
    fn set_node_mode_updates_open_write_handles() -> Result<(), Box<dyn std::error::Error>> {
        let (fs, ids) = metadata_fixture()?;
        let temp = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: temp.path().join("cache"),
            })
            .with_write_cache_dir(temp.path().join("writes"));
        let handle = fs.begin_write_handle(ids.file, true, None)?;
        fs.write_to_handle(handle, 0, b"updated")?;

        let chmod_revision = fs.set_node_mode(ids.file, 0o644)?;
        let write_revision = fs
            .commit_write_handle(handle)?
            .ok_or_else(|| "write handle did not commit".to_owned())?;

        assert_eq!(
            write_revision.base_revision_id,
            Some(chmod_revision.revision_id)
        );
        assert_eq!(write_revision.posix_mode, 0o100_644);
        assert!(!write_revision.executable);
        Ok(())
    }

    #[test]
    fn set_node_mode_does_not_rebase_stale_write_handles() -> Result<(), Box<dyn std::error::Error>>
    {
        let (fs, ids) = metadata_fixture()?;
        let temp = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: temp.path().join("cache"),
            })
            .with_write_cache_dir(temp.path().join("writes"));
        let original_revision_id = fs
            .store
            .get_node_by_id(ids.file)?
            .ok_or_else(|| "file missing".to_owned())?
            .current_rev
            .ok_or_else(|| "file revision missing".to_owned())?;
        let stale_handle = fs.begin_write_handle(ids.file, true, None)?;
        let latest_handle = fs.begin_write_handle(ids.file, true, None)?;
        fs.write_to_handle(latest_handle, 0, b"newer")?;
        let latest_revision = fs
            .commit_write_handle(latest_handle)?
            .ok_or_else(|| "latest write did not commit".to_owned())?;

        let chmod_revision = fs.set_node_mode(ids.file, 0o644)?;

        let stale = fs
            .write_handles
            .get(&stale_handle)
            .ok_or_else(|| "stale handle missing".to_owned())?;
        assert_eq!(
            chmod_revision.base_revision_id,
            Some(latest_revision.revision_id)
        );
        assert_eq!(stale.base_revision_id, Some(original_revision_id));
        assert_eq!(stale.posix_mode, 0o100_755);
        assert!(stale.executable);
        Ok(())
    }

    #[test]
    fn open_handle_mode_sets_first_file_revision() -> Result<(), Box<dyn std::error::Error>> {
        let (fs, ids) = metadata_fixture()?;
        let temp = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: temp.path().join("cache"),
            })
            .with_write_cache_dir(temp.path().join("writes"));
        let (file, _attr, _op_id) =
            fs.create_local_node(ids.root, "new-script.sh", NodeKind::File)?;
        let handle = fs.begin_write_handle(file.node_id, true, Some(0o644))?;

        assert!(fs.set_open_write_handle_mode(file.node_id, 0o755));
        fs.write_to_handle(handle, 0, b"#!/bin/sh\n")?;
        let revision = fs
            .commit_write_handle(handle)?
            .ok_or_else(|| "write handle did not commit".to_owned())?;

        assert_eq!(revision.posix_mode, 0o100_755);
        assert!(revision.executable);
        Ok(())
    }

    #[test]
    fn set_node_mode_after_dirty_write_clears_after_acks() -> Result<(), Box<dyn std::error::Error>>
    {
        let (fs, ids) = metadata_fixture()?;
        let temp = tempfile::tempdir()?;
        let mut fs = fs
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: WorkspaceContentKey::generate(),
                cache_dir: temp.path().join("cache"),
            })
            .with_write_cache_dir(temp.path().join("writes"));
        let handle = fs.begin_write_handle(ids.file, true, None)?;
        fs.write_to_handle(handle, 0, b"updated")?;
        let write_revision = fs
            .commit_write_handle(handle)?
            .ok_or_else(|| "write handle did not commit".to_owned())?;
        fs.set_node_mode(ids.file, 0o644)?;
        assert_eq!(
            fs.store
                .node_state(ids.file)?
                .ok_or_else(|| "node state missing".to_owned())?
                .dirty_base_revision_id,
            Some(write_revision.revision_id)
        );
        let pending = fs.store.list_pending_ops(ids.workspace)?;
        assert_eq!(pending.len(), 2);

        fs.store
            .apply_committed_operation(&pending[0].operation, Cursor::new(4)?)?;
        fs.store
            .apply_committed_operation(&pending[1].operation, Cursor::new(5)?)?;

        assert_eq!(
            fs.store
                .node_state(ids.file)?
                .ok_or_else(|| "node state missing after ack".to_owned())?
                .hydration_state,
            HydrationState::Hydrated
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rename_and_delete_on_a_converge_to_b() -> Result<(), Box<dyn std::error::Error>> {
        let harness = fs2_testkit::TwoClientHarness::start()?;
        harness.simulate_file_creation_on_a("move-me.txt", b"move")?;
        harness.simulate_file_creation_on_a("delete-me.txt", b"delete")?;
        let mut origin_store = harness.open_client_a_store()?;
        let mut mirror_store = harness.open_client_b_store()?;
        InboundSync::new(&harness.client_a, harness.workspace_id, Duration::ZERO)
            .sync_startup(&mut origin_store)?;
        harness.sync_b(&mut mirror_store)?;

        let mut metadata_fs =
            MetadataWorkspaceFs::new(origin_store, harness.workspace_id, harness.root_node_id);
        metadata_fs.move_local_node(
            harness.root_node_id,
            "move-me.txt",
            harness.root_node_id,
            "moved.txt",
        )?;
        metadata_fs.delete_local_node(harness.root_node_id, "delete-me.txt", false)?;
        let report = OutboundQueue::new(&harness.client_a).drain_workspace(
            &mut metadata_fs.store,
            harness.workspace_id,
            &[],
        )?;
        assert_eq!(report.submitted, 2);
        assert_eq!(report.failed, None);

        harness.sync_b(&mut mirror_store)?;
        assert!(mirror_store
            .get_node_by_path(harness.workspace_id, "moved.txt")?
            .is_some());
        assert!(mirror_store
            .get_node_by_path(harness.workspace_id, "move-me.txt")?
            .is_none());
        assert!(mirror_store
            .get_node_by_path(harness.workspace_id, "delete-me.txt")?
            .is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chmod_on_a_converges_to_b() -> Result<(), Box<dyn std::error::Error>> {
        let harness = fs2_testkit::TwoClientHarness::start()?;
        let simulated = harness.simulate_file_creation_on_a("script.sh", b"#!/bin/sh\n")?;
        let OperationKind::CreateNode { node_id, .. } = simulated.operation.kind else {
            unreachable!("simulate_file_creation_on_a creates nodes")
        };
        let mut origin_store = harness.open_client_a_store()?;
        let mut mirror_store = harness.open_client_b_store()?;
        InboundSync::new(&harness.client_a, harness.workspace_id, Duration::ZERO)
            .sync_startup(&mut origin_store)?;
        harness.sync_b(&mut mirror_store)?;

        let mut metadata_fs =
            MetadataWorkspaceFs::new(origin_store, harness.workspace_id, harness.root_node_id)
                .with_device_id(harness.device_a_id);
        let revision = metadata_fs.set_node_mode(node_id, 0o755)?;
        assert!(revision.executable);
        let report = OutboundQueue::new(&harness.client_a).drain_workspace(
            &mut metadata_fs.store,
            harness.workspace_id,
            &[],
        )?;
        assert_eq!(report.failed, None);
        assert_eq!(report.submitted, 1);

        harness.sync_b(&mut mirror_store)?;
        let script = mirror_store
            .get_node_by_path(harness.workspace_id, "script.sh")?
            .ok_or_else(|| "script missing on mirror".to_owned())?;
        let mirror_revision = mirror_store
            .get_revision(
                script
                    .current_rev
                    .ok_or_else(|| "script has no revision on mirror".to_owned())?,
            )?
            .ok_or_else(|| "script revision missing on mirror".to_owned())?;
        assert_eq!(mirror_revision.posix_mode, 0o100_755);
        assert!(mirror_revision.executable);
        Ok(())
    }

    #[test]
    fn mounted_mkdir_and_create_record_pending_ops() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("metadata.sqlite");
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        let mut store = LocalStore::open(&db_path)?;
        store.initialize_workspace(workspace_id, "mounted-create", root_id)?;
        let mountpoint = tempfile::tempdir()?;
        let session = mount_metadata_workspace(store, workspace_id, root_id, mountpoint.path())?;
        fs::create_dir(mountpoint.path().join("src"))?;
        let created_file = fs::File::create(mountpoint.path().join("created.txt"))?;
        drop(created_file);
        drop(session);

        let reopened = LocalStore::open(&db_path)?;
        let pending = reopened.list_pending_ops(workspace_id)?;
        assert_eq!(pending.len(), 2);
        assert!(reopened.get_node_by_path(workspace_id, "src")?.is_some());
        assert!(reopened
            .get_node_by_path(workspace_id, "created.txt")?
            .is_some());
        Ok(())
    }

    #[test]
    fn write_commit_encrypts_blob_and_marks_node_dirty() -> Result<(), Box<dyn std::error::Error>> {
        let mut store = LocalStore::in_memory()?;
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        store.initialize_workspace(workspace_id, "write", root_id)?;
        let cache = tempfile::tempdir()?;
        let content_key = WorkspaceContentKey::generate();
        let mut fs = MetadataWorkspaceFs::new(store, workspace_id, root_id)
            .with_hydration(HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key: content_key.clone(),
                cache_dir: cache.path().join("cache"),
            })
            .with_write_cache_dir(cache.path().join("writes"));
        let (file, _, _op_id) = fs.create_local_node(root_id, "hello.txt", NodeKind::File)?;
        let handle = fs.begin_write_handle(file.node_id, true, None)?;
        assert_eq!(fs.write_to_handle(handle, 0, b"hello")?, 5);
        let revision = fs
            .commit_write_handle(handle)?
            .ok_or_else(|| "write handle was not committed".to_owned())?;

        assert_eq!(revision.size, 5);
        let RevisionContent::File {
            blob_id,
            encryption_header,
            ..
        } = &revision.content
        else {
            return Err("committed revision was not file content".into());
        };
        let pending_upload = fs
            .store
            .pending_blob_upload(blob_id)?
            .ok_or_else(|| "pending blob upload missing".to_owned())?;
        let header_json = pending_upload
            .encryption_header
            .ok_or_else(|| "pending blob upload encryption header missing".to_owned())?;
        assert_eq!(Some(header_json.as_str()), encryption_header.as_deref());
        let header = serde_json::from_str(&header_json)?;
        assert_eq!(
            decrypt_blob(
                &EncryptedBlob {
                    blob_id: blob_id.clone(),
                    header,
                    ciphertext: pending_upload.bytes,
                },
                &content_key,
            )?,
            b"hello"
        );
        let state = fs
            .store
            .node_state(file.node_id)?
            .ok_or_else(|| "node state missing".to_owned())?;
        assert_eq!(state.hydration_state, HydrationState::Dirty);
        assert_eq!(state.dirty_base_revision_id, None);
        let local_blob_path = state
            .local_blob_path
            .ok_or_else(|| "dirty local path missing".to_owned())?;
        assert_eq!(fs::read(local_blob_path)?, b"hello");
        let pending = fs.store.list_pending_ops(workspace_id)?;
        assert_eq!(pending.len(), 2);
        let create_op = pending
            .iter()
            .find(|pending| matches!(pending.operation.kind, OperationKind::CreateNode { .. }))
            .ok_or_else(|| "pending create op missing".to_owned())?
            .operation
            .clone();
        let put_op = pending
            .iter()
            .find(|pending| {
                matches!(
                    pending.operation.kind,
                    OperationKind::PutFileRevision { .. }
                )
            })
            .ok_or_else(|| "pending put revision op missing".to_owned())?
            .operation
            .clone();
        assert_eq!(fs.read_file(file.node_id, 0, 5)?, b"hello");
        assert_eq!(
            fs.store
                .node_state(file.node_id)?
                .ok_or_else(|| "node state missing after read".to_owned())?
                .hydration_state,
            HydrationState::Dirty
        );
        let clean_handle = fs.begin_write_handle(file.node_id, false, None)?;
        assert!(fs.commit_write_handle(clean_handle)?.is_none());
        assert_eq!(fs.store.list_pending_ops(workspace_id)?.len(), 2);
        fs.store
            .apply_committed_operation(&create_op, Cursor::new(1)?)?;
        fs.store
            .apply_committed_operation(&put_op, Cursor::new(2)?)?;
        assert_eq!(
            fs.store
                .node_state(file.node_id)?
                .ok_or_else(|| "node state missing after ack".to_owned())?
                .hydration_state,
            HydrationState::Hydrated
        );
        Ok(())
    }

    #[test]
    fn mounted_mkdir_and_file_write_record_pending_ops() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("metadata.sqlite");
        let cache_dir = temp.path().join("cache");
        let workspace_id = WorkspaceId::new_v4();
        let root_id = NodeId::new_v4();
        let mut store = LocalStore::open(&db_path)?;
        store.initialize_workspace(workspace_id, "mounted-write", root_id)?;
        let content_key = WorkspaceContentKey::generate();
        let mountpoint = tempfile::tempdir()?;
        let session = mount_hydrated_metadata_workspace(
            store,
            workspace_id,
            root_id,
            HydrationConfig {
                client: ApiClient::new("http://127.0.0.1:1", "token")?,
                content_key,
                cache_dir,
            },
            mountpoint.path(),
        )?;
        fs::create_dir(mountpoint.path().join("src"))?;
        fs::write(mountpoint.path().join("file.txt"), b"hi\n")?;
        drop(session);

        let reopened = LocalStore::open(&db_path)?;
        let pending = reopened.list_pending_ops(workspace_id)?;
        assert_eq!(pending.len(), 3);
        let file = reopened
            .get_node_by_path(workspace_id, "file.txt")?
            .ok_or_else(|| "written file metadata missing".to_owned())?;
        let state = reopened
            .node_state(file.node_id)?
            .ok_or_else(|| "written file state missing".to_owned())?;
        assert_eq!(state.hydration_state, HydrationState::Dirty);
        let local_blob_path = state
            .local_blob_path
            .ok_or_else(|| "written file cache path missing".to_owned())?;
        assert_eq!(fs::read(local_blob_path)?, b"hi\n");
        assert!(pending.iter().any(|pending| matches!(
            pending.operation.kind,
            OperationKind::PutFileRevision { node_id, .. } if node_id == file.node_id
        )));
        Ok(())
    }

    #[test]
    fn read_file_reports_offline_metadata_only_error() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;
        let error = match fs.read_file(ids.file, 0, 16) {
            Ok(bytes) => {
                return Err(format!("read unexpectedly returned {} bytes", bytes.len()).into())
            }
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
        assert!(error.to_string().contains("metadata-only"));
        Ok(())
    }

    #[test]
    fn mounted_empty_workspace_lists_empty_root() -> Result<(), Box<dyn std::error::Error>> {
        let mountpoint = tempfile::tempdir()?;
        let _session = mount_empty_workspace(mountpoint.path())?;
        let entries = std::fs::read_dir(mountpoint.path())?.collect::<Result<Vec<_>, _>>()?;
        assert!(entries.is_empty());
        Ok(())
    }

    #[test]
    fn metadata_mount_options_do_not_enable_kernel_permission_filtering() {
        assert!(!metadata_mount_options()
            .iter()
            .any(|option| matches!(option, MountOption::DefaultPermissions)));
    }

    #[test]
    fn datetime_conversion_preserves_pre_epoch_metadata_times(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let before_epoch = chrono::DateTime::parse_from_rfc3339("1965-07-09T01:02:03.500Z")?
            .with_timezone(&chrono::Utc);
        assert!(datetime_to_system_time(before_epoch) < SystemTime::UNIX_EPOCH);
        Ok(())
    }

    #[test]
    fn mounted_metadata_workspace_can_be_browsed_cold() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("metadata.sqlite");
        let (workspace_id, root_id, file_id) = create_metadata_store(&db_path)?;
        let store = LocalStore::open(&db_path)?;
        let mountpoint = tempfile::tempdir()?;
        let session = mount_metadata_workspace(store, workspace_id, root_id, mountpoint.path())?;
        let root_entries = std::fs::read_dir(mountpoint.path())?.collect::<Result<Vec<_>, _>>()?;
        assert!(root_entries
            .iter()
            .any(|entry| entry.file_name() == "project"));
        let project = mountpoint.path().join("project");
        let project_entries = std::fs::read_dir(&project)?.collect::<Result<Vec<_>, _>>()?;
        assert!(project_entries
            .iter()
            .any(|entry| entry.file_name() == "README.md"));
        let metadata = std::fs::metadata(project.join("README.md"))?;
        assert!(metadata.is_file());
        drop(session);
        let reopened = LocalStore::open(&db_path)?;
        let state = reopened
            .node_state(file_id)?
            .map(|state| state.hydration_state);
        assert_eq!(state, Some(fs2_daemon::HydrationState::MetadataOnly));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_file_reuses_verified_blob_cache_without_backend(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let harness = fs2_testkit::TwoClientHarness::start()?;
        let simulated = harness.simulate_file_creation_on_a("cached.txt", b"cached bytes")?;
        let mut client_b_store = harness.open_client_b_store()?;
        assert_eq!(harness.sync_b(&mut client_b_store)?.applied, 1);
        let mut fs =
            MetadataWorkspaceFs::new(client_b_store, harness.workspace_id, harness.root_node_id);
        let node = fs
            .resolve_path(&simulated.path)?
            .ok_or_else(|| "synced file metadata missing".to_owned())?;
        let (blob_id, _header, expected_size) = fs.file_revision_blob(&node)?;
        let cache_dir = tempfile::tempdir()?;
        let cache_path = cache_dir.path().join("cached-plaintext");
        fs::write(&cache_path, b"cached bytes")?;
        let cache_path_string = cache_path.display().to_string();
        fs.store
            .mark_blob_cached(&blob_id, &cache_path_string, expected_size, true)?;

        assert_eq!(fs.read_file(node.node_id, 0, 64)?, b"cached bytes");
        let state = fs
            .store
            .node_state(node.node_id)?
            .ok_or_else(|| "cached state missing".to_owned())?;
        assert_eq!(state.hydration_state, HydrationState::Hydrated);
        assert_eq!(
            state.local_blob_path.as_deref(),
            Some(cache_path_string.as_str())
        );
        Ok(())
    }

    #[test]
    fn read_file_does_not_serve_stale_hydrated_path_after_revision_change(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut fs, ids) = metadata_fixture()?;
        let old_node = fs
            .store
            .get_node_by_id(ids.file)?
            .ok_or_else(|| "file node missing".to_owned())?;
        let old_revision = old_node
            .current_rev
            .ok_or_else(|| "file revision missing".to_owned())?;
        let cache_dir = tempfile::tempdir()?;
        let old_path = cache_dir.path().join("old-plaintext");
        fs::write(&old_path, b"old revision")?;
        let old_path_string = old_path.display().to_string();
        let (old_blob_id, _header, old_size) = fs.file_revision_blob(&old_node)?;
        fs.store
            .mark_blob_cached(&old_blob_id, &old_path_string, old_size, true)?;
        fs.store.set_hydration_state(
            ids.file,
            HydrationState::Hydrated,
            Some(&old_path_string),
            false,
        )?;
        let update = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: ids.workspace,
            device_id: ids.device,
            base_cursor: Cursor::new(3)?,
            kind: OperationKind::PutFileRevision {
                node_id: ids.file,
                base_revision_id: Some(old_revision),
                revision: NodeRevision {
                    revision_id: RevisionId::new_v4(),
                    node_id: ids.file,
                    workspace_id: ids.workspace,
                    device_id: ids.device,
                    base_revision_id: Some(old_revision),
                    content: RevisionContent::File {
                        blob_id: fs2_core::BlobId::new("sha256:new".to_owned())?,
                        chunk_ids: Vec::new(),
                        content_hash: "plaintext-sha256:new".to_owned(),
                        encryption_header: Some("{}".to_owned()),
                    },
                    posix_mode: 0o100_644,
                    mtime: chrono::Utc::now(),
                    size: 12,
                    executable: false,
                    created_at: chrono::Utc::now(),
                },
            },
            created_at: chrono::Utc::now(),
        };
        fs.store
            .apply_committed_operation(&update, Cursor::new(4)?)?;

        let error = match fs.read_file(ids.file, 0, 64) {
            Ok(bytes) => return Err(format!("served stale {} bytes", bytes.len()).into()),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_file_hydrates_then_serves_cached_bytes() -> Result<(), Box<dyn std::error::Error>>
    {
        let harness = fs2_testkit::TwoClientHarness::start()?;
        let simulated = harness.simulate_file_creation_on_a("file.txt", b"hello from fuse")?;
        let mut client_b_store = harness.open_client_b_store()?;
        assert_eq!(harness.sync_b(&mut client_b_store)?.applied, 1);
        let cache_dir = tempfile::tempdir()?;
        let mut fs =
            MetadataWorkspaceFs::new(client_b_store, harness.workspace_id, harness.root_node_id)
                .with_hydration(HydrationConfig {
                    client: harness.client_b.clone(),
                    content_key: harness.content_key.clone(),
                    cache_dir: cache_dir.path().to_path_buf(),
                });
        let node = fs
            .resolve_path(&simulated.path)?
            .ok_or_else(|| "synced file metadata missing".to_owned())?;

        assert_eq!(fs.read_file(node.node_id, 0, 5)?, b"hello");
        let state = fs
            .store
            .node_state(node.node_id)?
            .ok_or_else(|| "hydrated state missing".to_owned())?;
        assert_eq!(state.hydration_state, HydrationState::Hydrated);
        assert!(state.last_accessed_at.is_some());
        let cache_path = state
            .local_blob_path
            .ok_or_else(|| "hydrated cache path missing".to_owned())?;
        fs.hydration = None;
        assert_eq!(fs.read_file(node.node_id, 6, 4)?, b"from");
        assert_eq!(fs::read(cache_path)?, b"hello from fuse");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mounted_read_path_hydrates_file_on_first_cat() -> Result<(), Box<dyn std::error::Error>>
    {
        let harness = fs2_testkit::TwoClientHarness::start()?;
        harness.simulate_file_creation_on_a("cat.txt", b"cat downloads bytes")?;
        let mut client_b_store = harness.open_client_b_store()?;
        assert_eq!(harness.sync_b(&mut client_b_store)?.applied, 1);
        let cache_dir = tempfile::tempdir()?;
        let mountpoint = tempfile::tempdir()?;
        let session = mount_hydrated_metadata_workspace(
            client_b_store,
            harness.workspace_id,
            harness.root_node_id,
            HydrationConfig {
                client: harness.client_b.clone(),
                content_key: harness.content_key.clone(),
                cache_dir: cache_dir.path().to_path_buf(),
            },
            mountpoint.path(),
        )?;
        assert_eq!(
            fs::read_to_string(mountpoint.path().join("cat.txt"))?,
            "cat downloads bytes"
        );
        assert_eq!(
            fs::read_to_string(mountpoint.path().join("cat.txt"))?,
            "cat downloads bytes"
        );
        drop(session);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relative_symlink_round_trips_through_sync_and_fuse(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let harness = fs2_testkit::TwoClientHarness::start()?;
        let symlink_id = NodeId::new_v4();
        let op = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: harness.workspace_id,
            device_id: harness.device_a_id,
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::CreateNode {
                node_id: symlink_id,
                parent_id: harness.root_node_id,
                name: "README.link".to_owned(),
                kind: NodeKind::Symlink,
                initial_revision: Some(NodeRevision {
                    revision_id: RevisionId::new_v4(),
                    node_id: symlink_id,
                    workspace_id: harness.workspace_id,
                    device_id: harness.device_a_id,
                    base_revision_id: None,
                    content: RevisionContent::Symlink {
                        target: "README.md".to_owned(),
                    },
                    posix_mode: 0o120_777,
                    mtime: chrono::Utc::now(),
                    size: 9,
                    executable: false,
                    created_at: chrono::Utc::now(),
                }),
            },
            created_at: chrono::Utc::now(),
        };
        harness
            .client_a
            .submit_operation(harness.workspace_id, &op)?;
        let mut client_b_store = harness.open_client_b_store()?;
        assert_eq!(harness.sync_b(&mut client_b_store)?.applied, 1);
        let metadata_fs =
            MetadataWorkspaceFs::new(client_b_store, harness.workspace_id, harness.root_node_id);
        let link_node = metadata_fs
            .resolve_path("README.link")?
            .ok_or_else(|| "synced symlink metadata missing".to_owned())?;
        assert_eq!(metadata_fs.symlink_target(link_node.node_id)?, "README.md");
        let mountpoint = tempfile::tempdir()?;
        let session = mount_metadata_workspace(
            metadata_fs.store,
            harness.workspace_id,
            harness.root_node_id,
            mountpoint.path(),
        )?;
        assert_eq!(
            fs::read_link(mountpoint.path().join("README.link"))?,
            PathBuf::from("README.md")
        );
        drop(session);
        Ok(())
    }

    #[test]
    fn manual_test_instructions_cover_linux_and_macos() {
        assert!(MANUAL_TEST_INSTRUCTIONS.contains("Linux"));
        assert!(MANUAL_TEST_INSTRUCTIONS.contains("macOS"));
        assert!(MANUAL_TEST_INSTRUCTIONS.contains("ls <mount>"));
    }

    #[derive(Debug, Clone, Copy)]
    struct FixtureIds {
        workspace: WorkspaceId,
        device: fs2_core::DeviceId,
        root: NodeId,
        project: NodeId,
        file: NodeId,
        symlink: NodeId,
    }

    fn metadata_fixture() -> Result<(MetadataWorkspaceFs, FixtureIds), Box<dyn std::error::Error>> {
        let store = LocalStore::in_memory()?;
        populate_metadata_store(store)
    }

    fn create_metadata_store(
        db_path: &Path,
    ) -> Result<(WorkspaceId, NodeId, NodeId), Box<dyn std::error::Error>> {
        let store = LocalStore::open(db_path)?;
        let (_fs, ids) = populate_metadata_store(store)?;
        Ok((ids.workspace, ids.root, ids.file))
    }

    fn populate_metadata_store(
        mut store: LocalStore,
    ) -> Result<(MetadataWorkspaceFs, FixtureIds), Box<dyn std::error::Error>> {
        let ids = FixtureIds {
            workspace: WorkspaceId::new_v4(),
            device: fs2_core::DeviceId::new_v4(),
            root: NodeId::new_v4(),
            project: NodeId::new_v4(),
            file: NodeId::new_v4(),
            symlink: NodeId::new_v4(),
        };
        store.initialize_workspace(ids.workspace, "metadata", ids.root)?;
        let project = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: ids.workspace,
            device_id: ids.device,
            base_cursor: Cursor::new(0)?,
            kind: OperationKind::CreateNode {
                node_id: ids.project,
                parent_id: ids.root,
                name: "project".to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            created_at: chrono::Utc::now(),
        };
        store.apply_committed_operation(&project, Cursor::new(1)?)?;
        let revision_id = RevisionId::new_v4();
        let file = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: ids.workspace,
            device_id: ids.device,
            base_cursor: Cursor::new(1)?,
            kind: OperationKind::CreateNode {
                node_id: ids.file,
                parent_id: ids.project,
                name: "README.md".to_owned(),
                kind: NodeKind::File,
                initial_revision: Some(NodeRevision {
                    revision_id,
                    node_id: ids.file,
                    workspace_id: ids.workspace,
                    device_id: ids.device,
                    base_revision_id: None,
                    content: RevisionContent::File {
                        blob_id: fs2_core::BlobId::new("sha256:test".to_owned())?,
                        chunk_ids: Vec::new(),
                        content_hash: "plaintext-sha256:test".to_owned(),
                        encryption_header: Some("{}".to_owned()),
                    },
                    posix_mode: 0o100_755,
                    mtime: chrono::Utc::now(),
                    size: 12,
                    executable: true,
                    created_at: chrono::Utc::now(),
                }),
            },
            created_at: chrono::Utc::now(),
        };
        store.apply_committed_operation(&file, Cursor::new(2)?)?;
        let symlink = Operation {
            op_id: fs2_core::OpId::new_v4(),
            workspace_id: ids.workspace,
            device_id: ids.device,
            base_cursor: Cursor::new(2)?,
            kind: OperationKind::CreateNode {
                node_id: ids.symlink,
                parent_id: ids.project,
                name: "README.link".to_owned(),
                kind: NodeKind::Symlink,
                initial_revision: Some(NodeRevision {
                    revision_id: RevisionId::new_v4(),
                    node_id: ids.symlink,
                    workspace_id: ids.workspace,
                    device_id: ids.device,
                    base_revision_id: None,
                    content: RevisionContent::Symlink {
                        target: "README.md".to_owned(),
                    },
                    posix_mode: 0o120_777,
                    mtime: chrono::Utc::now(),
                    size: 9,
                    executable: false,
                    created_at: chrono::Utc::now(),
                }),
            },
            created_at: chrono::Utc::now(),
        };
        store.apply_committed_operation(&symlink, Cursor::new(3)?)?;
        let fs = MetadataWorkspaceFs::new(store, ids.workspace, ids.root);
        Ok((fs, ids))
    }
}
