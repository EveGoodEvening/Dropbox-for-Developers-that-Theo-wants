#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Read-only FUSE adapter skeleton for FS2 workspaces.

use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyDirectory,
    ReplyEntry, Request,
};
use std::{
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootDirectoryEntry {
    pub inode: u64,
    pub kind: FileType,
    pub name: &'static str,
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
        let skip = usize::try_from(offset.max(0)).unwrap_or(usize::MAX);
        for (entry, next_offset) in self.root_entries().into_iter().zip([1_i64, 2]).skip(skip) {
            if reply.add(entry.inode, next_offset, entry.kind, entry.name) {
                break;
            }
        }
        reply.ok();
    }
}

pub fn mount_empty_workspace(mountpoint: impl AsRef<Path>) -> std::io::Result<BackgroundSession> {
    fuser::spawn_mount2(
        EmptyWorkspaceFs::new(),
        mountpoint,
        &[
            MountOption::RO,
            MountOption::FSName("fs2-empty".to_owned()),
            MountOption::DefaultPermissions,
        ],
    )
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn manual_test_instructions_cover_linux_and_macos() {
        assert!(MANUAL_TEST_INSTRUCTIONS.contains("Linux"));
        assert!(MANUAL_TEST_INSTRUCTIONS.contains("macOS"));
        assert!(MANUAL_TEST_INSTRUCTIONS.contains("ls <mount>"));
    }

    #[test]
    fn mounted_empty_workspace_lists_empty_root() -> Result<(), Box<dyn std::error::Error>> {
        let mountpoint = tempfile::tempdir()?;
        let _session = mount_empty_workspace(mountpoint.path())?;
        let entries = std::fs::read_dir(mountpoint.path())?.collect::<Result<Vec<_>, _>>()?;
        assert!(entries.is_empty());
        Ok(())
    }
}
