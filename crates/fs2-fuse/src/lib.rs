#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Read-only FUSE adapter skeleton for FS2 workspaces.

use fs2_core::{Node, NodeId, NodeKind, WorkspaceId};
use fs2_daemon::LocalStore;
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyDirectory,
    ReplyEntry, Request,
};
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::Path,
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

#[derive(Debug)]
pub struct MetadataWorkspaceFs {
    store: LocalStore,
    workspace_id: WorkspaceId,
    inodes: InodeMap,
}

impl MetadataWorkspaceFs {
    #[must_use]
    pub fn new(store: LocalStore, workspace_id: WorkspaceId, root_node_id: NodeId) -> Self {
        Self {
            store,
            workspace_id,
            inodes: InodeMap::new(root_node_id),
        }
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

fn empty_mount_options() -> Vec<MountOption> {
    vec![MountOption::RO, MountOption::FSName("fs2-empty".to_owned())]
}

fn metadata_mount_options() -> Vec<MountOption> {
    vec![
        MountOption::RO,
        MountOption::FSName("fs2-metadata".to_owned()),
    ]
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
