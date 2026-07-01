#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Read-only FUSE adapter skeleton for FS2 workspaces.

use fs2_core::{
    names_collide, BlobId, CasePolicy, DeviceId, Node, NodeId, NodeKind, NodeName, NodeRevision,
    Operation, OperationKind, RevisionContent, WorkspaceId,
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
        let handle = self.next_write_handle;
        self.next_write_handle = self.next_write_handle.saturating_add(1).max(1);
        fs::create_dir_all(&self.write_cache_dir)?;
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
            fs::create_dir_all(parent)?;
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
                blob_id: encrypted.blob_id,
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
        self.store
            .apply_local_pending_op(&operation)
            .map_err(io_other)?;
        let pinned = self
            .store
            .node_state(write_handle.node_id)
            .map_err(io_other)?
            .is_some_and(|state| state.pinned);
        self.store
            .mark_node_dirty(
                write_handle.node_id,
                &cache_path_string,
                write_handle.base_revision_id,
                pinned,
            )
            .map_err(io_other)?;
        Ok(revision)
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
        fs::create_dir_all(&hydration.cache_dir)?;
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
        if mode.is_some()
            || uid.is_some()
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
        let Some(node_id) = self.inodes.node_for(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
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
    let write_cache_dir = hydration.cache_dir.join("writes");
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
    use fs2_core::{Cursor, NodeRevision, Operation, OperationKind, RevisionContent, RevisionId};

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-fuse");
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
