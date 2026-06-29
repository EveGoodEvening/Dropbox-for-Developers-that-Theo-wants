//! Unit tests for the FUSE adapter using a mock [`Fs2Backend`].
//!
//! These tests exercise the read-only filesystem logic (lookup, getattr,
//! readdir, read, readlink, open/opendir) without a live FUSE mount. The
//! sandbox cannot deliver kernel FUSE requests, so live-mount acceptance is
//! recorded as blocked in `plan/todo.md`.

use std::collections::HashMap;

use fs2_core::{NodeId, NodeKind, RevisionId, WorkspaceId};
use fs2_sync::{LocalNode, LocalRevision};
use parking_lot::Mutex;

use super::{
    BlobReader, Fs2Backend, Fs2Filesystem, LocalStoreBackend, NoBlobReader, ReadError, ROOT_INO,
};
use fs2_core::{Cursor, DeviceId, NodeRevision, Operation, OperationKind, RevisionContent};

/// A mock backend holding nodes, revisions, and file bytes in memory.
struct MockBackend {
    workspace_id: WorkspaceId,
    root_node_id: NodeId,
    nodes: Mutex<HashMap<NodeId, LocalNode>>,
    revisions: Mutex<HashMap<NodeId, LocalRevision>>,
    bytes: Mutex<HashMap<NodeId, Vec<u8>>>,
    symlinks: Mutex<HashMap<NodeId, String>>,
}

impl MockBackend {
    fn new(root: NodeId) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            root,
            LocalNode {
                node_id: root,
                workspace_id: WorkspaceId::new(),
                parent_id: None,
                name: String::new(),
                kind: NodeKind::Directory,
                current_revision_id: None,
                deleted: false,
            },
        );
        Self {
            workspace_id: WorkspaceId::new(),
            root_node_id: root,
            nodes: Mutex::new(nodes),
            revisions: Mutex::new(HashMap::new()),
            bytes: Mutex::new(HashMap::new()),
            symlinks: Mutex::new(HashMap::new()),
        }
    }

    fn add_dir(&self, parent: NodeId, name: &str) -> NodeId {
        let id = NodeId::new();
        self.nodes.lock().insert(
            id,
            LocalNode {
                node_id: id,
                workspace_id: self.workspace_id,
                parent_id: Some(parent),
                name: name.to_owned(),
                kind: NodeKind::Directory,
                current_revision_id: None,
                deleted: false,
            },
        );
        id
    }

    fn add_file(&self, parent: NodeId, name: &str, content: &[u8], exec: bool) -> NodeId {
        let id = NodeId::new();
        let rev_id = RevisionId::new();
        self.nodes.lock().insert(
            id,
            LocalNode {
                node_id: id,
                workspace_id: self.workspace_id,
                parent_id: Some(parent),
                name: name.to_owned(),
                kind: NodeKind::File,
                current_revision_id: Some(rev_id),
                deleted: false,
            },
        );
        self.revisions.lock().insert(
            id,
            LocalRevision {
                revision_id: rev_id,
                node_id: id,
                blob_id: Some(format!("sha256:{id}")),
                chunk_ids: None,
                symlink_target: None,
                size: content.len() as u64,
                content_hash: None,
                encryption_header: None,
                posix_mode: if exec { 0o755 } else { 0o644 },
                mtime: "2026-06-29T00:00:00Z".to_owned(),
                created_at: "2026-06-29T00:00:00Z".to_owned(),
            },
        );
        self.bytes.lock().insert(id, content.to_vec());
        id
    }

    fn add_symlink(&self, parent: NodeId, name: &str, target: &str) -> NodeId {
        let id = NodeId::new();
        let rev_id = RevisionId::new();
        self.nodes.lock().insert(
            id,
            LocalNode {
                node_id: id,
                workspace_id: self.workspace_id,
                parent_id: Some(parent),
                name: name.to_owned(),
                kind: NodeKind::Symlink,
                current_revision_id: Some(rev_id),
                deleted: false,
            },
        );
        self.revisions.lock().insert(
            id,
            LocalRevision {
                revision_id: rev_id,
                node_id: id,
                blob_id: None,
                chunk_ids: None,
                symlink_target: Some(target.to_owned()),
                size: target.len() as u64,
                content_hash: None,
                encryption_header: None,
                posix_mode: 0o777,
                mtime: "2026-06-29T00:00:00Z".to_owned(),
                created_at: "2026-06-29T00:00:00Z".to_owned(),
            },
        );
        self.symlinks.lock().insert(id, target.to_owned());
        id
    }
}

impl Fs2Backend for MockBackend {
    fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    fn root_node_id(&self) -> NodeId {
        self.root_node_id
    }

    fn lookup(&self, parent: NodeId, name: &str) -> Option<LocalNode> {
        self.nodes
            .lock()
            .values()
            .find(|n| n.parent_id == Some(parent) && n.name == name && !n.deleted)
            .cloned()
    }

    fn get_node(&self, node_id: NodeId) -> Option<LocalNode> {
        self.nodes.lock().get(&node_id).cloned()
    }

    fn get_revision(&self, node_id: NodeId) -> Option<LocalRevision> {
        self.revisions.lock().get(&node_id).cloned()
    }

    fn list_children(&self, parent: NodeId) -> Vec<LocalNode> {
        self.nodes
            .lock()
            .values()
            .filter(|n| n.parent_id == Some(parent) && !n.deleted)
            .cloned()
            .collect()
    }

    fn read_file(&self, node_id: NodeId) -> Result<Vec<u8>, ReadError> {
        self.bytes
            .lock()
            .get(&node_id)
            .cloned()
            .ok_or(ReadError::NotHydrated)
    }

    fn readlink(&self, node_id: NodeId) -> Option<String> {
        self.symlinks.lock().get(&node_id).cloned()
    }
}

/// A mock backend whose `read_file` always fails (simulates offline/not-hydrated).
struct OfflineBackend(MockBackend);

impl Fs2Backend for OfflineBackend {
    fn workspace_id(&self) -> WorkspaceId {
        self.0.workspace_id()
    }
    fn root_node_id(&self) -> NodeId {
        self.0.root_node_id()
    }
    fn lookup(&self, parent: NodeId, name: &str) -> Option<LocalNode> {
        self.0.lookup(parent, name)
    }
    fn get_node(&self, node_id: NodeId) -> Option<LocalNode> {
        self.0.get_node(node_id)
    }
    fn get_revision(&self, node_id: NodeId) -> Option<LocalRevision> {
        self.0.get_revision(node_id)
    }
    fn list_children(&self, parent: NodeId) -> Vec<LocalNode> {
        self.0.list_children(parent)
    }
    fn read_file(&self, _node_id: NodeId) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::NotHydrated)
    }
    fn readlink(&self, node_id: NodeId) -> Option<String> {
        self.0.readlink(node_id)
    }
}

fn make_fs() -> (Fs2Filesystem<MockBackend>, NodeId) {
    let root = NodeId::new();
    let backend = MockBackend::new(root);
    let fs = Fs2Filesystem::new(backend);
    (fs, root)
}

#[test]
fn root_getattr() {
    let (fs, _root) = make_fs();
    let attr = fs.getattr_core(ROOT_INO).expect("root attr");
    assert_eq!(attr.kind, fuser::FileType::Directory);
    assert_eq!(attr.perm, 0o755);
    assert_eq!(attr.nlink, 2);
}

#[test]
fn root_getattr_unknown_ino_is_enoent() {
    let (fs, _root) = make_fs();
    assert_eq!(fs.getattr_core(9999).unwrap_err(), libc::ENOENT);
}

#[test]
fn root_readdir_lists_dot_dotdot_and_children() {
    let (fs, root) = make_fs();
    fs.backend.add_dir(root, "apps");
    fs.backend.add_dir(root, "docs");
    let entries = fs.readdir_core(ROOT_INO, 0).expect("readdir");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"."));
    assert!(names.contains(&".."));
    assert!(names.contains(&"apps"));
    assert!(names.contains(&"docs"));
}

#[test]
fn readdir_does_not_hydrate_file_content() {
    // readdir must succeed even when file bytes are absent (offline backend).
    let root = NodeId::new();
    let backend = MockBackend::new(root);
    backend.add_file(root, "a.txt", b"data", false);
    let offline = OfflineBackend(backend);
    let fs = Fs2Filesystem::new(offline);
    let entries = fs.readdir_core(ROOT_INO, 0).expect("readdir");
    assert!(entries.iter().any(|e| e.name == "a.txt"));
}

#[test]
fn readdir_on_file_is_enotdir() {
    let (fs, root) = make_fs();
    let file_id = fs.backend.add_file(root, "f.txt", b"hi", false);
    let ino = fs.inodes.lock().ino_of(file_id);
    assert_eq!(fs.readdir_core(ino, 0).unwrap_err(), libc::ENOTDIR);
}

#[test]
fn lookup_finds_child_and_assigns_stable_ino() {
    let (fs, root) = make_fs();
    fs.backend.add_dir(root, "apps");
    let attr = fs.lookup_core(ROOT_INO, "apps").expect("lookup apps");
    assert_eq!(attr.kind, fuser::FileType::Directory);
    // Looking up again returns the same ino.
    let attr2 = fs.lookup_core(ROOT_INO, "apps").expect("lookup apps 2");
    assert_eq!(attr.ino, attr2.ino);
}

#[test]
fn lookup_missing_is_enoent() {
    let (fs, _root) = make_fs();
    assert_eq!(fs.lookup_core(ROOT_INO, "nope").unwrap_err(), libc::ENOENT);
}

#[test]
fn getattr_for_file_has_size_and_perm() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "a.txt", b"hello world", false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    assert_eq!(attr.kind, fuser::FileType::RegularFile);
    assert_eq!(attr.size, "hello world".len() as u64);
    assert_eq!(attr.perm, 0o644);
}

#[test]
fn getattr_preserves_executable_bit() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "run.sh", b"#!/bin/sh\n", true);
    let attr = fs.lookup_core(ROOT_INO, "run.sh").expect("lookup");
    assert_eq!(attr.perm, 0o755);
}

#[test]
fn read_returns_bytes_within_range() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "a.txt", b"hello world", false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    let bytes = fs.read_core(attr.ino, 0, 5).expect("read");
    assert_eq!(bytes, b"hello");
    let bytes2 = fs.read_core(attr.ino, 6, 5).expect("read 2");
    assert_eq!(bytes2, b"world");
}

#[test]
fn read_past_eof_returns_empty() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "a.txt", b"hi", false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    let bytes = fs.read_core(attr.ino, 10, 5).expect("read");
    assert!(bytes.is_empty());
}

#[test]
fn read_full_file() {
    let (fs, root) = make_fs();
    let content = b"the quick brown fox";
    let _id = fs.backend.add_file(root, "a.txt", content, false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    let bytes = fs.read_core(attr.ino, 0, 100).expect("read");
    assert_eq!(bytes, content);
}

#[test]
fn read_when_offline_returns_eio() {
    let root = NodeId::new();
    let backend = MockBackend::new(root);
    backend.add_file(root, "a.txt", b"hi", false);
    let offline = OfflineBackend(backend);
    let fs = Fs2Filesystem::new(offline);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    assert_eq!(fs.read_core(attr.ino, 0, 5).unwrap_err(), libc::EIO);
}

#[test]
fn open_file_succeeds() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "a.txt", b"hi", false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    fs.open_core(attr.ino).expect("open file");
}

#[test]
fn open_directory_is_eisdir() {
    let (fs, root) = make_fs();
    let dir_id = fs.backend.add_dir(root, "sub");
    let ino = fs.inodes.lock().ino_of(dir_id);
    assert_eq!(fs.open_core(ino).unwrap_err(), libc::EISDIR);
}

#[test]
fn opendir_directory_succeeds() {
    let (fs, root) = make_fs();
    let dir_id = fs.backend.add_dir(root, "sub");
    let ino = fs.inodes.lock().ino_of(dir_id);
    fs.opendir_core(ino).expect("opendir");
}

#[test]
fn opendir_file_is_enotdir() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "a.txt", b"hi", false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    assert_eq!(fs.opendir_core(attr.ino).unwrap_err(), libc::ENOTDIR);
}

#[test]
fn readlink_returns_target() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_symlink(root, "link", "../other/file.txt");
    let attr = fs.lookup_core(ROOT_INO, "link").expect("lookup");
    assert_eq!(attr.kind, fuser::FileType::Symlink);
    assert_eq!(attr.size, "../other/file.txt".len() as u64);
    let target = fs.readlink_core(attr.ino).expect("readlink");
    assert_eq!(target, "../other/file.txt");
}

#[test]
fn readlink_on_non_symlink_is_einval() {
    let (fs, root) = make_fs();
    let _id = fs.backend.add_file(root, "a.txt", b"hi", false);
    let attr = fs.lookup_core(ROOT_INO, "a.txt").expect("lookup");
    assert_eq!(fs.readlink_core(attr.ino).unwrap_err(), libc::EINVAL);
}

#[test]
fn readdir_pagination_with_offset() {
    let (fs, root) = make_fs();
    fs.backend.add_dir(root, "a");
    fs.backend.add_dir(root, "b");
    fs.backend.add_dir(root, "c");
    // First page from offset 0.
    let page0 = fs.readdir_core(ROOT_INO, 0).expect("readdir 0");
    assert!(page0.len() >= 5); // . .. a b c
    // Request a later offset; earlier entries are skipped.
    let skip_to = page0[3].offset; // offset of the entry after "b"'s slot
    let page1 = fs.readdir_core(ROOT_INO, skip_to).expect("readdir offset");
    let names: Vec<&str> = page1.iter().map(|e| e.name.as_str()).collect();
    assert!(!names.contains(&"."));
    assert!(!names.contains(&".."));
}

#[test]
fn inode_stable_across_lookup_and_readdir() {
    let (fs, root) = make_fs();
    fs.backend.add_dir(root, "apps");
    let lookup_attr = fs.lookup_core(ROOT_INO, "apps").expect("lookup");
    let entries = fs.readdir_core(ROOT_INO, 0).expect("readdir");
    let apps_entry = entries.iter().find(|e| e.name == "apps").expect("apps in readdir");
    assert_eq!(lookup_attr.ino, apps_entry.ino);
}

#[test]
fn mount_options_include_fsname() {
    let opts = Fs2Filesystem::<MockBackend>::mount_options();
    assert!(opts.iter().any(|o| matches!(o, fuser::MountOption::FSName(n) if n == "fs2")));
}

/// A mock `BlobReader` returning canned bytes by blob id.
struct MockBlobReader {
    bytes: std::collections::HashMap<String, Vec<u8>>,
}

impl BlobReader for MockBlobReader {
    fn read(&self, blob_id: &str) -> Result<Vec<u8>, ReadError> {
        self.bytes
            .get(blob_id)
            .cloned()
            .ok_or(ReadError::NotHydrated)
    }
}

/// Build a `LocalStore` with a root + one file node (via operation replay).
fn store_with_file(content: &[u8]) -> (fs2_sync::LocalStore, WorkspaceId, NodeId, NodeId) {
    use fs2_core::RevisionId;
    let store = fs2_sync::LocalStore::open_in_memory().unwrap();
    let ws_id = WorkspaceId::new();
    let root_id = NodeId::new();
    let device = DeviceId::new();
    store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
    store.insert_root_node(ws_id, root_id).unwrap();
    let file_node_id = NodeId::new();
    let rev_id = RevisionId::new();
    let blob_id = format!("sha256:{file_node_id}");
    let op = Operation::new(
        ws_id,
        device,
        Cursor::zero(),
        OperationKind::CreateNode {
            parent_id: root_id,
            name: "hello.txt".to_owned(),
            kind: NodeKind::File,
            initial_revision: Some(NodeRevision {
                revision_id: rev_id,
                node_id: file_node_id,
                workspace_id: ws_id,
                device_id: device,
                base_revision_id: None,
                content: RevisionContent::File {
                    blob_id: blob_id.clone(),
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
    store.apply_operation(&op, Cursor::from(1)).unwrap();
    let file_node = store
        .get_node_by_path(ws_id, "hello.txt")
        .unwrap()
        .expect("file node");
    (store, ws_id, root_id, file_node.node_id)
}

#[test]
fn local_store_backend_lookup_and_read() {
    let content = b"hello from local store";
    let (store, ws_id, root_id, _file_node_id) = store_with_file(content);
    // The blob id stored in the revision is the one from the op; fetch it.
    let file_node = store
        .get_node_by_path(ws_id, "hello.txt")
        .unwrap()
        .expect("file node");
    let rev = store
        .get_revision(file_node.current_revision_id.expect("rev id"))
        .unwrap()
        .expect("revision");
    let blob_id = rev.blob_id.expect("blob id");
    let mut bytes = std::collections::HashMap::new();
    bytes.insert(blob_id, content.to_vec());
    let reader = MockBlobReader { bytes };
    let backend = LocalStoreBackend::new(store, reader, ws_id, root_id);
    let fs = Fs2Filesystem::new(backend);

    let root_attr = fs.getattr_core(ROOT_INO).expect("root");
    assert_eq!(root_attr.kind, fuser::FileType::Directory);

    let attr = fs.lookup_core(ROOT_INO, "hello.txt").expect("lookup file");
    assert_eq!(attr.kind, fuser::FileType::RegularFile);
    assert_eq!(attr.size, content.len() as u64);

    let read_bytes = fs.read_core(attr.ino, 0, 100).expect("read");
    assert_eq!(read_bytes, content);

    let entries = fs.readdir_core(ROOT_INO, 0).expect("readdir");
    assert!(entries.iter().any(|e| e.name == "hello.txt"));
}

#[test]
fn local_store_backend_read_offline_is_eio() {
    let content = b"offline test";
    let (store, ws_id, root_id, _file_node_id) = store_with_file(content);
    let backend = LocalStoreBackend::new(store, NoBlobReader, ws_id, root_id);
    let fs = Fs2Filesystem::new(backend);
    let attr = fs.lookup_core(ROOT_INO, "hello.txt").expect("lookup");
    assert_eq!(fs.read_core(attr.ino, 0, 100).unwrap_err(), libc::EIO);
}

#[test]
fn local_store_backend_readlink_for_symlink() {
    use fs2_core::RevisionId;
    let store = fs2_sync::LocalStore::open_in_memory().unwrap();
    let ws_id = WorkspaceId::new();
    let root_id = NodeId::new();
    let device = DeviceId::new();
    store.upsert_workspace(ws_id, "test-ws", root_id).unwrap();
    store.insert_root_node(ws_id, root_id).unwrap();
    let link_node_id = NodeId::new();
    let rev_id = RevisionId::new();
    let op = Operation::new(
        ws_id,
        device,
        Cursor::zero(),
        OperationKind::CreateNode {
            parent_id: root_id,
            name: "link".to_owned(),
            kind: NodeKind::Symlink,
            initial_revision: Some(NodeRevision {
                revision_id: rev_id,
                node_id: link_node_id,
                workspace_id: ws_id,
                device_id: device,
                base_revision_id: None,
                content: RevisionContent::Symlink {
                    target: "../other.txt".to_owned(),
                },
                posix_mode: 0o777,
                mtime: chrono::Utc::now(),
                size: "../other.txt".len() as u64,
                executable: false,
                created_at: chrono::Utc::now(),
            }),
        },
        chrono::Utc::now(),
    );
    store.apply_operation(&op, Cursor::from(1)).unwrap();
    let backend = LocalStoreBackend::new(store, NoBlobReader, ws_id, root_id);
    let fs = Fs2Filesystem::new(backend);
    let attr = fs.lookup_core(ROOT_INO, "link").expect("lookup link");
    assert_eq!(attr.kind, fuser::FileType::Symlink);
    let target = fs.readlink_core(attr.ino).expect("readlink");
    assert_eq!(target, "../other.txt");
}
