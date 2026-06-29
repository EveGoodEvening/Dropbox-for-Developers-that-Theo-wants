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
use std::time::{Duration, UNIX_EPOCH};

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
}

#[cfg(test)]
mod tests;
