//! FUSE filesystem adapter (macOS macFUSE / Linux FUSE3).
//!
//! Implements a read-only metadata-backed FUSE filesystem over the local
//! `SQLite` store. Directory entries are served from metadata; file content is
//! lazily hydrated on first `read` through a [`BlobReader`] (which the daemon
//! backs with the blob store / cache).
//!
//! The adapter is unit-tested via the [`Fs2Backend`] and [`BlobReader`] traits
//! using in-memory mocks; live-mount acceptance is recorded as blocked in the
//! sandbox because kernel FUSE requests are not delivered here.
//!
//! Module boundaries follow `design.md` §3.2: `fs2fs` delegates sync and
//! hydration decisions to the daemon. The traits here are the internal
//! equivalent of the local RPC API (`design.md` §17) and can later be backed
//! by a Unix-domain-socket client instead of direct calls.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry};
use fs2_core::{NodeKind, NodeId, WorkspaceId};
use fs2_sync::{LocalNode, LocalRevision, LocalStore};
use parking_lot::Mutex;
use tracing::{debug, warn};

/// FUSE root inode (constant).
pub const ROOT_INO: u64 = 1;

const TTL: Duration = Duration::from_secs(1);

/// Metadata + content backend the FUSE adapter delegates to.
///
/// This mirrors the read-side of the local RPC API (`design.md` §17):
/// `get_node`, `list_children`, `read_file`, `readlink`. A production daemon
/// implements this against `LocalStore` plus the blob cache/hydrator; tests
/// implement it with in-memory fakes.
pub trait Fs2Backend: Send + Sync {
    /// Workspace id for this mount.
    fn workspace_id(&self) -> WorkspaceId;

    /// Root node id of the workspace.
    fn root_node_id(&self) -> NodeId;

    /// Look up a live child by name under `parent`.
    ///
    /// Returns the child node, or `None` if no such live child exists.
    fn lookup(&self, parent: NodeId, name: &str) -> Option<LocalNode>;

    /// Get a node by id.
    fn get_node(&self, node_id: NodeId) -> Option<LocalNode>;

    /// Get the revision for a node, including blob id and metadata.
    fn get_revision(&self, node_id: NodeId) -> Option<LocalRevision>;

    /// List live children of a directory node.
    fn list_children(&self, parent: NodeId) -> Vec<LocalNode>;

    /// Read file bytes for a node, hydrating on demand.
    ///
    /// Implementations should download + verify the blob if it is not cached,
    /// then return the full bytes. Returns an error if offline and not
    /// hydrated, or on verification failure.
    ///
    /// # Errors
    /// - [`ReadError::NotHydrated`] when bytes are absent and cannot be fetched.
    /// - [`ReadError::Io`] on backend/verification failure.
    fn read_file(&self, node_id: NodeId) -> Result<Vec<u8>, ReadError>;

    /// Read a symlink target string for a node.
    ///
    /// Returns `None` if the node is not a symlink.
    fn readlink(&self, node_id: NodeId) -> Option<String>;

    // ---- Write/mutation methods (design §6.4-6.6, §13) ----

    /// Create a directory node under `parent` with `name`.
    ///
    /// Creates the local node immediately and queues a pending `CreateNode`
    /// op. Returns the new node id.
    ///
    /// # Errors
    /// Returns a [`WriteError`] on collision or backend failure.
    fn mkdir(&self, parent: NodeId, name: &str) -> Result<NodeId, WriteError>;

    /// Create a file node under `parent` with `name` and open a write handle.
    ///
    /// Returns the new node id and a write handle id.
    ///
    /// # Errors
    /// Returns a [`WriteError`] on collision or backend failure.
    fn create_file(&self, parent: NodeId, name: &str, mode: u32) -> Result<(NodeId, u64), WriteError>;

    /// Write `data` at `offset` to the staging file for write handle `fh`.
    ///
    /// # Errors
    /// Returns a [`WriteError`] if the handle is invalid or the write fails.
    fn write(&self, fh: u64, offset: i64, data: &[u8]) -> Result<(), WriteError>;

    /// Flush/close a write handle: compute hash, queue upload + `PutFileRevision`,
    /// mark local state dirty until backend ack.
    ///
    /// # Errors
    /// Returns a [`WriteError`] on commit failure.
    fn flush(&self, fh: u64) -> Result<(), WriteError>;

    /// Release a write handle (drop staging state). Idempotent.
    fn release(&self, fh: u64);

    /// Rename/move a node from `(old_parent, old_name)` to `(new_parent, new_name)`.
    ///
    /// Queues a `MoveNode` op and applies optimistic local state.
    ///
    /// # Errors
    /// Returns a [`WriteError`] on collision, cycle, or backend rejection.
    fn rename(
        &self,
        old_parent: NodeId,
        old_name: &str,
        new_parent: NodeId,
        new_name: &str,
    ) -> Result<(), WriteError>;

    /// Remove a file node (unlink). Queues a `DeleteNode` op.
    ///
    /// # Errors
    /// Returns a [`WriteError`] if the node is not a file or backend rejects.
    fn unlink(&self, parent: NodeId, name: &str) -> Result<(), WriteError>;

    /// Remove a directory node (rmdir). Queues a `DeleteNode` op.
    ///
    /// # Errors
    /// Returns a [`WriteError`] if the directory is not empty or backend rejects.
    fn rmdir(&self, parent: NodeId, name: &str) -> Result<(), WriteError>;

    /// Set the POSIX mode (executable bit) for a node. Queues a metadata op.
    ///
    /// # Errors
    /// Returns a [`WriteError`] on backend failure.
    fn setattr_mode(&self, node_id: NodeId, mode: u32) -> Result<(), WriteError>;
}

/// Errors returned by write/mutation methods.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A live sibling with the same name already exists.
    #[error("path collision: {0}")]
    Collision(String),
    /// The node or parent was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// The operation is invalid (e.g. rmdir on non-empty dir).
    #[error("invalid operation: {0}")]
    Invalid(String),
    /// A backend/IO failure.
    #[error("io error: {0}")]
    Io(String),
}

/// Errors returned by [`Fs2Backend::read_file`].
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// Bytes are absent and the backend is offline / cannot hydrate.
    #[error("not hydrated and offline")]
    NotHydrated,
    /// Generic backend or verification failure.
    #[error("io error: {0}")]
    Io(String),
}

/// Build a [`FileAttr`] from node + revision metadata.
fn node_to_attr(ino: u64, node: &LocalNode, rev: Option<&LocalRevision>) -> FileAttr {
    let (kind, size, perm, symlink_target) = match node.kind {
        NodeKind::Directory => (FileType::Directory, 0, 0o755, None),
        NodeKind::File => {
            let size = rev.map_or(0, |r| r.size);
            // Preserve executable bit from POSIX mode.
            let perm = rev.map_or(0o644, |r| (r.posix_mode & 0o777) as u16);
            (FileType::RegularFile, size, perm, None)
        }
        NodeKind::Symlink => {
            let target = rev.and_then(|r| r.symlink_target.clone()).unwrap_or_default();
            // Symlink size is the target length.
            (FileType::Symlink, target.len() as u64, 0o777, Some(target))
        }
    };
    let _ = symlink_target; // target used only for size above
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: if node.kind == NodeKind::Directory { 2 } else { 1 },
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}


/// A directory entry returned by [`Fs2Filesystem::readdir_core`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Inode number.
    pub ino: u64,
    /// Entry kind.
    pub kind: FileType,
    /// Entry name.
    pub name: String,
    /// FUSE offset of the next entry.
    pub offset: i64,
}

/// The FUSE filesystem adapter.
///
/// Holds an inode <-> [`NodeId`] map (root fixed at [`ROOT_INO`]) and a
/// backend that provides metadata and content. The map is guarded by a
/// `parking_lot::Mutex` because lookups are synchronous and short.
pub struct Fs2Filesystem<B: Fs2Backend> {
    pub(crate) backend: B,
    pub(crate) inodes: Mutex<InodeMap>,
}

#[derive(Default)]
struct InodeMap {
    /// `node_id` -> `ino`
    to_ino: HashMap<NodeId, u64>,
    /// `ino` -> `node_id`
    to_node: HashMap<u64, NodeId>,
    /// next ino allocator
    next: u64,
}

impl InodeMap {
    fn new(root_node_id: NodeId) -> Self {
        let mut to_ino = HashMap::new();
        let mut to_node = HashMap::new();
        to_ino.insert(root_node_id, ROOT_INO);
        to_node.insert(ROOT_INO, root_node_id);
        Self {
            to_ino,
            to_node,
            next: ROOT_INO + 1,
        }
    }

    fn ino_of(&mut self, node_id: NodeId) -> u64 {
        if let Some(&ino) = self.to_ino.get(&node_id) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.to_ino.insert(node_id, ino);
        self.to_node.insert(ino, node_id);
        ino
    }

    fn node_of(&self, ino: u64) -> Option<NodeId> {
        self.to_node.get(&ino).copied()
    }
}

impl<B: Fs2Backend> Fs2Filesystem<B> {
    /// Create a new filesystem adapter over `backend`.
    #[must_use]
    pub fn new(backend: B) -> Self {
        let root = backend.root_node_id();
        Self {
            backend,
            inodes: Mutex::new(InodeMap::new(root)),
        }
    }

    /// Build the mount option list for a user-owned mount.
    #[must_use]
    pub fn mount_options() -> Vec<MountOption> {
        vec![
            MountOption::AutoUnmount,
            MountOption::AllowRoot,
            MountOption::FSName("fs2".to_owned()),
        ]
    }

    /// Mount the filesystem at `mountpoint` (blocking until unmounted).
    ///
    /// This is the production entry point. Live mounting cannot be verified in
    /// the sandbox (kernel FUSE requests are not delivered); unit tests
    /// exercise the [`Filesystem`] methods directly instead.
    ///
    /// # Errors
    /// Returns an error if the mount fails to establish.
    pub fn mount(self, mountpoint: &std::path::Path) -> anyhow::Result<()> {
        let options = Self::mount_options();
        debug!("mounting fs2 at {}", mountpoint.display());
        fuser::mount2(self, mountpoint, &options)?;
        Ok(())
    }

    fn resolve_ino(&self, ino: u64) -> Option<NodeId> {
        self.inodes.lock().node_of(ino)
    }

    // ---- Testable core logic (no fuser::Request needed) ----

    /// Look up a child by name under `parent_ino` and return its attr + ino.
    ///
    /// Returns `Ok(attr)` on success, `Err(errno)` on failure.
    pub fn lookup_core(&self, parent_ino: u64, name: &str) -> Result<FileAttr, i32> {
        let parent_id = self.resolve_ino(parent_ino).ok_or(libc::ENOENT)?;
        let node = self
            .backend
            .lookup(parent_id, name)
            .ok_or(libc::ENOENT)?;
        if node.deleted {
            return Err(libc::ENOENT);
        }
        let ino = self.inodes.lock().ino_of(node.node_id);
        let rev = self.backend.get_revision(node.node_id);
        Ok(node_to_attr(ino, &node, rev.as_ref()))
    }

    /// Get attributes for an inode.
    pub fn getattr_core(&self, ino: u64) -> Result<FileAttr, i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        let node = self.backend.get_node(node_id).ok_or(libc::ENOENT)?;
        if node.deleted {
            return Err(libc::ENOENT);
        }
        let rev = self.backend.get_revision(node_id);
        let stable_ino = self.inodes.lock().ino_of(node_id);
        Ok(node_to_attr(stable_ino, &node, rev.as_ref()))
    }

    /// Read a symlink target.
    pub fn readlink_core(&self, ino: u64) -> Result<String, i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        self.backend.readlink(node_id).ok_or(libc::EINVAL)
    }

    /// Validate that an inode is openable as a file. Returns Ok(()) or errno.
    pub fn open_core(&self, ino: u64) -> Result<(), i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        let node = self.backend.get_node(node_id).ok_or(libc::ENOENT)?;
        if node.deleted {
            return Err(libc::ENOENT);
        }
        if node.kind != NodeKind::File {
            return Err(libc::EISDIR);
        }
        Ok(())
    }

    /// Read `size` bytes from `offset` of the file at `ino`.
    pub fn read_core(&self, ino: u64, offset: i64, size: u32) -> Result<Vec<u8>, i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        let bytes = self.backend.read_file(node_id).map_err(|e| {
            warn!("read: backend error for node {node_id}: {e}");
            libc::EIO
        })?;
        let off = usize::try_from(offset).unwrap_or(0);
        if off >= bytes.len() {
            return Ok(Vec::new());
        }
        let end = (off + usize::try_from(size).unwrap_or(bytes.len())).min(bytes.len());
        Ok(bytes[off..end].to_vec())
    }

    /// Validate that an inode is openable as a directory.
    pub fn opendir_core(&self, ino: u64) -> Result<(), i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        let node = self.backend.get_node(node_id).ok_or(libc::ENOENT)?;
        if node.deleted {
            return Err(libc::ENOENT);
        }
        if node.kind != NodeKind::Directory {
            return Err(libc::ENOTDIR);
        }
        Ok(())
    }

    /// List directory entries (including `.` and `..`) starting at `offset`.
    ///
    /// Does not hydrate file content; only metadata is read.
    pub fn readdir_core(&self, ino: u64, offset: i64) -> Result<Vec<DirEntry>, i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        let node = self.backend.get_node(node_id).ok_or(libc::ENOENT)?;
        if node.deleted {
            return Err(libc::ENOENT);
        }
        if node.kind != NodeKind::Directory {
            return Err(libc::ENOTDIR);
        }
        let mut entries = Vec::new();
        let mut idx: i64 = 0;

        // "." entry
        idx += 1;
        if offset < idx {
            let dot_ino = self.inodes.lock().ino_of(node_id);
            entries.push(DirEntry {
                ino: dot_ino,
                kind: FileType::Directory,
                name: ".".to_owned(),
                offset: idx,
            });
        }
        // ".." entry
        idx += 1;
        if offset < idx {
            let parent_ino = node
                .parent_id
                .map_or(ROOT_INO, |p| self.inodes.lock().ino_of(p));
            entries.push(DirEntry {
                ino: parent_ino,
                kind: FileType::Directory,
                name: "..".to_owned(),
                offset: idx,
            });
        }
        let children = self.backend.list_children(node_id);
        for child in children {
            if child.deleted {
                continue;
            }
            idx += 1;
            if offset >= idx {
                continue;
            }
            let child_ino = self.inodes.lock().ino_of(child.node_id);
            let kind = match child.kind {
                NodeKind::Directory => FileType::Directory,
                NodeKind::File => FileType::RegularFile,
                NodeKind::Symlink => FileType::Symlink,
            };
            entries.push(DirEntry {
                ino: child_ino,
                kind,
                name: child.name,
                offset: idx,
            });
        }
        Ok(entries)
    }

    // ---- Write-path core methods (design §6.4-6.6, §13) ----

    /// Create a directory. Returns the new inode.
    pub fn mkdir_core(&self, parent_ino: u64, name: &str) -> Result<u64, i32> {
        let parent_id = self.resolve_ino(parent_ino).ok_or(libc::ENOENT)?;
        let node_id = self.backend.mkdir(parent_id, name).map_err(|e| match e {
            WriteError::Collision(_) => libc::EEXIST,
            WriteError::NotFound(_) => libc::ENOENT,
            WriteError::Invalid(_) => libc::EINVAL,
            WriteError::Io(_) => libc::EIO,
        })?;
        Ok(self.inodes.lock().ino_of(node_id))
    }

    /// Create a file. Returns the new inode and write handle.
    pub fn create_core(&self, parent_ino: u64, name: &str, mode: u32) -> Result<(u64, u64), i32> {
        let parent_id = self.resolve_ino(parent_ino).ok_or(libc::ENOENT)?;
        let (node_id, fh) = self.backend.create_file(parent_id, name, mode).map_err(|e| match e {
            WriteError::Collision(_) => libc::EEXIST,
            WriteError::NotFound(_) => libc::ENOENT,
            WriteError::Invalid(_) => libc::EINVAL,
            WriteError::Io(_) => libc::EIO,
        })?;
        let ino = self.inodes.lock().ino_of(node_id);
        Ok((ino, fh))
    }

    /// Write data to a write handle.
    pub fn write_core(&self, fh: u64, offset: i64, data: &[u8]) -> Result<u32, i32> {
        self.backend.write(fh, offset, data).map_err(|e| match e {
            WriteError::NotFound(_) => libc::EBADF,
            WriteError::Io(_) | WriteError::Collision(_) | WriteError::Invalid(_) => libc::EIO,
        })?;
        Ok(data.len() as u32)
    }

    /// Flush a write handle (commit the revision).
    pub fn flush_core(&self, fh: u64) -> Result<(), i32> {
        self.backend.flush(fh).map_err(|_| libc::EIO)
    }

    /// Release a write handle.
    pub fn release_core(&self, fh: u64) {
        self.backend.release(fh);
    }

    /// Rename/move a node.
    pub fn rename_core(
        &self,
        old_parent_ino: u64,
        old_name: &str,
        new_parent_ino: u64,
        new_name: &str,
    ) -> Result<(), i32> {
        let old_parent = self.resolve_ino(old_parent_ino).ok_or(libc::ENOENT)?;
        let new_parent = self.resolve_ino(new_parent_ino).ok_or(libc::ENOENT)?;
        self.backend.rename(old_parent, old_name, new_parent, new_name).map_err(|e| match e {
            WriteError::Collision(_) => libc::EEXIST,
            WriteError::NotFound(_) => libc::ENOENT,
            WriteError::Invalid(_) => libc::EINVAL,
            WriteError::Io(_) => libc::EIO,
        })
    }

    /// Unlink a file.
    pub fn unlink_core(&self, parent_ino: u64, name: &str) -> Result<(), i32> {
        let parent = self.resolve_ino(parent_ino).ok_or(libc::ENOENT)?;
        self.backend.unlink(parent, name).map_err(|e| match e {
            WriteError::NotFound(_) => libc::ENOENT,
            WriteError::Invalid(_) => libc::EISDIR,
            WriteError::Io(_) | WriteError::Collision(_) => libc::EIO,
        })
    }

    /// Remove a directory.
    pub fn rmdir_core(&self, parent_ino: u64, name: &str) -> Result<(), i32> {
        let parent = self.resolve_ino(parent_ino).ok_or(libc::ENOENT)?;
        self.backend.rmdir(parent, name).map_err(|e| match e {
            WriteError::NotFound(_) => libc::ENOENT,
            WriteError::Invalid(_) => libc::ENOTEMPTY,
            WriteError::Io(_) | WriteError::Collision(_) => libc::EIO,
        })
    }

    /// Set the mode (executable bit) for a node.
    pub fn setattr_mode_core(&self, ino: u64, mode: u32) -> Result<FileAttr, i32> {
        let node_id = self.resolve_ino(ino).ok_or(libc::ENOENT)?;
        self.backend.setattr_mode(node_id, mode).map_err(|e| match e {
            WriteError::NotFound(_) => libc::ENOENT,
            WriteError::Io(_) | WriteError::Collision(_) | WriteError::Invalid(_) => libc::EIO,
        })?;
        // Return the updated attr.
        self.getattr_core(ino)
    }
}

impl<B: Fs2Backend> Filesystem for Fs2Filesystem<B> {
    fn lookup(&mut self, _req: &fuser::Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_str().unwrap_or("");
        match self.lookup_core(parent, name_str) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&mut self, _req: &fuser::Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.getattr_core(ino) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn readlink(&mut self, _req: &fuser::Request<'_>, ino: u64, reply: ReplyData) {
        match self.readlink_core(ino) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(errno) => reply.error(errno),
        }
    }

    fn open(&mut self, _req: &fuser::Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        match self.open_core(ino) {
            Ok(()) => reply.opened(0, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.read_core(ino, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(errno) => reply.error(errno),
        }
    }

    fn opendir(&mut self, _req: &fuser::Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        match self.opendir_core(ino) {
            Ok(()) => reply.opened(0, 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        match self.readdir_core(ino, offset) {
            Ok(entries) => {
                for e in &entries {
                    if !reply.add(e.ino, e.offset, e.kind, &e.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn mknod(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: fuser::ReplyEntry,
    ) {
        let name_str = name.to_str().unwrap_or("");
        // Only regular files and directories are supported via mknod.
        if mode & libc::S_IFMT == libc::S_IFDIR {
            match self.mkdir_core(parent, name_str) {
                Ok(ino) => {
                    let attr = self.getattr_core(ino).unwrap_or_else(|_| root_attr_fallback(ino));
                    reply.entry(&TTL, &attr, 0);
                }
                Err(errno) => reply.error(errno),
            }
        } else {
            match self.create_core(parent, name_str, mode) {
                Ok((ino, _fh)) => {
                    let attr = self.getattr_core(ino).unwrap_or_else(|_| root_attr_fallback(ino));
                    reply.entry(&TTL, &attr, 0);
                }
                Err(errno) => reply.error(errno),
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let name_str = name.to_str().unwrap_or("");
        match self.mkdir_core(parent, name_str) {
            Ok(ino) => {
                let attr = self.getattr_core(ino).unwrap_or_else(|_| root_attr_fallback(ino));
                reply.entry(&TTL, &attr, 0);
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn create(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let name_str = name.to_str().unwrap_or("");
        match self.create_core(parent, name_str, mode) {
            Ok((ino, fh)) => {
                let attr = self.getattr_core(ino).unwrap_or_else(|_| root_attr_fallback(ino));
                reply.created(&TTL, &attr, 0, fh, 0);
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn write(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        match self.write_core(fh, offset, data) {
            Ok(n) => reply.written(n),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        match self.flush_core(fh) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(
        &mut self,
        _req: &fuser::Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.release_core(fh);
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &fuser::Request<'_>,
        old_parent: u64,
        old_name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let old_name_str = old_name.to_str().unwrap_or("");
        let new_name_str = new_name.to_str().unwrap_or("");
        match self.rename_core(old_parent, old_name_str, new_parent, new_name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn unlink(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let name_str = name.to_str().unwrap_or("");
        match self.unlink_core(parent, name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rmdir(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEmpty,
    ) {
        let name_str = name.to_str().unwrap_or("");
        match self.rmdir_core(parent, name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn setattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(mode) = mode {
            match self.setattr_mode_core(ino, mode) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(errno) => reply.error(errno),
            }
        } else {
            match self.getattr_core(ino) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(errno) => reply.error(errno),
            }
        }
    }
}

/// Fallback attr for a freshly created node before metadata is available.
fn root_attr_fallback(ino: u64) -> FileAttr {
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// Compute the local blob cache path for a blob id (mirrors the CLI layout).
fn blob_cache_path(blob_id: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let sanitized = blob_id.replace(':', "/");
    format!("{home}/.fs2/blobs/{sanitized}")
}

/// A [`Fs2Backend`] backed by a [`LocalStore`] plus a [`BlobReader`] for
/// content hydration.
///
/// `BlobReader::read` is called when file bytes are needed; the daemon
/// implements it to fetch from the blob store / cache. This keeps the FUSE
/// adapter decoupled from the blob store crate.
pub struct LocalStoreBackend<R: BlobReader = NoBlobReader> {
    store: LocalStore,
    blob_reader: R,
    workspace_id: WorkspaceId,
    root_node_id: NodeId,
    /// Write handles: `fh` -> (`node_id`, staging bytes, next `fh` allocator).
    write_handles: parking_lot::Mutex<WriteHandleState>,
}

/// Internal write-handle state.
struct WriteHandleState {
    /// `fh` -> (`node_id`, staging bytes).
    handles: HashMap<u64, (NodeId, Vec<u8>)>,
    /// Next fh allocator.
    next: u64,
}

impl Default for WriteHandleState {
    fn default() -> Self {
        Self {
            handles: HashMap::new(),
            next: 1,
        }
    }
}

/// Reads file bytes for a node by blob id (hydration).
///
/// Implementations download the blob from the object store / cache, verify
/// its hash, and return the plaintext bytes.
pub trait BlobReader: Send + Sync {
    /// Read the bytes for `blob_id`.
    ///
    /// # Errors
    /// Returns [`ReadError::NotHydrated`] if the blob is absent and cannot be
    /// fetched (offline), or [`ReadError::Io`] on failure.
    fn read(&self, blob_id: &str) -> Result<Vec<u8>, ReadError>;
}

/// A [`BlobReader`] that never hydrates (always offline). Used when file
/// content is not yet cached.
pub struct NoBlobReader;

impl BlobReader for NoBlobReader {
    fn read(&self, _blob_id: &str) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::NotHydrated)
    }
}

impl<R: BlobReader> LocalStoreBackend<R> {
    /// Create a backend over `store` with `blob_reader` for content.
    ///
    /// `root_node_id` is the workspace root node id.
    #[must_use]
    pub fn new(store: LocalStore, blob_reader: R, workspace_id: WorkspaceId, root_node_id: NodeId) -> Self {
        Self {
            store,
            blob_reader,
            workspace_id,
            root_node_id,
            write_handles: parking_lot::Mutex::new(WriteHandleState::default()),
        }
    }
}

impl<R: BlobReader> Fs2Backend for LocalStoreBackend<R> {
    fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    fn root_node_id(&self) -> NodeId {
        self.root_node_id
    }

    fn lookup(&self, parent: NodeId, name: &str) -> Option<LocalNode> {
        // Resolve parent path then look up child by name under it.
        let parent_node = self.store.get_node_by_id(parent).ok().flatten()?;
        if parent_node.deleted || parent_node.kind != NodeKind::Directory {
            return None;
        }
        let parent_path = self.store.node_path(parent).ok().flatten()?;
        let child_path = if parent_path.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_path}/{name}")
        };
        self.store
            .get_node_by_path(self.workspace_id, &child_path)
            .ok()
            .flatten()
            .filter(|n| !n.deleted)
    }

    fn get_node(&self, node_id: NodeId) -> Option<LocalNode> {
        self.store.get_node_by_id(node_id).ok().flatten()
    }

    fn get_revision(&self, node_id: NodeId) -> Option<LocalRevision> {
        let node = self.get_node(node_id)?;
        let rev_id = node.current_revision_id?;
        self.store.get_revision(rev_id).ok().flatten()
    }

    fn list_children(&self, parent: NodeId) -> Vec<LocalNode> {
        self.store
            .list_children(self.workspace_id, Some(parent))
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !n.deleted)
            .collect()
    }
    fn read_file(&self, node_id: NodeId) -> Result<Vec<u8>, ReadError> {
        let rev = self.get_revision(node_id).ok_or_else(|| ReadError::Io("no revision".to_owned()))?;
        let blob_id = rev.blob_id.ok_or_else(|| ReadError::Io("no blob id".to_owned()))?;
        let bytes = self.blob_reader.read(&blob_id)?;
        // Record access timestamp in SQLite (design §7.3); ignore failure.
        let _ = self.store.touch_access(node_id);
        Ok(bytes)
    }

    fn readlink(&self, node_id: NodeId) -> Option<String> {
        self.get_revision(node_id).and_then(|r| r.symlink_target)
    }

    fn mkdir(&self, parent: NodeId, name: &str) -> Result<NodeId, WriteError> {
        use fs2_core::{Cursor, DeviceId, NodeKind, Operation, OperationKind};
        // Check for collision.
        if self.lookup(parent, name).is_some() {
            return Err(WriteError::Collision(name.to_owned()));
        }
        let device = DeviceId::new();
        let op = Operation::new(
            self.workspace_id,
            device,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: parent,
                name: name.to_owned(),
                kind: NodeKind::Directory,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        // Apply locally (optimistic) and queue as pending op.
        self.store
            .apply_operation(&op, Cursor::zero())
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .put_pending_op(&op)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        // Look up the newly-created node to return its id.
        let parent_path = self
            .store
            .node_path(parent)
            .map_err(|e| WriteError::Io(e.to_string()))?
            .unwrap_or_default();
        let child_path = if parent_path.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_path}/{name}")
        };
        let node = self
            .store
            .get_node_by_path(self.workspace_id, &child_path)
            .map_err(|e| WriteError::Io(e.to_string()))?
            .ok_or_else(|| WriteError::Io("created node not found".to_owned()))?;
        Ok(node.node_id)
    }

    fn create_file(&self, parent: NodeId, name: &str, _mode: u32) -> Result<(NodeId, u64), WriteError> {
        use fs2_core::{Cursor, DeviceId, NodeKind, Operation, OperationKind};
        if self.lookup(parent, name).is_some() {
            return Err(WriteError::Collision(name.to_owned()));
        }
        let device = DeviceId::new();
        // Create the node with no initial revision (empty file).
        let op = Operation::new(
            self.workspace_id,
            device,
            Cursor::zero(),
            OperationKind::CreateNode {
                parent_id: parent,
                name: name.to_owned(),
                kind: NodeKind::File,
                initial_revision: None,
            },
            chrono::Utc::now(),
        );
        self.store
            .apply_operation(&op, Cursor::zero())
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .put_pending_op(&op)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        let parent_path = self
            .store
            .node_path(parent)
            .map_err(|e| WriteError::Io(e.to_string()))?
            .unwrap_or_default();
        let child_path = if parent_path.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_path}/{name}")
        };
        let node = self
            .store
            .get_node_by_path(self.workspace_id, &child_path)
            .map_err(|e| WriteError::Io(e.to_string()))?
            .ok_or_else(|| WriteError::Io("created node not found".to_owned()))?;
        // Allocate a write handle with empty staging bytes.
        let mut state = self.write_handles.lock();
        let fh = state.next;
        state.next += 1;
        state.handles.insert(fh, (node.node_id, Vec::new()));
        Ok((node.node_id, fh))
    }

    fn write(&self, fh: u64, offset: i64, data: &[u8]) -> Result<(), WriteError> {
        let mut state = self.write_handles.lock();
        let entry = state
            .handles
            .get_mut(&fh)
            .ok_or_else(|| WriteError::NotFound("write handle".to_owned()))?;
        let staging = &mut entry.1;
        let off = usize::try_from(offset).unwrap_or(0);
        // Extend staging if writing past current end.
        if off + data.len() > staging.len() {
            staging.resize(off + data.len(), 0);
        }
        staging[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&self, fh: u64) -> Result<(), WriteError> {
        use fs2_core::{Cursor, DeviceId, NodeRevision, Operation, OperationKind, RevisionContent, RevisionId};
        let (node_id, bytes) = {
            let mut state = self.write_handles.lock();
            state
                .handles
                .remove(&fh)
                .ok_or_else(|| WriteError::NotFound("write handle".to_owned()))?
        };
        // Compute blob id from the staging bytes (ciphertext hash; here we
        // use plaintext hash for dev simplicity).
        let blob_id = fs2_crypto::compute_blob_id(&bytes);
        // Cache the blob locally.
        let cache_path = blob_cache_path(&blob_id);
        if let Some(parent) = std::path::Path::new(&cache_path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&cache_path, &bytes)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .mark_blob_cached(&blob_id, &cache_path, bytes.len() as u64)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        // Mark local state dirty until backend ack.
        self.store
            .set_hydration_state(node_id, "dirtylocal")
            .map_err(|e| WriteError::Io(e.to_string()))?;
        // Queue a PutFileRevision op.
        let device = DeviceId::new();
        let rev_id = RevisionId::new();
        let rev = NodeRevision {
            revision_id: rev_id,
            node_id,
            workspace_id: self.workspace_id,
            device_id: device,
            base_revision_id: None,
            content: RevisionContent::File {
                blob_id: blob_id.clone(),
                chunk_ids: vec![],
                content_hash: blob_id,
                encryption_header: None,
            },
            posix_mode: 0o644,
            mtime: chrono::Utc::now(),
            size: bytes.len() as u64,
            executable: false,
            created_at: chrono::Utc::now(),
        };
        let op = Operation::new(
            self.workspace_id,
            device,
            Cursor::zero(),
            OperationKind::PutFileRevision {
                node_id,
                base_revision_id: None,
                revision: rev,
            },
            chrono::Utc::now(),
        );
        // Apply the revision locally so the node's current_revision_id is
        // updated and the file can be read back immediately.
        self.store
            .apply_operation(&op, Cursor::zero())
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .put_pending_op(&op)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        // Mark hydrated (bytes are now cached locally).
        self.store
            .set_hydration_state(node_id, "hydrated")
            .map_err(|e| WriteError::Io(e.to_string()))?;
        Ok(())
    }

    fn release(&self, fh: u64) {
        // Drop the staging state; if not flushed, the bytes are lost (the
        // daemon would persist staging to disk in production).
        let _ = self.write_handles.lock().handles.remove(&fh);
    }

    fn rename(
        &self,
        old_parent: NodeId,
        old_name: &str,
        new_parent: NodeId,
        new_name: &str,
    ) -> Result<(), WriteError> {
        use fs2_core::{Cursor, DeviceId, Operation, OperationKind};
        let node = self
            .lookup(old_parent, old_name)
            .ok_or_else(|| WriteError::NotFound(old_name.to_owned()))?;
        // FUSE rename replaces an existing target (atomic save semantics).
        // Delete the existing target node first, if any.
        if let Some(target) = self.lookup(new_parent, new_name) {
            let del_device = DeviceId::new();
            let del_op = Operation::new(
                self.workspace_id,
                del_device,
                Cursor::zero(),
                OperationKind::DeleteNode {
                    node_id: target.node_id,
                    recursive: false,
                },
                chrono::Utc::now(),
            );
            self.store
                .apply_operation(&del_op, Cursor::zero())
                .map_err(|e| WriteError::Io(e.to_string()))?;
            self.store
                .put_pending_op(&del_op)
                .map_err(|e| WriteError::Io(e.to_string()))?;
        }
        let device = DeviceId::new();
        let op = Operation::new(
            self.workspace_id,
            device,
            Cursor::zero(),
            OperationKind::MoveNode {
                node_id: node.node_id,
                old_parent_id: old_parent,
                old_name: old_name.to_owned(),
                new_parent_id: new_parent,
                new_name: new_name.to_owned(),
            },
            chrono::Utc::now(),
        );
        self.store
            .apply_operation(&op, Cursor::zero())
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .put_pending_op(&op)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        Ok(())
    }

    fn unlink(&self, parent: NodeId, name: &str) -> Result<(), WriteError> {
        use fs2_core::{Cursor, DeviceId, Operation, OperationKind};
        let node = self
            .lookup(parent, name)
            .ok_or_else(|| WriteError::NotFound(name.to_owned()))?;
        if node.kind != NodeKind::File {
            return Err(WriteError::Invalid("unlink on non-file".to_owned()));
        }
        let device = DeviceId::new();
        let op = Operation::new(
            self.workspace_id,
            device,
            Cursor::zero(),
            OperationKind::DeleteNode {
                node_id: node.node_id,
                recursive: false,
            },
            chrono::Utc::now(),
        );
        self.store
            .apply_operation(&op, Cursor::zero())
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .put_pending_op(&op)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        Ok(())
    }

    fn rmdir(&self, parent: NodeId, name: &str) -> Result<(), WriteError> {
        use fs2_core::{Cursor, DeviceId, Operation, OperationKind};
        let node = self
            .lookup(parent, name)
            .ok_or_else(|| WriteError::NotFound(name.to_owned()))?;
        if node.kind != NodeKind::Directory {
            return Err(WriteError::Invalid("rmdir on non-directory".to_owned()));
        }
        // Check empty.
        let children = self
            .store
            .list_children(self.workspace_id, Some(node.node_id))
            .map_err(|e| WriteError::Io(e.to_string()))?;
        if !children.is_empty() {
            return Err(WriteError::Invalid("directory not empty".to_owned()));
        }
        let device = DeviceId::new();
        let op = Operation::new(
            self.workspace_id,
            device,
            Cursor::zero(),
            OperationKind::DeleteNode {
                node_id: node.node_id,
                recursive: false,
            },
            chrono::Utc::now(),
        );
        self.store
            .apply_operation(&op, Cursor::zero())
            .map_err(|e| WriteError::Io(e.to_string()))?;
        self.store
            .put_pending_op(&op)
            .map_err(|e| WriteError::Io(e.to_string()))?;
        Ok(())
    }

    fn setattr_mode(&self, node_id: NodeId, mode: u32) -> Result<(), WriteError> {
        // The local store doesn't have a direct setattr op; in production this
        // would queue a metadata revision. For now, record the mode in the
        // revision (if any) is a future step. We mark it as a pending op
        // placeholder by recording the intent.
        // TODO: queue a metadata revision op when the op model supports it.
        let _ = (node_id, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
