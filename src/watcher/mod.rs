//! Watcher/indexer foundation for CHUNK-04.
//!
//! U4 resolution: this chunk intentionally freezes the watcher-facing snapshot
//! and event-queue contracts while shipping a stdlib-only polling filesystem
//! adapter for macOS and Linux project roots. The adapter walks a real root with
//! `std::fs`, records deterministic metadata snapshots without following
//! symlinks, prunes non-overridable Git control trees, and avoids descending into
//! ignored trees unless `.syncignore` negation rules require explicit descendant
//! evaluation. Later daemon chunks can add native backends (`fsevents` on macOS,
//! `inotify` on Linux, or another notifier) behind the same [`SnapshotProducer`],
//! [`IndexedSnapshot`], and [`EventQueue`] contracts without changing what
//! CHUNK-05 consumes.
//!
//! The indexer applies CHUNK-03 policy before events leave this module. Ignored,
//! rebuild-local, and platform-pinned entries remain visible as policy metadata,
//! but their content-sync disposition is suppressed so raw unsafe filesystem
//! changes do not leak into the sync/convergence layer. [`EventQueue`] can bind to
//! stdlib file-backed storage using the full `watcher_events` event payload so
//! unacknowledged events survive process restart.

use crate::catalog::{
    diff_manifests, ContentHash, ManifestId, ProjectId, StructureChange, TreeEntry,
    TreeEntryKind, TreeManifest,
};
use crate::foundation::{Migration, MigrationError, MigrationRunner, Platform, WatchError};
use crate::policy::{Action, PlatformPin, Policy, RuleEffect};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const MODULE_NAME: &str = "watcher";

pub const WATCHER_SNAPSHOT_CONTRACT_VERSION: &str = "watcher-snapshot-v1";
pub const WATCHER_EVENT_QUEUE_CONTRACT_VERSION: &str = "watcher-event-queue-v1";
pub const WATCHER_EVENT_QUEUE_STORAGE_FORMAT_VERSION: &str = "watcher-event-queue-lines-v1";

pub const WATCHER_EVENTS_MIGRATION_VERSION: &str = "watcher_events_v1";
pub const WATCHER_EVENTS_MIGRATION_DESCRIPTION: &str =
    "watcher event queue schema; persists full policy-aware filesystem events for sync";
pub const WATCHER_EVENTS_TABLE: &str = "watcher_events";
pub const WATCHER_EVENTS_MIGRATION_TABLES: &[&str] = &[WATCHER_EVENTS_TABLE];
pub const WATCHER_EVENTS_MIGRATION_UP_SQL: &[&str] = &[concat!(
    "CREATE TABLE watcher_events (",
    "id TEXT PRIMARY KEY, ",
    "project_id TEXT, ",
    "sequence INTEGER NOT NULL, ",
    "contract_version TEXT NOT NULL, ",
    "kind TEXT NOT NULL, ",
    "path TEXT NOT NULL, ",
    "previous_path TEXT, ",
    "content_sync TEXT NOT NULL, ",
    "before_path TEXT, ",
    "before_entry_kind TEXT, ",
    "before_size_bytes INTEGER, ",
    "before_modified_unix_millis INTEGER, ",
    "before_permissions INTEGER, ",
    "before_content_hash TEXT, ",
    "before_symlink_target TEXT, ",
    "before_policy_action TEXT, ",
    "before_policy_content_sync TEXT, ",
    "before_rebuild_hint TEXT, ",
    "before_platform_pin_os TEXT, ",
    "before_platform_pin_arch TEXT, ",
    "after_path TEXT, ",
    "after_entry_kind TEXT, ",
    "after_size_bytes INTEGER, ",
    "after_modified_unix_millis INTEGER, ",
    "after_permissions INTEGER, ",
    "after_content_hash TEXT, ",
    "after_symlink_target TEXT, ",
    "after_policy_action TEXT, ",
    "after_policy_content_sync TEXT, ",
    "after_rebuild_hint TEXT, ",
    "after_platform_pin_os TEXT, ",
    "after_platform_pin_arch TEXT, ",
    "snapshot_id TEXT, ",
    "manifest_id TEXT, ",
    "created_unix_millis INTEGER NOT NULL, ",
    "acked INTEGER NOT NULL);"
)];
pub const WATCHER_EVENTS_MIGRATION_DOWN_SQL: &[&str] = &["DROP TABLE watcher_events;"];

/// Frozen content-sync disposition attached to both snapshots and events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentSyncDisposition {
    /// The entry may be considered by CHUNK-05 for ordinary content sync.
    Safe,
    /// The entry is metadata-only for CHUNK-05; content sync must not consume it.
    Suppressed,
}

impl ContentSyncDisposition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Suppressed => "suppressed",
        }
    }
}

/// Metadata hint telling CHUNK-05 why content bytes are intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildHint {
    RebuildLocally,
}

impl RebuildHint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RebuildLocally => "rebuild-locally",
        }
    }
}

/// Policy metadata preserved for every indexed entry and event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyMetadata {
    pub action: Action,
    pub content_sync: ContentSyncDisposition,
    pub rebuild_hint: Option<RebuildHint>,
    pub platform_pin: Option<PlatformPin>,
}

impl PolicyMetadata {
    pub fn from_action(action: Action) -> Self {
        let content_sync = match &action {
            Action::Sync => ContentSyncDisposition::Safe,
            Action::Ignore | Action::RebuildLocally | Action::PlatformPin(_) => {
                ContentSyncDisposition::Suppressed
            }
        };
        let rebuild_hint = match &action {
            Action::RebuildLocally => Some(RebuildHint::RebuildLocally),
            Action::Sync | Action::Ignore | Action::PlatformPin(_) => None,
        };
        let platform_pin = match &action {
            Action::PlatformPin(pin) => Some(pin.clone()),
            Action::Sync | Action::Ignore | Action::RebuildLocally => None,
        };

        Self {
            action,
            content_sync,
            rebuild_hint,
            platform_pin,
        }
    }

    pub fn allows_content_sync(&self) -> bool {
        self.content_sync == ContentSyncDisposition::Safe
    }
}

/// A catalog entry plus the policy decision that was made at index time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub catalog_entry: TreeEntry,
    pub policy: PolicyMetadata,
    pub symlink_target: Option<String>,
}

impl SnapshotEntry {
    pub fn new(catalog_entry: TreeEntry, policy: PolicyMetadata) -> Self {
        Self {
            catalog_entry,
            policy,
            symlink_target: None,
        }
    }

    fn with_symlink_target(mut self, symlink_target: Option<String>) -> Self {
        self.symlink_target = symlink_target;
        self
    }

    pub fn path(&self) -> &str {
        &self.catalog_entry.path
    }

    pub fn allows_content_sync(&self) -> bool {
        self.policy.allows_content_sync()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotPolicyContext {
    policy: Policy,
    platform: Platform,
}

/// Stable snapshot contract consumed by CHUNK-05.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSnapshot {
    pub contract_version: &'static str,
    pub id: ManifestId,
    pub project_id: ProjectId,
    pub manifest: TreeManifest,
    pub entries: Vec<SnapshotEntry>,
    policy_context: Option<SnapshotPolicyContext>,
}

impl IndexedSnapshot {
    pub fn new(project_id: impl Into<ProjectId>, entries: Vec<SnapshotEntry>) -> Self {
        Self::new_with_policy_context(project_id, entries, None)
    }

    fn new_with_policy_context(
        project_id: impl Into<ProjectId>,
        mut entries: Vec<SnapshotEntry>,
        policy_context: Option<SnapshotPolicyContext>,
    ) -> Self {
        let project_id = project_id.into();
        entries.sort_by(compare_snapshot_entries);
        let manifest_id = catalog_fingerprint(&project_id, &entries);
        let snapshot_id = snapshot_fingerprint(&project_id, &entries);
        let manifest_entries = entries
            .iter()
            .map(|entry| entry.catalog_entry.clone())
            .collect::<Vec<_>>();
        let manifest = TreeManifest::new(manifest_id, project_id.clone(), manifest_entries);

        Self {
            contract_version: WATCHER_SNAPSHOT_CONTRACT_VERSION,
            id: snapshot_id,
            project_id,
            manifest,
            entries,
            policy_context,
        }
    }

    pub fn entry(&self, path: &str) -> Option<&SnapshotEntry> {
        self.entries.iter().find(|entry| entry.path() == path)
    }

    pub fn content_sync_entries(&self) -> impl Iterator<Item = &SnapshotEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.allows_content_sync())
    }

    fn entries_by_path(&self) -> BTreeMap<String, SnapshotEntry> {
        self.entries
            .iter()
            .map(|entry| (entry.path().to_owned(), entry.clone()))
            .collect()
    }

    fn current_policy_for_path(&self, path: &str) -> Option<PolicyMetadata> {
        self.policy_context.as_ref().map(|context| {
            PolicyMetadata::from_action(context.policy.evaluate(path, &context.platform))
        })
    }
}

/// Frozen event kinds; path-specific data lives on [`FsEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Edited,
    Moved,
    Deleted,
    PermissionChanged,
    SymlinkChanged,
    PolicyChanged,
}

impl EventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Edited => "edited",
            Self::Moved => "moved",
            Self::Deleted => "deleted",
            Self::PermissionChanged => "permission-changed",
            Self::SymlinkChanged => "symlink-changed",
            Self::PolicyChanged => "policy-changed",
        }
    }
}

/// Policy-aware filesystem event handed to CHUNK-05.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    pub contract_version: &'static str,
    pub sequence: u64,
    pub kind: EventKind,
    pub path: String,
    pub previous_path: Option<String>,
    pub before: Option<SnapshotEntry>,
    pub after: Option<SnapshotEntry>,
    pub content_sync: ContentSyncDisposition,
}

impl FsEvent {
    pub fn new(
        kind: EventKind,
        path: impl Into<String>,
        previous_path: Option<String>,
        before: Option<SnapshotEntry>,
        after: Option<SnapshotEntry>,
    ) -> Self {
        let content_sync = event_content_sync(kind, before.as_ref(), after.as_ref());
        Self {
            contract_version: WATCHER_EVENT_QUEUE_CONTRACT_VERSION,
            sequence: 0,
            kind,
            path: path.into(),
            previous_path,
            before,
            after,
            content_sync,
        }
    }

    pub fn created(after: SnapshotEntry) -> Self {
        Self::new(EventKind::Created, after.path().to_owned(), None, None, Some(after))
    }

    pub fn updated(before: SnapshotEntry, after: SnapshotEntry) -> Self {
        event_from_updated_entries(before, after)
    }

    pub fn moved(before: SnapshotEntry, after: SnapshotEntry) -> Self {
        Self::new(
            EventKind::Moved,
            after.path().to_owned(),
            Some(before.path().to_owned()),
            Some(before),
            Some(after),
        )
    }

    pub fn deleted(before: SnapshotEntry) -> Self {
        Self::new(EventKind::Deleted, before.path().to_owned(), None, Some(before), None)
    }

    pub fn allows_content_sync(&self) -> bool {
        self.content_sync == ContentSyncDisposition::Safe
    }

    pub fn policy_metadata(&self) -> Option<&PolicyMetadata> {
        self.after
            .as_ref()
            .or(self.before.as_ref())
            .map(|entry| &entry.policy)
    }
}

/// Policy-aware event queue with optional stdlib file-backed durability.
///
/// The durable file uses [`WATCHER_EVENT_QUEUE_STORAGE_FORMAT_VERSION`], a stable
/// line-oriented encoding of the full [`FsEvent`] payload. Native database-backed
/// storage can later write the same fields into the `watcher_events` table shape
/// described by [`WATCHER_EVENTS_MIGRATION_UP_SQL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventQueue {
    events: VecDeque<FsEvent>,
    next_sequence: u64,
    storage_path: Option<PathBuf>,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
            next_sequence: 1,
            storage_path: None,
        }
    }

    pub fn from_snapshots(previous: Option<&IndexedSnapshot>, current: &IndexedSnapshot) -> Self {
        let mut queue = Self::new();
        queue.extend_snapshot_diff(previous, current);
        queue
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, WatchError> {
        let path = path.as_ref().to_path_buf();
        let mut queue = if path.exists() {
            parse_event_queue_file(&path)?
        } else {
            Self::new()
        };
        queue.storage_path = Some(path);
        queue.flush()?;
        Ok(queue)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, WatchError> {
        let path = path.as_ref().to_path_buf();
        let mut queue = if path.exists() {
            parse_event_queue_file(&path)?
        } else {
            Self::new()
        };
        queue.storage_path = Some(path);
        Ok(queue)
    }

    pub fn bind_storage(mut self, path: impl AsRef<Path>) -> Result<Self, WatchError> {
        self.storage_path = Some(path.as_ref().to_path_buf());
        self.flush()?;
        Ok(self)
    }

    pub fn persist_to_path(&self, path: impl AsRef<Path>) -> Result<(), WatchError> {
        write_event_queue_file(path.as_ref(), self)
    }

    pub fn flush(&self) -> Result<(), WatchError> {
        if let Some(path) = &self.storage_path {
            self.persist_to_path(path)?;
        }
        Ok(())
    }

    pub fn storage_path(&self) -> Option<&Path> {
        self.storage_path.as_deref()
    }

    pub fn contract_version(&self) -> &'static str {
        WATCHER_EVENT_QUEUE_CONTRACT_VERSION
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn iter(&self) -> impl Iterator<Item = &FsEvent> {
        self.events.iter()
    }

    pub fn content_sync_events(&self) -> impl Iterator<Item = &FsEvent> {
        self.events.iter().filter(|event| event.allows_content_sync())
    }

    pub fn peek(&self) -> Option<&FsEvent> {
        self.events.front()
    }

    pub fn pop_front(&mut self) -> Option<FsEvent> {
        self.events.pop_front()
    }

    pub fn try_pop_front(&mut self) -> Result<Option<FsEvent>, WatchError> {
        let event = self.events.pop_front();
        if let Some(event_ref) = event.as_ref() {
            if let Err(error) = self.flush() {
                self.events.push_front(event_ref.clone());
                return Err(error);
            }
        }
        Ok(event)
    }

    pub fn drain(&mut self) -> Vec<FsEvent> {
        self.events.drain(..).collect()
    }

    pub fn try_drain(&mut self) -> Result<Vec<FsEvent>, WatchError> {
        let events = self.drain();
        if !events.is_empty() {
            if let Err(error) = self.flush() {
                self.events = events.iter().cloned().collect();
                return Err(error);
            }
        }
        Ok(events)
    }

    pub fn enqueue(&mut self, mut event: FsEvent) {
        event.sequence = self.next_sequence;
        self.next_sequence += 1;
        self.coalesce_or_push(event);
    }

    pub fn try_enqueue(&mut self, event: FsEvent) -> Result<(), WatchError> {
        let mut staged = self.clone();
        staged.enqueue(event);
        staged.flush()?;
        *self = staged;
        Ok(())
    }

    pub fn extend_snapshot_diff(
        &mut self,
        previous: Option<&IndexedSnapshot>,
        current: &IndexedSnapshot,
    ) {
        let Some(previous) = previous else {
            for entry in &current.entries {
                self.enqueue(FsEvent::created(entry.clone()));
            }
            return;
        };

        let before_by_path = previous.entries_by_path();
        let after_by_path = current.entries_by_path();
        let after_suppressed_dirs = after_by_path
            .values()
            .filter(|entry| {
                entry.catalog_entry.kind == TreeEntryKind::Directory && !entry.allows_content_sync()
            })
            .map(|entry| entry.path().to_owned())
            .collect::<Vec<_>>();
        let mut emitted_paths = BTreeSet::new();

        for change in diff_manifests(&previous.manifest, &current.manifest) {
            match change {
                StructureChange::Added { entry } => {
                    if let Some(after) = after_by_path.get(&entry.path) {
                        emitted_paths.insert(entry.path);
                        self.enqueue(FsEvent::created(after.clone()));
                    }
                }
                StructureChange::Removed { entry } => {
                    let path = entry.path;
                    if let Some(before) = before_by_path.get(&path) {
                        emitted_paths.insert(path.clone());
                        let mut event = FsEvent::deleted(before.clone());
                        if removed_path_is_suppressed_by_current_policy(
                            &path,
                            current,
                            &after_suppressed_dirs,
                        ) {
                            event.content_sync = ContentSyncDisposition::Suppressed;
                        }
                        self.enqueue(event);
                    }
                }
                StructureChange::Modified { before, after } => {
                    if let (Some(before_entry), Some(after_entry)) =
                        (before_by_path.get(&before.path), after_by_path.get(&after.path))
                    {
                        emitted_paths.insert(after.path);
                        self.enqueue(FsEvent::updated(before_entry.clone(), after_entry.clone()));
                    }
                }
                StructureChange::Moved { before, after } => {
                    if let (Some(before_entry), Some(after_entry)) =
                        (before_by_path.get(&before.path), after_by_path.get(&after.path))
                    {
                        emitted_paths.insert(before.path);
                        emitted_paths.insert(after.path);
                        self.enqueue_policy_aware_move(
                            before_entry.clone(),
                            after_entry.clone(),
                            current,
                            &after_suppressed_dirs,
                        );
                    }
                }
            }
        }

        for (path, after) in &after_by_path {
            if emitted_paths.contains(path) {
                continue;
            }
            if let Some(before) = before_by_path.get(path) {
                if before.catalog_entry == after.catalog_entry && before.policy != after.policy {
                    self.enqueue(FsEvent::new(
                        EventKind::PolicyChanged,
                        path.clone(),
                        None,
                        Some(before.clone()),
                        Some(after.clone()),
                    ));
                }
            }
        }
    }

    fn enqueue_policy_aware_move(
        &mut self,
        before: SnapshotEntry,
        after: SnapshotEntry,
        current: &IndexedSnapshot,
        after_suppressed_dirs: &[String],
    ) {
        let source_suppressed_by_current_policy = removed_path_is_suppressed_by_current_policy(
            before.path(),
            current,
            after_suppressed_dirs,
        );
        match policy_aware_move_events(before, after, source_suppressed_by_current_policy) {
            PolicyAwareMoveEvents::Single(event) => self.enqueue(*event),
            PolicyAwareMoveEvents::Split(events) => {
                let [delete_source, create_destination] = *events;
                self.enqueue(delete_source);
                self.enqueue(create_destination);
            }
        }
    }

    fn coalesce_or_push(&mut self, event: FsEvent) {
        let mut events = self.events.drain(..).collect::<Vec<_>>();
        events.push(event);
        events.sort_by_key(|event| event.sequence);

        let mut index = 0;
        while index < events.len() {
            let mut probe = index + 1;
            let mut changed = false;

            while probe < events.len() {
                if !events_overlap(&events[index], &events[probe]) {
                    probe += 1;
                    continue;
                }

                let second = events.remove(probe);
                let first = events.remove(index);
                match coalesce_events(first, second) {
                    CoalesceResult::DropBoth => {}
                    CoalesceResult::One(coalesced) => events.push(*coalesced),
                    CoalesceResult::Split(first, second) => {
                        events.push(*first);
                        events.push(*second);
                    }
                    CoalesceResult::Both(first, second) => {
                        events.insert(index, *first);
                        events.insert(probe, *second);
                        probe += 1;
                        continue;
                    }
                }

                events.sort_by_key(|event| event.sequence);
                index = 0;
                changed = true;
                break;
            }

            if !changed {
                index += 1;
            }
        }

        self.events = events.into_iter().collect();
    }
}

/// Tree abstraction used by the stdlib polling indexer.
pub trait FileTree {
    fn entries(&self) -> Result<Vec<LocalTreeEntry>, WatchError>;
}

/// Deterministic in-memory tree for tests and polling adapters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InMemoryTree {
    entries: Vec<LocalTreeEntry>,
}

impl InMemoryTree {
    pub fn new(entries: Vec<LocalTreeEntry>) -> Self {
        Self { entries }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn push(&mut self, entry: LocalTreeEntry) {
        self.entries.push(entry);
    }

    pub fn as_slice(&self) -> &[LocalTreeEntry] {
        &self.entries
    }
}

impl From<Vec<LocalTreeEntry>> for InMemoryTree {
    fn from(entries: Vec<LocalTreeEntry>) -> Self {
        Self::new(entries)
    }
}

impl FileTree for InMemoryTree {
    fn entries(&self) -> Result<Vec<LocalTreeEntry>, WatchError> {
        Ok(self.entries.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LocalEntryKind {
    Directory,
    File,
    Symlink,
}


/// Metadata-only local tree entry. File contents are never stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTreeEntry {
    pub path: String,
    pub kind: LocalEntryKind,
    pub size_bytes: u64,
    pub modified_unix_millis: u64,
    pub permissions: u32,
    pub content_hash: Option<ContentHash>,
    pub symlink_target: Option<String>,
}

impl LocalTreeEntry {
    pub fn directory(path: impl Into<String>, modified_unix_millis: u64, permissions: u32) -> Self {
        Self {
            path: path.into(),
            kind: LocalEntryKind::Directory,
            size_bytes: 0,
            modified_unix_millis,
            permissions,
            content_hash: None,
            symlink_target: None,
        }
    }

    pub fn file(
        path: impl Into<String>,
        size_bytes: u64,
        modified_unix_millis: u64,
        permissions: u32,
        content_hash: Option<ContentHash>,
    ) -> Self {
        Self {
            path: path.into(),
            kind: LocalEntryKind::File,
            size_bytes,
            modified_unix_millis,
            permissions,
            content_hash,
            symlink_target: None,
        }
    }

    pub fn symlink(
        path: impl Into<String>,
        target: impl Into<String>,
        modified_unix_millis: u64,
        permissions: u32,
    ) -> Self {
        let target = target.into();
        Self {
            path: path.into(),
            kind: LocalEntryKind::Symlink,
            size_bytes: 0,
            modified_unix_millis,
            permissions,
            content_hash: Some(stable_hash_string("symlink-target", &target)),
            symlink_target: Some(target),
        }
    }

    fn with_path(mut self, path: String) -> Self {
        self.path = path;
        self
    }

    fn to_catalog_entry(&self) -> TreeEntry {
        match self.kind {
            LocalEntryKind::Directory => TreeEntry::directory(
                self.path.clone(),
                self.modified_unix_millis,
                self.permissions,
            ),
            LocalEntryKind::File => TreeEntry::file(
                self.path.clone(),
                self.size_bytes,
                self.modified_unix_millis,
                self.permissions,
                self.content_hash.clone(),
            ),
            LocalEntryKind::Symlink => TreeEntry::symlink(
                self.path.clone(),
                self.modified_unix_millis,
                self.permissions,
                self.content_hash.clone(),
            ),
        }
    }
}

/// Producer interface frozen for CHUNK-05 and future native watcher adapters.
pub trait SnapshotProducer {
    fn produce_snapshot(&self) -> Result<IndexedSnapshot, WatchError>;
}

pub struct SnapshotIndexJob<'a, T: FileTree + ?Sized> {
    indexer: &'a LocalIndexer,
    tree: &'a T,
}

impl<T: FileTree + ?Sized> SnapshotProducer for SnapshotIndexJob<'_, T> {
    fn produce_snapshot(&self) -> Result<IndexedSnapshot, WatchError> {
        self.indexer.index_tree(self.tree)
    }
}

/// Stdlib polling adapter for macOS and Linux project roots.
///
/// This adapter performs a deterministic `std::fs` walk, records metadata without
/// reading through symlinks, and leaves the watcher contract open for later native
/// `fsevents`/`inotify` implementations behind [`SnapshotProducer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollingFileSystemAdapter {
    root: PathBuf,
    indexer: LocalIndexer,
}

impl PollingFileSystemAdapter {
    pub fn new(root: impl Into<PathBuf>, indexer: LocalIndexer) -> Self {
        Self {
            root: root.into(),
            indexer,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn indexer(&self) -> &LocalIndexer {
        &self.indexer
    }
}

impl SnapshotProducer for PollingFileSystemAdapter {
    fn produce_snapshot(&self) -> Result<IndexedSnapshot, WatchError> {
        self.indexer.index_project_root(&self.root)
    }
}

/// Stdlib-only local indexer used by the polling adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIndexer {
    project_id: ProjectId,
    policy: Policy,
    platform: Platform,
}

impl LocalIndexer {
    pub fn new(project_id: impl Into<ProjectId>, policy: Policy, platform: Platform) -> Self {
        Self {
            project_id: project_id.into(),
            policy,
            platform,
        }
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    pub fn producer<'a, T: FileTree + ?Sized>(&'a self, tree: &'a T) -> SnapshotIndexJob<'a, T> {
        SnapshotIndexJob { indexer: self, tree }
    }

    pub fn polling_adapter(&self, root: impl Into<PathBuf>) -> PollingFileSystemAdapter {
        PollingFileSystemAdapter::new(root, self.clone())
    }

    pub fn index_project_root(&self, root: impl AsRef<Path>) -> Result<IndexedSnapshot, WatchError> {
        let entries = collect_project_root_entries(root.as_ref(), self)?;
        self.index_entries(entries)
    }

    pub fn index_tree<T: FileTree + ?Sized>(
        &self,
        tree: &T,
    ) -> Result<IndexedSnapshot, WatchError> {
        self.index_entries(tree.entries()?)
    }

    fn index_entries(&self, entries: Vec<LocalTreeEntry>) -> Result<IndexedSnapshot, WatchError> {
        let mut entries_by_path = BTreeMap::new();
        for entry in entries {
            let path = normalize_relative_path(&entry.path)?;
            entries_by_path.insert(path.clone(), entry.with_path(path));
        }

        let mut snapshot_entries = Vec::new();
        let mut non_traversed_prefixes = Vec::new();
        for (path, local_entry) in entries_by_path {
            if is_descendant_of_any(&path, &non_traversed_prefixes) {
                continue;
            }

            let action = self.policy.evaluate(&path, &self.platform);
            let policy = PolicyMetadata::from_action(action);
            let catalog_entry = local_entry.to_catalog_entry();
            let symlink_target = if catalog_entry.kind == TreeEntryKind::Symlink {
                local_entry.symlink_target.clone()
            } else {
                None
            };
            let should_not_traverse = catalog_entry.kind == TreeEntryKind::Symlink
                || is_git_metadata_directory(&path, &catalog_entry, &self.platform);
            snapshot_entries.push(
                SnapshotEntry::new(catalog_entry, policy).with_symlink_target(symlink_target),
            );
            if should_not_traverse {
                non_traversed_prefixes.push(path);
            }
        }

        Ok(IndexedSnapshot::new_with_policy_context(
            self.project_id.clone(),
            snapshot_entries,
            Some(SnapshotPolicyContext {
                policy: self.policy.clone(),
                platform: self.platform.clone(),
            }),
        ))
    }

    pub fn index_tree_with_events<T: FileTree + ?Sized>(
        &self,
        tree: &T,
        previous: Option<&IndexedSnapshot>,
    ) -> Result<(IndexedSnapshot, EventQueue), WatchError> {
        let snapshot = self.index_tree(tree)?;
        let queue = EventQueue::from_snapshots(previous, &snapshot);
        Ok((snapshot, queue))
    }

    pub fn index_project_root_with_events(
        &self,
        root: impl AsRef<Path>,
        previous: Option<&IndexedSnapshot>,
    ) -> Result<(IndexedSnapshot, EventQueue), WatchError> {
        let snapshot = self.index_project_root(root)?;
        let queue = EventQueue::from_snapshots(previous, &snapshot);
        Ok((snapshot, queue))
    }
}

pub fn watcher_events_initial_migration() -> Migration {
    Migration::new(
        WATCHER_EVENTS_MIGRATION_VERSION,
        WATCHER_EVENTS_MIGRATION_DESCRIPTION,
        WATCHER_EVENTS_MIGRATION_UP_SQL,
        WATCHER_EVENTS_MIGRATION_DOWN_SQL,
        WATCHER_EVENTS_MIGRATION_TABLES,
    )
}

pub fn watcher_events_migration_runner() -> Result<MigrationRunner, MigrationError> {
    MigrationRunner::with_migrations([watcher_events_initial_migration()])
}

const EVENT_QUEUE_NONE_FIELD: &str = "~";
const EVENT_QUEUE_ENTRY_FIELD_COUNT: usize = 13;
const EVENT_QUEUE_LINE_FIELD_COUNT: usize = 7 + (EVENT_QUEUE_ENTRY_FIELD_COUNT * 2);

fn collect_project_root_entries(
    root: &Path,
    indexer: &LocalIndexer,
) -> Result<Vec<LocalTreeEntry>, WatchError> {
    let metadata = fs::metadata(root)
        .map_err(|error| watch_io_error("read project root metadata", root, error))?;
    if !metadata.is_dir() {
        return Err(WatchError::unsupported(format!(
            "watcher project root is not a directory: {}",
            root.display()
        )));
    }

    let mut entries = Vec::new();
    let sync_overrides = policy_sync_overrides(indexer.policy());
    walk_project_directory(root, "", indexer, &sync_overrides, &mut entries)?;
    entries.sort_by(compare_local_tree_entries);
    Ok(entries)
}

fn walk_project_directory(
    absolute_dir: &Path,
    relative_dir: &str,
    indexer: &LocalIndexer,
    sync_overrides: &[SyncOverrideRule],
    entries: &mut Vec<LocalTreeEntry>,
) -> Result<(), WatchError> {
    let read_dir = match fs::read_dir(absolute_dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(watch_io_error("read project directory", absolute_dir, error)),
    };

    let mut children = Vec::new();
    for child in read_dir {
        let child = child.map_err(|error| watch_io_error("read project directory entry", absolute_dir, error))?;
        children.push(child.path());
    }
    children.sort_by_key(|path| path_file_name(path));

    for child_path in children {
        let child_name = path_file_name(&child_path);
        let relative_path = if relative_dir.is_empty() {
            child_name
        } else {
            format!("{relative_dir}/{child_name}")
        };
        let metadata = fs::symlink_metadata(&child_path)
            .map_err(|error| watch_io_error("read project entry metadata", &child_path, error))?;
        let entry = local_entry_from_metadata(&relative_path, &child_path, &metadata)?;
        let should_descend = entry.kind == LocalEntryKind::Directory
            && should_descend_polled_directory(&relative_path, indexer, sync_overrides);
        entries.push(entry);
        if should_descend {
            walk_project_directory(&child_path, &relative_path, indexer, sync_overrides, entries)?;
        }
    }

    Ok(())
}

fn should_descend_polled_directory(
    relative_path: &str,
    indexer: &LocalIndexer,
    sync_overrides: &[SyncOverrideRule],
) -> bool {
    let catalog_entry = TreeEntry::directory(relative_path.to_owned(), 0, 0);
    if is_git_metadata_directory(relative_path, &catalog_entry, indexer.platform()) {
        return false;
    }

    let policy = PolicyMetadata::from_action(indexer.policy().evaluate(relative_path, indexer.platform()));
    policy.allows_content_sync()
        || sync_override_may_match_descendant(sync_overrides, relative_path, indexer.platform())
}

fn local_entry_from_metadata(
    relative_path: &str,
    absolute_path: &Path,
    metadata: &fs::Metadata,
) -> Result<LocalTreeEntry, WatchError> {
    let modified_unix_millis = modified_unix_millis(metadata);
    let permissions = permissions_bits(metadata);
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        let target = fs::read_link(absolute_path)
            .map_err(|error| watch_io_error("read symlink target", absolute_path, error))?;
        return Ok(LocalTreeEntry::symlink(
            relative_path.to_owned(),
            path_to_slash_string(&target),
            modified_unix_millis,
            permissions,
        ));
    }

    if file_type.is_dir() {
        return Ok(LocalTreeEntry::directory(
            relative_path.to_owned(),
            modified_unix_millis,
            permissions,
        ));
    }

    let content_hash = hash_file_contents(absolute_path).ok();
    Ok(LocalTreeEntry::file(
        relative_path.to_owned(),
        metadata.len(),
        modified_unix_millis,
        permissions,
        content_hash,
    ))
}

fn path_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn path_to_slash_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn modified_unix_millis(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn permissions_bits(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn permissions_bits(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncOverrideRule {
    anchored: bool,
    contains_slash: bool,
    directory_only: bool,
    segments: Vec<String>,
}

impl SyncOverrideRule {
    fn from_original(original: &str) -> Self {
        let pattern = original
            .trim()
            .strip_prefix('!')
            .unwrap_or(original.trim())
            .trim_start();
        let anchored = pattern.starts_with('/');
        let directory_only = pattern.ends_with('/');
        let pattern = pattern
            .trim_start_matches('/')
            .trim_end_matches('/')
            .replace('\\', "/");
        let contains_slash = pattern.contains('/');
        let segments = pattern
            .split('/')
            .filter(|segment| !segment.is_empty() && *segment != ".")
            .map(str::to_owned)
            .collect::<Vec<_>>();

        Self {
            anchored,
            contains_slash,
            directory_only,
            segments,
        }
    }

    fn may_match_descendant(&self, relative_path: &str, case_sensitive: bool) -> bool {
        if self.segments.is_empty() {
            return false;
        }

        let path_segments = relative_path_segments(relative_path);
        if self.segments.len() == 1 && !self.contains_slash {
            if !self.anchored {
                return true;
            }
            return path_segments.first().is_some_and(|segment| {
                sync_segment_glob_matches(self.segments[0].as_str(), segment, case_sensitive)
            });
        }

        if !self.anchored {
            return true;
        }

        anchored_rule_may_match_descendant(
            &self.segments,
            &path_segments,
            case_sensitive,
            self.directory_only,
        )
    }
}

fn policy_sync_overrides(policy: &Policy) -> Vec<SyncOverrideRule> {
    policy
        .project_rules()
        .rules()
        .iter()
        .chain(policy.user_rules().rules())
        .filter(|rule| rule.effect() == RuleEffect::Sync)
        .map(|rule| SyncOverrideRule::from_original(rule.original()))
        .collect()
}

fn sync_override_may_match_descendant(
    sync_overrides: &[SyncOverrideRule],
    relative_path: &str,
    platform: &Platform,
) -> bool {
    let case_sensitive = platform.capabilities.case_sensitive_paths;
    sync_overrides
        .iter()
        .any(|rule| rule.may_match_descendant(relative_path, case_sensitive))
}

fn anchored_rule_may_match_descendant(
    pattern: &[String],
    path_prefix: &[&str],
    case_sensitive: bool,
    allow_prefix: bool,
) -> bool {
    if path_prefix.is_empty() {
        return true;
    }
    if pattern.is_empty() {
        return allow_prefix;
    }

    if pattern[0] == "**" {
        return anchored_rule_may_match_descendant(
            &pattern[1..],
            path_prefix,
            case_sensitive,
            allow_prefix,
        ) || anchored_rule_may_match_descendant(
            pattern,
            &path_prefix[1..],
            case_sensitive,
            allow_prefix,
        );
    }

    sync_segment_glob_matches(pattern[0].as_str(), path_prefix[0], case_sensitive)
        && anchored_rule_may_match_descendant(
            &pattern[1..],
            &path_prefix[1..],
            case_sensitive,
            allow_prefix,
        )
}

fn relative_path_segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
}

fn sync_segment_glob_matches(pattern: &str, text: &str, case_sensitive: bool) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let mut pattern_index = 0;
    let mut text_index = 0;
    let mut last_star = None;
    let mut retry_text_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?'
                || sync_chars_match(pattern[pattern_index], text[text_index], case_sensitive))
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            last_star = Some(pattern_index);
            pattern_index += 1;
            retry_text_index = text_index;
        } else if let Some(star_index) = last_star {
            pattern_index = star_index + 1;
            retry_text_index += 1;
            text_index = retry_text_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
}

fn sync_chars_match(left: char, right: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        left == right
    } else {
        left.eq_ignore_ascii_case(&right)
    }
}

fn is_git_metadata_directory(path: &str, entry: &TreeEntry, platform: &Platform) -> bool {
    entry.kind == TreeEntryKind::Directory
        && path
            .split('/')
            .any(|segment| path_segment_eq(segment, ".git", platform))
}

fn path_segment_eq(left: &str, right: &str, platform: &Platform) -> bool {
    if platform.capabilities.case_sensitive_paths {
        left == right
    } else {
        left.eq_ignore_ascii_case(right)
    }
}

fn hash_file_contents(path: &Path) -> Result<ContentHash, WatchError> {
    let mut file = File::open(path).map_err(|error| watch_io_error("open file for hashing", path, error))?;
    let mut hash = FNV_OFFSET;
    hash_str(&mut hash, "file-content");
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| watch_io_error("read file for hashing", path, error))?;
        if read == 0 {
            break;
        }
        hash_bytes(&mut hash, &buffer[..read]);
    }
    Ok(format!("fnv64:{hash:016x}"))
}

fn parse_event_queue_file(path: &Path) -> Result<EventQueue, WatchError> {
    let text = fs::read_to_string(path)
        .map_err(|error| watch_io_error("read watcher event queue", path, error))?;
    parse_event_queue(&text)
}

fn write_event_queue_file(path: &Path, queue: &EventQueue) -> Result<(), WatchError> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| watch_io_error("create watcher event queue directory", parent, error))?;
    }

    let temp_path = event_queue_temp_path(path);
    let encoded = serialize_event_queue(queue);
    {
        let mut file = File::create(&temp_path)
            .map_err(|error| watch_io_error("create watcher event queue", &temp_path, error))?;
        file.write_all(encoded.as_bytes())
            .map_err(|error| watch_io_error("write watcher event queue", &temp_path, error))?;
        file.sync_all()
            .map_err(|error| watch_io_error("sync watcher event queue", &temp_path, error))?;
    }
    fs::rename(&temp_path, path)
        .map_err(|error| watch_io_error("replace watcher event queue", path, error))
}

fn event_queue_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("watcher_events");
    path.with_file_name(format!("{file_name}.tmp"))
}

fn serialize_event_queue(queue: &EventQueue) -> String {
    let mut output = String::new();
    output.push_str("format\t");
    push_encoded_field(&mut output, WATCHER_EVENT_QUEUE_STORAGE_FORMAT_VERSION);
    output.push('\n');
    output.push_str("next_sequence\t");
    output.push_str(&queue.next_sequence.to_string());
    output.push('\n');
    for event in queue.iter() {
        push_event_line(&mut output, event);
    }
    output
}

fn push_event_line(output: &mut String, event: &FsEvent) {
    output.push_str("event\t");
    push_encoded_field(output, event.contract_version);
    output.push('\t');
    output.push_str(&event.sequence.to_string());
    output.push('\t');
    output.push_str(event.kind.as_str());
    output.push('\t');
    push_encoded_field(output, &event.path);
    output.push('\t');
    push_optional_encoded_field(output, event.previous_path.as_deref());
    output.push('\t');
    output.push_str(event.content_sync.as_str());
    push_snapshot_entry_fields(output, event.before.as_ref());
    push_snapshot_entry_fields(output, event.after.as_ref());
    output.push('\n');
}

fn push_snapshot_entry_fields(output: &mut String, entry: Option<&SnapshotEntry>) {
    let Some(entry) = entry else {
        output.push_str("\t0");
        for _ in 1..EVENT_QUEUE_ENTRY_FIELD_COUNT {
            output.push('\t');
            output.push_str(EVENT_QUEUE_NONE_FIELD);
        }
        return;
    };

    output.push_str("\t1\t");
    push_encoded_field(output, &entry.catalog_entry.path);
    output.push('\t');
    output.push_str(entry.catalog_entry.kind.as_str());
    output.push('\t');
    output.push_str(&entry.catalog_entry.size_bytes.to_string());
    output.push('\t');
    output.push_str(&entry.catalog_entry.modified_unix_millis.to_string());
    output.push('\t');
    output.push_str(&entry.catalog_entry.permissions.to_string());
    output.push('\t');
    push_optional_encoded_field(output, entry.catalog_entry.content_hash.as_deref());
    output.push('\t');
    push_optional_encoded_field(output, entry.symlink_target.as_deref());
    output.push('\t');
    push_encoded_field(output, entry.policy.action.as_str());
    output.push('\t');
    output.push_str(entry.policy.content_sync.as_str());
    output.push('\t');
    push_optional_encoded_field(output, entry.policy.rebuild_hint.map(RebuildHint::as_str));
    output.push('\t');
    push_optional_encoded_field(
        output,
        entry
            .policy
            .platform_pin
            .as_ref()
            .map(|pin| pin.os_family.as_str()),
    );
    output.push('\t');
    push_optional_encoded_field(
        output,
        entry
            .policy
            .platform_pin
            .as_ref()
            .map(|pin| pin.architecture.as_str()),
    );
}

fn parse_event_queue(text: &str) -> Result<EventQueue, WatchError> {
    let mut lines = text.lines();
    let format_line = lines
        .next()
        .ok_or_else(|| WatchError::unsupported("watcher event queue file is empty"))?;
    let format_parts = format_line.split('\t').collect::<Vec<_>>();
    if format_parts.len() != 2 || format_parts[0] != "format" {
        return Err(WatchError::unsupported("watcher event queue format header is invalid"));
    }
    let format_version = decode_field(format_parts[1])?;
    if format_version != WATCHER_EVENT_QUEUE_STORAGE_FORMAT_VERSION {
        return Err(WatchError::unsupported(format!(
            "unsupported watcher event queue format: {format_version}"
        )));
    }

    let next_sequence_line = lines
        .next()
        .ok_or_else(|| WatchError::unsupported("watcher event queue next_sequence header is missing"))?;
    let next_sequence_parts = next_sequence_line.split('\t').collect::<Vec<_>>();
    if next_sequence_parts.len() != 2 || next_sequence_parts[0] != "next_sequence" {
        return Err(WatchError::unsupported(
            "watcher event queue next_sequence header is invalid",
        ));
    }
    let mut next_sequence = parse_u64_field(next_sequence_parts[1], "next_sequence")?;

    let mut events = VecDeque::new();
    for (line_index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let event = parse_event_line(line, line_index + 3)?;
        next_sequence = next_sequence.max(event.sequence.saturating_add(1));
        events.push_back(event);
    }

    Ok(EventQueue {
        events,
        next_sequence,
        storage_path: None,
    })
}

fn parse_event_line(line: &str, line_number: usize) -> Result<FsEvent, WatchError> {
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() != EVENT_QUEUE_LINE_FIELD_COUNT {
        return Err(WatchError::unsupported(format!(
            "watcher event queue line {line_number} has {} fields, expected {EVENT_QUEUE_LINE_FIELD_COUNT}",
            fields.len()
        )));
    }
    if fields[0] != "event" {
        return Err(WatchError::unsupported(format!(
            "watcher event queue line {line_number} is not an event row"
        )));
    }

    let mut index = 1;
    let contract_version = decode_field(fields[index])?;
    index += 1;
    if contract_version != WATCHER_EVENT_QUEUE_CONTRACT_VERSION {
        return Err(WatchError::unsupported(format!(
            "watcher event queue line {line_number} has unsupported event contract: {contract_version}"
        )));
    }
    let sequence = parse_u64_field(fields[index], "event sequence")?;
    index += 1;
    let kind = event_kind_from_str(fields[index])?;
    index += 1;
    let path = decode_field(fields[index])?;
    index += 1;
    let previous_path = decode_optional_field(fields[index])?;
    index += 1;
    let content_sync = content_sync_from_str(fields[index])?;
    index += 1;
    let before = parse_snapshot_entry_fields(&fields, &mut index)?;
    let after = parse_snapshot_entry_fields(&fields, &mut index)?;
    if index != fields.len() {
        return Err(WatchError::unsupported(format!(
            "watcher event queue line {line_number} has trailing fields"
        )));
    }

    let mut event = FsEvent::new(kind, path, previous_path, before, after);
    event.sequence = sequence;
    event.content_sync = content_sync;
    Ok(event)
}

fn parse_snapshot_entry_fields(
    fields: &[&str],
    index: &mut usize,
) -> Result<Option<SnapshotEntry>, WatchError> {
    let present = fields[*index];
    *index += 1;
    if present == "0" {
        *index += EVENT_QUEUE_ENTRY_FIELD_COUNT - 1;
        return Ok(None);
    }
    if present != "1" {
        return Err(WatchError::unsupported("watcher event queue entry marker is invalid"));
    }

    let path = decode_field(fields[*index])?;
    *index += 1;
    let kind = tree_entry_kind_from_str(fields[*index])?;
    *index += 1;
    let size_bytes = parse_u64_field(fields[*index], "entry size_bytes")?;
    *index += 1;
    let modified_unix_millis = parse_u64_field(fields[*index], "entry modified_unix_millis")?;
    *index += 1;
    let permissions = parse_u32_field(fields[*index], "entry permissions")?;
    *index += 1;
    let content_hash = decode_optional_field(fields[*index])?;
    *index += 1;
    let symlink_target = decode_optional_field(fields[*index])?;
    *index += 1;
    let action_name = decode_field(fields[*index])?;
    *index += 1;
    let policy_content_sync = content_sync_from_str(fields[*index])?;
    *index += 1;
    let rebuild_hint = decode_optional_field(fields[*index])?
        .map(|value| rebuild_hint_from_str(&value))
        .transpose()?;
    *index += 1;
    let platform_pin_os = decode_optional_field(fields[*index])?;
    *index += 1;
    let platform_pin_arch = decode_optional_field(fields[*index])?;
    *index += 1;
    let platform_pin = match (platform_pin_os, platform_pin_arch) {
        (Some(os_family), Some(architecture)) => Some(PlatformPin {
            os_family: os_family_from_str(&os_family),
            architecture: architecture_from_str(&architecture),
        }),
        (None, None) => None,
        _ => {
            return Err(WatchError::unsupported(
                "watcher event queue platform pin is missing os or architecture",
            ));
        }
    };
    let action = action_from_str(&action_name, platform_pin.clone())?;
    let catalog_entry = TreeEntry::new(
        path,
        kind,
        size_bytes,
        modified_unix_millis,
        permissions,
        content_hash,
    );

    Ok(Some(
        SnapshotEntry::new(
            catalog_entry,
            PolicyMetadata {
                action,
                content_sync: policy_content_sync,
                rebuild_hint,
                platform_pin,
            },
        )
        .with_symlink_target(symlink_target),
    ))
}

fn push_encoded_field(output: &mut String, value: &str) {
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'.'
            | b'_'
            | b'/'
            | b':'
            | b'-'
            | b'+'
            | b'=' => output.push(char::from(*byte)),
            byte => push_percent_encoded_byte(output, byte),
        }
    }
}

fn push_optional_encoded_field(output: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_encoded_field(output, value),
        None => output.push_str(EVENT_QUEUE_NONE_FIELD),
    }
}

fn decode_optional_field(field: &str) -> Result<Option<String>, WatchError> {
    if field == EVENT_QUEUE_NONE_FIELD {
        Ok(None)
    } else {
        decode_field(field).map(Some)
    }
}

fn decode_field(field: &str) -> Result<String, WatchError> {
    let mut output = Vec::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err(WatchError::unsupported("watcher event queue has truncated percent escape"));
        }
        let high = hex_value(bytes[index + 1])?;
        let low = hex_value(bytes[index + 2])?;
        output.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(output)
        .map_err(|_| WatchError::unsupported("watcher event queue field is not valid UTF-8"))
}

fn hex_value(byte: u8) -> Result<u8, WatchError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(WatchError::unsupported("watcher event queue has invalid percent escape")),
    }
}

fn parse_u64_field(field: &str, name: &str) -> Result<u64, WatchError> {
    field
        .parse::<u64>()
        .map_err(|_| WatchError::unsupported(format!("watcher event queue {name} is invalid")))
}

fn parse_u32_field(field: &str, name: &str) -> Result<u32, WatchError> {
    field
        .parse::<u32>()
        .map_err(|_| WatchError::unsupported(format!("watcher event queue {name} is invalid")))
}

fn event_kind_from_str(value: &str) -> Result<EventKind, WatchError> {
    match value {
        "created" => Ok(EventKind::Created),
        "edited" => Ok(EventKind::Edited),
        "moved" => Ok(EventKind::Moved),
        "deleted" => Ok(EventKind::Deleted),
        "permission-changed" => Ok(EventKind::PermissionChanged),
        "symlink-changed" => Ok(EventKind::SymlinkChanged),
        "policy-changed" => Ok(EventKind::PolicyChanged),
        _ => Err(WatchError::unsupported(format!(
            "unknown watcher event kind: {value}"
        ))),
    }
}

fn content_sync_from_str(value: &str) -> Result<ContentSyncDisposition, WatchError> {
    match value {
        "safe" => Ok(ContentSyncDisposition::Safe),
        "suppressed" => Ok(ContentSyncDisposition::Suppressed),
        _ => Err(WatchError::unsupported(format!(
            "unknown watcher content-sync disposition: {value}"
        ))),
    }
}

fn tree_entry_kind_from_str(value: &str) -> Result<TreeEntryKind, WatchError> {
    match value {
        "directory" => Ok(TreeEntryKind::Directory),
        "file" => Ok(TreeEntryKind::File),
        "symlink" => Ok(TreeEntryKind::Symlink),
        _ => Err(WatchError::unsupported(format!(
            "unknown watcher entry kind: {value}"
        ))),
    }
}

fn rebuild_hint_from_str(value: &str) -> Result<RebuildHint, WatchError> {
    match value {
        "rebuild-locally" => Ok(RebuildHint::RebuildLocally),
        _ => Err(WatchError::unsupported(format!(
            "unknown watcher rebuild hint: {value}"
        ))),
    }
}

fn action_from_str(value: &str, platform_pin: Option<PlatformPin>) -> Result<Action, WatchError> {
    match value {
        "sync" => Ok(Action::Sync),
        "ignore" => Ok(Action::Ignore),
        "rebuild-locally" => Ok(Action::RebuildLocally),
        "platform-pin" => platform_pin.map(Action::PlatformPin).ok_or_else(|| {
            WatchError::unsupported("watcher event queue platform-pin action has no platform pin")
        }),
        _ => Err(WatchError::unsupported(format!(
            "unknown watcher policy action: {value}"
        ))),
    }
}

fn os_family_from_str(value: &str) -> crate::foundation::OsFamily {
    match value {
        "linux" => crate::foundation::OsFamily::Linux,
        "macos" => crate::foundation::OsFamily::Macos,
        "windows" => crate::foundation::OsFamily::Windows,
        other => crate::foundation::OsFamily::Other(other.to_owned()),
    }
}

fn architecture_from_str(value: &str) -> crate::foundation::Architecture {
    match value {
        "x86_64" => crate::foundation::Architecture::X86_64,
        "aarch64" => crate::foundation::Architecture::Aarch64,
        "arm" => crate::foundation::Architecture::Arm,
        other => crate::foundation::Architecture::Other(other.to_owned()),
    }
}

fn watch_io_error(action: &str, path: &Path, error: std::io::Error) -> WatchError {
    WatchError::unsupported(format!("{action} `{}` failed: {error}", path.display()))
}

enum PolicyAwareMoveEvents {
    Single(Box<FsEvent>),
    Split(Box<[FsEvent; 2]>),
}

fn policy_aware_move_events(
    before: SnapshotEntry,
    after: SnapshotEntry,
    source_suppressed_by_current_policy: bool,
) -> PolicyAwareMoveEvents {
    if !source_suppressed_by_current_policy
        && before.allows_content_sync() == after.allows_content_sync()
    {
        return PolicyAwareMoveEvents::Single(Box::new(FsEvent::moved(before, after)));
    }

    let mut delete_source = FsEvent::deleted(before);
    if source_suppressed_by_current_policy {
        delete_source.content_sync = ContentSyncDisposition::Suppressed;
    }
    PolicyAwareMoveEvents::Split(Box::new([delete_source, FsEvent::created(after)]))
}

enum CoalesceResult {
    DropBoth,
    One(Box<FsEvent>),
    Split(Box<FsEvent>, Box<FsEvent>),
    Both(Box<FsEvent>, Box<FsEvent>),
}

fn coalesce_events(first: FsEvent, second: FsEvent) -> CoalesceResult {
    let first_sequence = first.sequence;
    let second_sequence = second.sequence;

    if first.kind == EventKind::Created {
        if second.kind == EventKind::Deleted && first.path == second.path {
            return CoalesceResult::DropBoth;
        }

        if second.kind == EventKind::Moved
            && second.previous_path.as_deref() == Some(first.path.as_str())
        {
            if let Some(after) = second.after.clone() {
                return CoalesceResult::One(Box::new(with_sequence(FsEvent::created(after), first_sequence)));
            }
        }

        if is_update_kind(second.kind) && first.path == second.path {
            if let Some(after) = second.after.clone().or_else(|| first.after.clone()) {
                return CoalesceResult::One(Box::new(with_sequence(FsEvent::created(after), first_sequence)));
            }
        }
    }

    if is_update_kind(first.kind) {
        if is_update_kind(second.kind) && first.path == second.path {
            if let (Some(before), Some(after)) = (first.before.clone(), second.after.clone()) {
                return CoalesceResult::One(Box::new(with_sequence(
                    FsEvent::updated(before, after),
                    first_sequence,
                )));
            }
        }

        if second.kind == EventKind::Deleted && first.path == second.path {
            if let Some(before) = coalesced_delete_before(
                first.before.clone(),
                second.before.clone(),
                second.content_sync,
            ) {
                return CoalesceResult::One(Box::new(with_sequence(
                    deleted_with_content_sync(before, second.content_sync),
                    first_sequence,
                )));
            }
        }
    }

    if first.kind == EventKind::Deleted && second.kind == EventKind::Created && first.path == second.path {
        if let (Some(before), Some(after)) = (first.before.clone(), second.after.clone()) {
            return CoalesceResult::One(Box::new(with_sequence(
                FsEvent::updated(before, after),
                first_sequence,
            )));
        }
    }

    if first.kind == EventKind::Moved {
        if is_update_kind(second.kind) && first.path == second.path {
            if let (Some(before), Some(after)) = (first.before.clone(), second.after.clone()) {
                return match policy_aware_move_events(before, after, false) {
                    PolicyAwareMoveEvents::Single(event) => {
                        CoalesceResult::One(Box::new(with_sequence(*event, first_sequence)))
                    }
                    PolicyAwareMoveEvents::Split(events) => {
                        let [delete_source, create_destination] = *events;
                        CoalesceResult::Split(
                            Box::new(with_sequence(delete_source, first_sequence)),
                            Box::new(with_sequence(create_destination, second_sequence)),
                        )
                    }
                };
            }
        }

        if second.kind == EventKind::Deleted && first.path == second.path {
            if second.content_sync == ContentSyncDisposition::Suppressed {
                if let Some(before) = first.before.clone() {
                    return CoalesceResult::One(Box::new(with_sequence(
                        FsEvent::deleted(before),
                        first_sequence,
                    )));
                }
            }

            if let Some(before) = coalesced_delete_before(
                first.before.clone(),
                second.before.clone(),
                second.content_sync,
            ) {
                return CoalesceResult::One(Box::new(with_sequence(
                    deleted_with_content_sync(before, second.content_sync),
                    first_sequence,
                )));
            }
        }
    }

    CoalesceResult::Both(Box::new(first), Box::new(second))
}

fn events_overlap(left: &FsEvent, right: &FsEvent) -> bool {
    left.path == right.path
        || left.previous_path.as_deref() == Some(right.path.as_str())
        || right.previous_path.as_deref() == Some(left.path.as_str())
        || left
            .previous_path
            .as_ref()
            .zip(right.previous_path.as_ref())
            .is_some_and(|(left, right)| left == right)
}

fn with_sequence(mut event: FsEvent, sequence: u64) -> FsEvent {
    event.sequence = sequence;
    event
}

fn deleted_with_content_sync(
    before: SnapshotEntry,
    content_sync: ContentSyncDisposition,
) -> FsEvent {
    let mut event = FsEvent::deleted(before);
    event.content_sync = content_sync;
    event
}

fn coalesced_delete_before(
    first_before: Option<SnapshotEntry>,
    second_before: Option<SnapshotEntry>,
    content_sync: ContentSyncDisposition,
) -> Option<SnapshotEntry> {
    if content_sync == ContentSyncDisposition::Suppressed {
        second_before.or(first_before)
    } else {
        first_before.or(second_before)
    }
}

fn is_update_kind(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Edited
            | EventKind::PermissionChanged
            | EventKind::SymlinkChanged
            | EventKind::PolicyChanged
    )
}

fn event_from_updated_entries(before: SnapshotEntry, after: SnapshotEntry) -> FsEvent {
    let kind = classify_modified_entry(&before, &after);
    FsEvent::new(kind, after.path().to_owned(), None, Some(before), Some(after))
}

fn classify_modified_entry(before: &SnapshotEntry, after: &SnapshotEntry) -> EventKind {
    if before.catalog_entry == after.catalog_entry && before.policy != after.policy {
        return EventKind::PolicyChanged;
    }

    if (before.catalog_entry.kind == TreeEntryKind::Symlink
        || after.catalog_entry.kind == TreeEntryKind::Symlink)
        && (before.catalog_entry.content_hash != after.catalog_entry.content_hash
            || before.symlink_target != after.symlink_target
            || before.catalog_entry.kind != after.catalog_entry.kind)
    {
        return EventKind::SymlinkChanged;
    }

    if before.catalog_entry.kind == after.catalog_entry.kind
        && before.catalog_entry.size_bytes == after.catalog_entry.size_bytes
        && before.catalog_entry.modified_unix_millis == after.catalog_entry.modified_unix_millis
        && before.catalog_entry.content_hash == after.catalog_entry.content_hash
        && before.catalog_entry.permissions != after.catalog_entry.permissions
    {
        return EventKind::PermissionChanged;
    }

    EventKind::Edited
}

fn event_content_sync(
    kind: EventKind,
    before: Option<&SnapshotEntry>,
    after: Option<&SnapshotEntry>,
) -> ContentSyncDisposition {
    let controlling_entry = match kind {
        EventKind::Deleted => before,
        EventKind::Created
        | EventKind::Edited
        | EventKind::Moved
        | EventKind::PermissionChanged
        | EventKind::SymlinkChanged
        | EventKind::PolicyChanged => after,
    };

    match controlling_entry {
        Some(entry) if entry.allows_content_sync() => ContentSyncDisposition::Safe,
        Some(_) | None => ContentSyncDisposition::Suppressed,
    }
}

fn removed_path_is_suppressed_by_current_policy(
    path: &str,
    current: &IndexedSnapshot,
    after_suppressed_dirs: &[String],
) -> bool {
    current
        .current_policy_for_path(path)
        .map_or_else(
            || is_descendant_or_same_of_any(path, after_suppressed_dirs),
            |policy| !policy.allows_content_sync(),
        )
}

fn normalize_relative_path(path: &str) -> Result<String, WatchError> {
    let path = path.replace('\\', "/");
    if path.starts_with('/') {
        return Err(WatchError::unsupported(
            "watcher paths must be relative to the project root",
        ));
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(WatchError::unsupported(
                        "watcher paths must remain inside the project root",
                    ));
                }
            }
            value if value.contains('\0') => {
                return Err(WatchError::unsupported("watcher paths must not contain NUL bytes"));
            }
            value => segments.push(value.to_owned()),
        }
    }

    if segments.is_empty() {
        return Err(WatchError::unsupported(
            "watcher paths must be relative entries below the project root",
        ));
    }

    Ok(segments.join("/"))
}

fn is_descendant_of_any(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| is_descendant(path, prefix))
}

fn is_descendant_or_same_of_any(path: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|prefix| path == prefix || is_descendant(path, prefix))
}

fn is_descendant(path: &str, prefix: &str) -> bool {
    path.len() > prefix.len()
        && path.starts_with(prefix)
        && path.as_bytes().get(prefix.len()) == Some(&b'/')
}

fn compare_snapshot_entries(left: &SnapshotEntry, right: &SnapshotEntry) -> Ordering {
    compare_tree_entries(&left.catalog_entry, &right.catalog_entry).then_with(|| {
        left.policy
            .action
            .as_str()
            .cmp(right.policy.action.as_str())
            .then_with(|| left.policy.content_sync.as_str().cmp(right.policy.content_sync.as_str()))
            .then_with(|| {
                left.policy
                    .rebuild_hint
                    .map(RebuildHint::as_str)
                    .cmp(&right.policy.rebuild_hint.map(RebuildHint::as_str))
            })
            .then_with(|| left.symlink_target.cmp(&right.symlink_target))
    })
}

fn compare_tree_entries(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.size_bytes.cmp(&right.size_bytes))
        .then_with(|| left.modified_unix_millis.cmp(&right.modified_unix_millis))
        .then_with(|| left.permissions.cmp(&right.permissions))
        .then_with(|| left.content_hash.cmp(&right.content_hash))
}

fn compare_local_tree_entries(left: &LocalTreeEntry, right: &LocalTreeEntry) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.size_bytes.cmp(&right.size_bytes))
        .then_with(|| left.modified_unix_millis.cmp(&right.modified_unix_millis))
        .then_with(|| left.permissions.cmp(&right.permissions))
        .then_with(|| left.content_hash.cmp(&right.content_hash))
        .then_with(|| left.symlink_target.cmp(&right.symlink_target))
}

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

fn catalog_fingerprint(project_id: &str, entries: &[SnapshotEntry]) -> ManifestId {
    let mut hash = FNV_OFFSET;
    hash_str(&mut hash, "catalog-manifest");
    hash_str(&mut hash, project_id);
    for entry in entries {
        hash_tree_entry(&mut hash, &entry.catalog_entry);
    }
    format!("manifest-{hash:016x}")
}

fn snapshot_fingerprint(project_id: &str, entries: &[SnapshotEntry]) -> ManifestId {
    let mut hash = FNV_OFFSET;
    hash_str(&mut hash, "indexed-snapshot");
    hash_str(&mut hash, project_id);
    for entry in entries {
        hash_tree_entry(&mut hash, &entry.catalog_entry);
        hash_policy_metadata(&mut hash, &entry.policy);
        hash_str(&mut hash, entry.symlink_target.as_deref().unwrap_or("-"));
    }
    format!("snapshot-{hash:016x}")
}

fn stable_hash_string(prefix: &str, value: &str) -> String {
    let mut hash = FNV_OFFSET;
    hash_str(&mut hash, prefix);
    hash_str(&mut hash, value);
    format!("{prefix}:{hash:016x}")
}

fn hash_tree_entry(hash: &mut u64, entry: &TreeEntry) {
    hash_str(hash, &entry.path);
    hash_str(hash, entry.kind.as_str());
    hash_str(hash, &entry.size_bytes.to_string());
    hash_str(hash, &entry.modified_unix_millis.to_string());
    hash_str(hash, &entry.permissions.to_string());
    hash_str(hash, entry.content_hash.as_deref().unwrap_or("-"));
}

fn hash_policy_metadata(hash: &mut u64, policy: &PolicyMetadata) {
    hash_str(hash, policy.action.as_str());
    hash_str(hash, policy.content_sync.as_str());
    hash_str(
        hash,
        policy
            .rebuild_hint
            .map(RebuildHint::as_str)
            .unwrap_or("-"),
    );
    if let Some(pin) = &policy.platform_pin {
        hash_str(hash, pin.os_family.as_str());
        hash_str(hash, pin.architecture.as_str());
    } else {
        hash_str(hash, "-");
    }
}

fn hash_str(hash: &mut u64, value: &str) {
    for byte in value.as_bytes().iter().copied().chain([0xff]) {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn push_percent_encoded_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(char::from(HEX[usize::from(byte >> 4)]));
    output.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::{
        Architecture, BASELINE_SCHEMA_VERSION, InMemoryMigrationStore, MachineId,
        MachineIdProvenance, OsFamily, PlatformCapabilities,
    };
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH as TEST_UNIX_EPOCH};

    #[test]
    fn create_edit_move_delete_events_are_deduped() {
        let empty = snapshot(vec![], Policy::new());
        let created = snapshot(vec![file("src/main.rs", 10, 10, "sha256:v1")], Policy::new());
        let edited = snapshot(vec![file("src/main.rs", 11, 11, "sha256:v2")], Policy::new());
        let moved = snapshot(vec![file("src/lib.rs", 11, 11, "sha256:v2")], Policy::new());
        let deleted = snapshot(vec![], Policy::new());

        let create_queue = EventQueue::from_snapshots(Some(&empty), &created);
        assert_eq!(single_kind(&create_queue), EventKind::Created);

        let edit_queue = EventQueue::from_snapshots(Some(&created), &edited);
        assert_eq!(single_kind(&edit_queue), EventKind::Edited);

        let move_queue = EventQueue::from_snapshots(Some(&edited), &moved);
        let move_event = move_queue.peek().unwrap();
        assert_eq!(move_event.kind, EventKind::Moved);
        assert_eq!(move_event.previous_path.as_deref(), Some("src/main.rs"));
        assert_eq!(move_event.path, "src/lib.rs");

        let delete_queue = EventQueue::from_snapshots(Some(&moved), &deleted);
        assert_eq!(single_kind(&delete_queue), EventKind::Deleted);

        let v1 = created.entry("src/main.rs").unwrap().clone();
        let v2 = edited.entry("src/main.rs").unwrap().clone();
        let mut queue = EventQueue::new();
        queue.enqueue(FsEvent::created(v1.clone()));
        queue.enqueue(FsEvent::updated(v1, v2.clone()));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.peek().unwrap().kind, EventKind::Created);
        assert_eq!(
            queue
                .peek()
                .unwrap()
                .after
                .as_ref()
                .unwrap()
                .catalog_entry
                .content_hash
                .as_deref(),
            Some("sha256:v2")
        );

        queue.enqueue(FsEvent::deleted(v2));
        assert!(queue.is_empty());
    }

    #[test]
    fn coalesces_repeated_overlaps_until_stable() {
        let a_v1 = snapshot(vec![file("a.txt", 10, 10, "sha256:a-v1")], Policy::new())
            .entry("a.txt")
            .unwrap()
            .clone();
        let b_v1 = snapshot(vec![file("b.txt", 10, 10, "sha256:b-v1")], Policy::new())
            .entry("b.txt")
            .unwrap()
            .clone();
        let b_v2 = snapshot(vec![file("b.txt", 11, 11, "sha256:b-v2")], Policy::new())
            .entry("b.txt")
            .unwrap()
            .clone();

        let mut queue = EventQueue::new();
        queue.enqueue(FsEvent::created(a_v1.clone()));
        queue.enqueue(FsEvent::updated(b_v1.clone(), b_v2.clone()));
        queue.enqueue(FsEvent::moved(a_v1, b_v1));

        assert_eq!(queue.len(), 1);
        let event = queue.peek().unwrap();
        assert_eq!(event.kind, EventKind::Created);
        assert_eq!(event.path, "b.txt");
        assert_eq!(event.sequence, 1);
        assert_eq!(
            event
                .after
                .as_ref()
                .unwrap()
                .catalog_entry
                .content_hash
                .as_deref(),
            Some("sha256:b-v2")
        );
    }

    #[test]
    fn ignored_git_and_platform_specific_events_preserve_policy_metadata() {
        let linux = linux();
        let policy = Policy::new();
        let indexer = LocalIndexer::new("project", policy, linux.clone());
        let tree = InMemoryTree::new(vec![
            LocalTreeEntry::directory(".git", 1, 0o700),
            file(".git/config", 1, 1, "sha256:git-config"),
            file(".gitmodules", 1, 1, "sha256:gitmodules"),
            file("dist/app.js", 2, 2, "sha256:dist"),
            file("native/addon.so", 3, 3, "sha256:native"),
            file("src/lib.rs", 4, 4, "sha256:src"),
        ]);

        let snapshot = indexer.index_tree(&tree).unwrap();

        assert!(snapshot.entry(".git/config").is_none());
        assert_action(&snapshot, ".git", "ignore");
        assert_action(&snapshot, ".gitmodules", "ignore");
        assert_action(&snapshot, "dist/app.js", "ignore");
        assert_action(&snapshot, "src/lib.rs", "sync");

        let native = snapshot.entry("native/addon.so").unwrap();
        assert_eq!(native.policy.action.as_str(), "platform-pin");
        assert_eq!(native.policy.content_sync, ContentSyncDisposition::Suppressed);
        assert!(native.policy.platform_pin.as_ref().unwrap().matches(&linux));

        let queue = EventQueue::from_snapshots(None, &snapshot);
        assert_eq!(
            queue
                .content_sync_events()
                .map(|event| event.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );
        assert!(queue.iter().any(|event| {
            event.path == ".gitmodules"
                && event.policy_metadata().unwrap().action.as_str() == "ignore"
                && !event.allows_content_sync()
        }));
        assert!(queue.iter().any(|event| {
            event.path == "native/addon.so"
                && event.policy_metadata().unwrap().action.as_str() == "platform-pin"
                && !event.allows_content_sync()
        }));
    }

    #[test]
    fn policy_transitions_suppress_ignored_deletes_and_allow_newly_syncable_content() {
        let sync_entries = vec![
            dir("src", 1, 0o755),
            file("src/main.rs", 10, 10, "sha256:src"),
        ];
        let before_sync = snapshot(sync_entries.clone(), Policy::new());
        let ignored_policy = Policy::from_syncignore("src/\n", "").unwrap();
        let after_ignored = snapshot(sync_entries, ignored_policy.clone());

        let policy_change_queue = EventQueue::from_snapshots(Some(&before_sync), &after_ignored);
        let ignored_file_event = policy_change_queue
            .iter()
            .find(|event| event.path == "src/main.rs")
            .unwrap();
        assert_eq!(ignored_file_event.kind, EventKind::PolicyChanged);
        assert_eq!(ignored_file_event.content_sync, ContentSyncDisposition::Suppressed);

        let pruned_ignored = snapshot(vec![dir("src", 1, 0o755)], ignored_policy);
        let delete_queue = EventQueue::from_snapshots(Some(&before_sync), &pruned_ignored);
        let suppressed_delete = delete_queue
            .iter()
            .find(|event| event.path == "src/main.rs")
            .unwrap();
        assert_eq!(suppressed_delete.kind, EventKind::Deleted);
        assert_eq!(suppressed_delete.content_sync, ContentSyncDisposition::Suppressed);

        let dist_entries = vec![
            dir("dist", 1, 0o755),
            file("dist/keep.js", 2, 2, "sha256:keep"),
        ];
        let ignored = snapshot(
            dist_entries.clone(),
            Policy::from_syncignore("dist/\n", "").unwrap(),
        );
        let re_included = snapshot(
            dist_entries,
            Policy::from_syncignore("dist/\n!dist/keep.js\n", "").unwrap(),
        );
        let reinclude_queue = EventQueue::from_snapshots(Some(&ignored), &re_included);
        let keep_event = reinclude_queue
            .iter()
            .find(|event| event.path == "dist/keep.js")
            .unwrap();
        assert_eq!(keep_event.kind, EventKind::PolicyChanged);
        assert_eq!(keep_event.content_sync, ContentSyncDisposition::Safe);
        assert!(keep_event.allows_content_sync());

        let reinclude_after_delete = snapshot(
            vec![dir("dist", 1, 0o755)],
            Policy::from_syncignore("dist/\n!dist/keep.js\n", "").unwrap(),
        );
        let delete_reincluded_queue =
            EventQueue::from_snapshots(Some(&re_included), &reinclude_after_delete);
        let delete_reincluded = delete_reincluded_queue
            .iter()
            .find(|event| event.path == "dist/keep.js")
            .unwrap();
        assert_eq!(delete_reincluded.kind, EventKind::Deleted);
        assert_eq!(delete_reincluded.content_sync, ContentSyncDisposition::Safe);

        let pruned_dist = snapshot(
            vec![dir("dist", 1, 0o755)],
            Policy::from_syncignore("dist/\n", "").unwrap(),
        );
        let newly_syncable_dist = snapshot(
            vec![
                dir("dist", 1, 0o755),
                file("dist/app.js", 3, 3, "sha256:app"),
            ],
            Policy::from_syncignore("!dist/\n", "").unwrap(),
        );
        let newly_syncable_queue =
            EventQueue::from_snapshots(Some(&pruned_dist), &newly_syncable_dist);
        let created_sync_child = newly_syncable_queue
            .iter()
            .find(|event| event.path == "dist/app.js")
            .unwrap();
        assert_eq!(created_sync_child.kind, EventKind::Created);
        assert_eq!(created_sync_child.content_sync, ContentSyncDisposition::Safe);
    }

    #[test]
    fn policy_boundary_moves_emit_only_safe_content_side() {
        let before_safe = snapshot(vec![file("src/shared.txt", 10, 10, "sha256:shared")], Policy::new());
        let after_suppressed = snapshot(
            vec![file("dist/shared.txt", 10, 10, "sha256:shared")],
            Policy::from_syncignore("dist/\n", "").unwrap(),
        );
        let safe_to_suppressed = EventQueue::from_snapshots(Some(&before_safe), &after_suppressed);

        assert_eq!(safe_to_suppressed.len(), 2);
        assert!(!safe_to_suppressed
            .iter()
            .any(|event| event.kind == EventKind::Moved));
        let delete_source = safe_to_suppressed
            .iter()
            .find(|event| event.kind == EventKind::Deleted && event.path == "src/shared.txt")
            .unwrap();
        assert_eq!(delete_source.content_sync, ContentSyncDisposition::Safe);
        let create_suppressed = safe_to_suppressed
            .iter()
            .find(|event| event.kind == EventKind::Created && event.path == "dist/shared.txt")
            .unwrap();
        assert_eq!(create_suppressed.content_sync, ContentSyncDisposition::Suppressed);
        assert_eq!(
            safe_to_suppressed
                .content_sync_events()
                .map(|event| (event.kind, event.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(EventKind::Deleted, "src/shared.txt")]
        );

        let before_suppressed = snapshot(
            vec![file("dist/shared.txt", 10, 10, "sha256:shared")],
            Policy::from_syncignore("dist/\n", "").unwrap(),
        );
        let after_safe = snapshot(vec![file("src/shared.txt", 10, 10, "sha256:shared")], Policy::new());
        let suppressed_to_safe = EventQueue::from_snapshots(Some(&before_suppressed), &after_safe);

        assert_eq!(suppressed_to_safe.len(), 2);
        assert!(!suppressed_to_safe
            .iter()
            .any(|event| event.kind == EventKind::Moved));
        let delete_suppressed = suppressed_to_safe
            .iter()
            .find(|event| event.kind == EventKind::Deleted && event.path == "dist/shared.txt")
            .unwrap();
        assert_eq!(delete_suppressed.content_sync, ContentSyncDisposition::Suppressed);
        let create_destination = suppressed_to_safe
            .iter()
            .find(|event| event.kind == EventKind::Created && event.path == "src/shared.txt")
            .unwrap();
        assert_eq!(create_destination.content_sync, ContentSyncDisposition::Safe);
        assert_eq!(
            suppressed_to_safe
                .content_sync_events()
                .map(|event| (event.kind, event.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(EventKind::Created, "src/shared.txt")]
        );
    }

    #[test]
    fn removing_reinclude_suppresses_existing_descendant_delete() {
        let dist_entries = vec![
            dir("dist", 1, 0o755),
            file("dist/keep.js", 2, 2, "sha256:keep"),
        ];
        let before = snapshot(
            dist_entries,
            Policy::from_syncignore("dist/\n!dist/keep.js\n", "").unwrap(),
        );
        let after_pruned_by_removed_negation = snapshot(
            vec![dir("dist", 1, 0o755)],
            Policy::from_syncignore("dist/\n", "").unwrap(),
        );

        let queue = EventQueue::from_snapshots(Some(&before), &after_pruned_by_removed_negation);
        let keep_delete = queue
            .iter()
            .find(|event| event.kind == EventKind::Deleted && event.path == "dist/keep.js")
            .unwrap();

        assert_eq!(keep_delete.content_sync, ContentSyncDisposition::Suppressed);
        assert!(queue
            .content_sync_events()
            .all(|event| event.path != "dist/keep.js"));
    }

    #[test]
    fn move_from_removed_reinclude_suppresses_source_delete_under_current_policy() {
        let before = snapshot(
            vec![
                dir("dist", 1, 0o755),
                file("dist/keep.js", 2, 2, "sha256:keep"),
            ],
            Policy::from_syncignore("dist/\n!dist/keep.js\n", "").unwrap(),
        );
        let after = snapshot(
            vec![
                dir("dist", 1, 0o755),
                file("src/keep.js", 2, 2, "sha256:keep"),
            ],
            Policy::from_syncignore("dist/\n", "").unwrap(),
        );

        let queue = EventQueue::from_snapshots(Some(&before), &after);

        assert!(!queue.iter().any(|event| event.kind == EventKind::Moved));
        let source_delete = queue
            .iter()
            .find(|event| event.kind == EventKind::Deleted && event.path == "dist/keep.js")
            .unwrap();
        assert_eq!(source_delete.content_sync, ContentSyncDisposition::Suppressed);
        let destination_create = queue
            .iter()
            .find(|event| event.kind == EventKind::Created && event.path == "src/keep.js")
            .unwrap();
        assert_eq!(destination_create.content_sync, ContentSyncDisposition::Safe);
        assert_eq!(
            queue
                .content_sync_events()
                .map(|event| (event.kind, event.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(EventKind::Created, "src/keep.js")]
        );
    }

    #[test]
    fn coalescing_preserves_policy_suppressed_delete_disposition() {
        let safe = snapshot(vec![file("src/main.rs", 10, 10, "sha256:src")], Policy::new())
            .entry("src/main.rs")
            .unwrap()
            .clone();
        let ignored = snapshot(
            vec![file("src/main.rs", 10, 10, "sha256:src")],
            Policy::from_syncignore("src/\n", "").unwrap(),
        )
        .entry("src/main.rs")
        .unwrap()
        .clone();

        let mut queue = EventQueue::new();
        queue.enqueue(FsEvent::updated(safe, ignored.clone()));
        queue.enqueue(FsEvent::deleted(ignored));

        assert_eq!(queue.len(), 1);
        let event = queue.peek().unwrap();
        assert_eq!(event.kind, EventKind::Deleted);
        assert_eq!(event.content_sync, ContentSyncDisposition::Suppressed);
        assert_eq!(
            event.before.as_ref().unwrap().policy.content_sync,
            ContentSyncDisposition::Suppressed
        );
    }

    #[test]
    fn coalesced_move_policy_change_reuses_policy_boundary_split() {
        let before = snapshot(vec![file("src/shared.txt", 10, 10, "sha256:shared")], Policy::new())
            .entry("src/shared.txt")
            .unwrap()
            .clone();
        let moved_safe = snapshot(
            vec![file("dist/shared.txt", 10, 10, "sha256:shared")],
            Policy::new(),
        )
        .entry("dist/shared.txt")
        .unwrap()
        .clone();
        let moved_suppressed = snapshot(
            vec![file("dist/shared.txt", 10, 10, "sha256:shared")],
            Policy::from_syncignore("dist/\n", "").unwrap(),
        )
        .entry("dist/shared.txt")
        .unwrap()
        .clone();

        let mut queue = EventQueue::new();
        queue.enqueue(FsEvent::moved(before, moved_safe.clone()));
        queue.enqueue(FsEvent::updated(moved_safe, moved_suppressed));

        assert_eq!(queue.len(), 2);
        assert!(!queue.iter().any(|event| event.kind == EventKind::Moved));
        let source_delete = queue
            .iter()
            .find(|event| event.kind == EventKind::Deleted && event.path == "src/shared.txt")
            .unwrap();
        assert_eq!(source_delete.content_sync, ContentSyncDisposition::Safe);
        assert_eq!(source_delete.sequence, 1);
        let destination_create = queue
            .iter()
            .find(|event| event.kind == EventKind::Created && event.path == "dist/shared.txt")
            .unwrap();
        assert_eq!(
            destination_create.content_sync,
            ContentSyncDisposition::Suppressed
        );
        assert_eq!(destination_create.sequence, 2);
        assert_eq!(
            queue
                .content_sync_events()
                .map(|event| (event.kind, event.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(EventKind::Deleted, "src/shared.txt")]
        );
    }

    #[test]
    fn coalesced_move_then_policy_suppressed_delete_keeps_source_content_delete() {
        let before = snapshot(
            vec![file("src/original.txt", 10, 10, "sha256:payload")],
            Policy::new(),
        );
        let moved = snapshot(
            vec![file("ignored/original.txt", 10, 10, "sha256:payload")],
            Policy::new(),
        );
        let after_destination_deleted_under_ignored_policy = snapshot(
            vec![],
            Policy::from_syncignore("ignored/\n", "").unwrap(),
        );

        let suppressed_destination_delete = EventQueue::from_snapshots(
            Some(&moved),
            &after_destination_deleted_under_ignored_policy,
        );
        let destination_delete = suppressed_destination_delete
            .iter()
            .find(|event| event.kind == EventKind::Deleted && event.path == "ignored/original.txt")
            .unwrap();
        assert_eq!(
            destination_delete.content_sync,
            ContentSyncDisposition::Suppressed
        );

        let mut queue = EventQueue::new();
        queue.extend_snapshot_diff(Some(&before), &moved);
        queue.extend_snapshot_diff(Some(&moved), &after_destination_deleted_under_ignored_policy);

        assert_eq!(
            queue
                .content_sync_events()
                .map(|event| (event.kind, event.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(EventKind::Deleted, "src/original.txt")]
        );
        assert!(queue.iter().all(|event| {
            event.path != "ignored/original.txt" || !event.allows_content_sync()
        }));
    }

    #[test]
    fn every_policy_action_variant_is_preserved_in_snapshot() {
        let snapshot = snapshot(
            vec![
                file("src/lib.rs", 1, 1, "sha256:sync"),
                file("dist/out.js", 1, 1, "sha256:ignored"),
                file("app/node_modules/pkg/index.js", 1, 1, "sha256:rebuild"),
                file("native/libaddon.so", 1, 1, "sha256:pinned"),
            ],
            Policy::new(),
        );

        let sync = snapshot.entry("src/lib.rs").unwrap();
        assert_eq!(sync.policy.action, Action::Sync);
        assert_eq!(sync.policy.content_sync, ContentSyncDisposition::Safe);

        let ignored = snapshot.entry("dist/out.js").unwrap();
        assert_eq!(ignored.policy.action, Action::Ignore);
        assert_eq!(ignored.policy.content_sync, ContentSyncDisposition::Suppressed);

        let rebuild = snapshot.entry("app/node_modules/pkg/index.js").unwrap();
        assert_eq!(rebuild.policy.action, Action::RebuildLocally);
        assert_eq!(rebuild.policy.rebuild_hint, Some(RebuildHint::RebuildLocally));
        assert_eq!(rebuild.policy.content_sync, ContentSyncDisposition::Suppressed);

        let pinned = snapshot.entry("native/libaddon.so").unwrap();
        assert_eq!(pinned.policy.action.as_str(), "platform-pin");
        assert!(pinned.policy.platform_pin.is_some());
        assert_eq!(pinned.policy.content_sync, ContentSyncDisposition::Suppressed);
    }

    #[test]
    fn rapid_bulk_changes_converge_to_stable_snapshot_and_deduped_queue() {
        let before = snapshot(
            (0..20)
                .map(|index| file(format!("src/file-{index}.txt"), 10, 10, &format!("sha256:old-{index}")))
                .collect(),
            Policy::new(),
        );
        let after_entries = (5..25)
            .map(|index| {
                let modified = index < 10;
                let hash = if modified {
                    format!("sha256:new-{index}")
                } else {
                    format!("sha256:old-{index}")
                };
                let size = 10 + (if modified { 1 } else { 0 });
                let modified_unix_millis = if modified { 20 } else { 10 };
                file(format!("src/file-{index}.txt"), size, modified_unix_millis, &hash)
            })
            .collect::<Vec<_>>();
        let mut reversed_after_entries = after_entries.clone();
        reversed_after_entries.reverse();

        let after = snapshot(after_entries, Policy::new());
        let after_reversed = snapshot(reversed_after_entries, Policy::new());

        assert_eq!(after.id, after_reversed.id);
        assert_eq!(
            after.manifest.serialize_deterministic(),
            after_reversed.manifest.serialize_deterministic()
        );

        let queue = EventQueue::from_snapshots(Some(&before), &after);
        let mut seen_paths = BTreeSet::new();
        for event in queue.iter() {
            assert!(seen_paths.insert((event.previous_path.clone(), event.path.clone())));
        }
        assert_eq!(queue.len(), 15);
        assert!(after.entry("src/file-0.txt").is_none());
        assert!(after.entry("src/file-24.txt").is_some());
    }

    #[test]
    fn renames_deletes_permissions_and_symlinks_are_first_class_events() {
        let before = snapshot(
            vec![
                file("docs/old.md", 7, 70, "sha256:moved"),
                file("tmp/delete.me", 3, 30, "sha256:delete"),
                LocalTreeEntry::file("bin/tool", 9, 90, 0o644, Some("sha256:tool".to_owned())),
                LocalTreeEntry::symlink("links/current", "targets/a", 5, 0o777),
                LocalTreeEntry::symlink("ignored-link", "dist", 5, 0o777),
                file("ignored-link/nested.txt", 1, 1, "sha256:nested"),
            ],
            Policy::new(),
        );
        let after = snapshot(
            vec![
                file("docs/new.md", 7, 70, "sha256:moved"),
                LocalTreeEntry::file("bin/tool", 9, 90, 0o600, Some("sha256:tool".to_owned())),
                LocalTreeEntry::symlink("links/current", "targets/b", 5, 0o777),
                LocalTreeEntry::symlink("ignored-link", "dist", 5, 0o777),
                file("ignored-link/nested.txt", 1, 1, "sha256:nested"),
            ],
            Policy::new(),
        );

        assert!(after.entry("ignored-link").is_some());
        assert!(after.entry("ignored-link/nested.txt").is_none());

        let queue = EventQueue::from_snapshots(Some(&before), &after);
        assert!(queue.iter().any(|event| {
            event.kind == EventKind::Moved
                && event.previous_path.as_deref() == Some("docs/old.md")
                && event.path == "docs/new.md"
        }));
        assert!(queue
            .iter()
            .any(|event| event.kind == EventKind::Deleted && event.path == "tmp/delete.me"));
        assert!(queue
            .iter()
            .any(|event| event.kind == EventKind::PermissionChanged && event.path == "bin/tool"));
        assert!(queue
            .iter()
            .any(|event| event.kind == EventKind::SymlinkChanged && event.path == "links/current"));
        assert_eq!(queue.len(), 4);
    }

    #[test]
    fn snapshot_producer_and_event_queue_contract_are_public_and_stable() {
        let indexer = LocalIndexer::new("project", Policy::new(), linux());
        let tree = InMemoryTree::new(vec![file("src/lib.rs", 4, 4, "sha256:src")]);
        let producer = indexer.producer(&tree);

        let snapshot = producer.produce_snapshot().unwrap();

        assert_eq!(snapshot.contract_version, WATCHER_SNAPSHOT_CONTRACT_VERSION);
        assert_eq!(snapshot.project_id, "project");
        assert_eq!(snapshot.manifest.project_id, "project");
        assert_eq!(snapshot.content_sync_entries().count(), 1);
        assert_eq!(EventKind::Created.as_str(), "created");
        assert_eq!(EventKind::PermissionChanged.as_str(), "permission-changed");
        assert_eq!(ContentSyncDisposition::Suppressed.as_str(), "suppressed");

        let mut queue = EventQueue::from_snapshots(None, &snapshot);
        assert_eq!(queue.contract_version(), WATCHER_EVENT_QUEUE_CONTRACT_VERSION);
        assert_eq!(queue.peek().unwrap().sequence, 1);
        assert_eq!(queue.peek().unwrap().contract_version, WATCHER_EVENT_QUEUE_CONTRACT_VERSION);

        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert!(queue.is_empty());
    }

    #[test]
    fn polling_adapter_walks_real_tree_deterministically_and_detects_changes() {
        let project = TempProject::new("polling-walk");
        project.write_file("src/main.rs", b"fn main() {}\n");
        project.write_file(".git/config", b"[core]\n");
        project.write_file("dist/app.js", b"ignored build output\n");
        project.write_file("native/addon.so", b"native\n");
        project.write_file("bin/tool", b"tool\n");
        project.set_mode("bin/tool", 0o600);

        let indexer = LocalIndexer::new("project", Policy::new(), linux());
        let snapshot = indexer
            .polling_adapter(project.root().to_path_buf())
            .produce_snapshot()
            .unwrap();
        let snapshot_again = indexer.index_project_root(project.root()).unwrap();

        assert_eq!(
            snapshot.manifest.serialize_deterministic(),
            snapshot_again.manifest.serialize_deterministic()
        );
        assert!(snapshot.entry("src/main.rs").is_some());
        assert!(snapshot.entry(".git").is_some());
        assert!(snapshot.entry(".git/config").is_none());
        assert_action(&snapshot, "dist", "ignore");
        assert!(snapshot.entry("dist/app.js").is_none());
        assert_action(&snapshot, "native/addon.so", "platform-pin");
        #[cfg(unix)]
        assert_eq!(
            snapshot.entry("bin/tool").unwrap().catalog_entry.permissions & 0o777,
            0o600
        );

        project.write_file("src/main.rs", b"fn main() { println!(\"v2\"); }\n");
        let edited = indexer.index_project_root(project.root()).unwrap();
        let edit_queue = EventQueue::from_snapshots(Some(&snapshot), &edited);
        assert!(edit_queue
            .iter()
            .any(|event| event.kind == EventKind::Edited && event.path == "src/main.rs"));

        project.rename("src/main.rs", "src/lib.rs");
        let moved = indexer.index_project_root(project.root()).unwrap();
        let move_queue = EventQueue::from_snapshots(Some(&edited), &moved);
        assert!(move_queue.iter().any(|event| {
            event.kind == EventKind::Moved
                && event.previous_path.as_deref() == Some("src/main.rs")
                && event.path == "src/lib.rs"
        }));

        project.remove_file("src/lib.rs");
        let deleted = indexer.index_project_root(project.root()).unwrap();
        let delete_queue = EventQueue::from_snapshots(Some(&moved), &deleted);
        assert!(delete_queue
            .iter()
            .any(|event| event.kind == EventKind::Deleted && event.path == "src/lib.rs"));
    }

    #[test]
    fn polling_adapter_honors_syncignore_negation_under_ignored_parent() {
        let project = TempProject::new("polling-negation");
        project.write_file("dist/drop.js", b"drop\n");
        project.write_file("dist/pkg/drop.js", b"drop nested\n");
        project.write_file("dist/pkg/keep.js", b"keep nested\n");
        project.write_file("build/drop.js", b"build drop\n");
        project.write_file("build/nested/sub/keep.js", b"unanchored keep\n");
        let policy = Policy::from_syncignore(
            "dist/\nbuild/\n!dist/**/keep.js\n!sub/keep.js\n",
            "",
        )
        .unwrap();
        let indexer = LocalIndexer::new("project", policy, linux());

        let snapshot = indexer.index_project_root(project.root()).unwrap();

        assert_action(&snapshot, "dist", "ignore");
        assert_action(&snapshot, "dist/drop.js", "ignore");
        assert_action(&snapshot, "dist/pkg", "ignore");
        assert_action(&snapshot, "dist/pkg/drop.js", "ignore");
        assert_action(&snapshot, "dist/pkg/keep.js", "sync");
        assert_action(&snapshot, "build", "ignore");
        assert_action(&snapshot, "build/drop.js", "ignore");
        assert_action(&snapshot, "build/nested/sub/keep.js", "sync");
    }

    #[cfg(unix)]
    #[test]
    fn polling_adapter_records_symlinks_and_never_follows_them() {
        let project = TempProject::new("polling-symlink");
        project.write_file("target/file.txt", b"target\n");
        std::os::unix::fs::symlink("target", project.path("link-to-target")).unwrap();
        let indexer = LocalIndexer::new("project", Policy::new(), linux());

        let entries = collect_project_root_entries(project.root(), &indexer).unwrap();
        let link = entries
            .iter()
            .find(|entry| entry.path == "link-to-target")
            .unwrap();
        assert_eq!(link.kind, LocalEntryKind::Symlink);
        assert_eq!(link.symlink_target.as_deref(), Some("target"));

        let snapshot = indexer.index_project_root(project.root()).unwrap();
        assert_eq!(
            snapshot.entry("link-to-target").unwrap().catalog_entry.kind,
            TreeEntryKind::Symlink
        );
        assert_eq!(
            snapshot
                .entry("link-to-target")
                .unwrap()
                .symlink_target
                .as_deref(),
            Some("target")
        );
        assert!(snapshot.entry("link-to-target/file.txt").is_none());
    }

    #[test]
    fn durable_event_queue_survives_reload_with_full_policy_payload() {
        let project = TempProject::new("durable-queue");
        let queue_path = project.path("watcher_events.queue");
        let before = snapshot(
            vec![file("native/addon.so", 6, 60, "sha256:native")],
            Policy::new(),
        );
        let after = snapshot(
            vec![file("native/addon-renamed.so", 6, 60, "sha256:native")],
            Policy::new(),
        );
        let before_entry = before.entry("native/addon.so").unwrap().clone();
        let after_entry = after.entry("native/addon-renamed.so").unwrap().clone();

        let mut queue = EventQueue::open(&queue_path).unwrap();
        queue
            .try_enqueue(FsEvent::moved(before_entry.clone(), after_entry.clone()))
            .unwrap();

        let encoded = std::fs::read_to_string(&queue_path).unwrap();
        assert!(encoded.starts_with("format\twatcher-event-queue-lines-v1\n"));
        assert!(encoded.contains("platform-pin"));

        let mut reloaded = EventQueue::load_from_path(&queue_path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.next_sequence(), 2);
        let event = reloaded.peek().unwrap();
        assert_eq!(event.kind, EventKind::Moved);
        assert_eq!(event.previous_path.as_deref(), Some("native/addon.so"));
        assert_eq!(event.path, "native/addon-renamed.so");
        assert_eq!(event.before.as_ref(), Some(&before_entry));
        assert_eq!(event.after.as_ref(), Some(&after_entry));
        assert_eq!(event.content_sync, ContentSyncDisposition::Suppressed);
        assert!(event.before.as_ref().unwrap().policy.platform_pin.is_some());
        assert!(event.after.as_ref().unwrap().policy.platform_pin.is_some());

        assert!(reloaded.try_pop_front().unwrap().is_some());
        let empty = EventQueue::load_from_path(&queue_path).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn try_enqueue_flush_failure_leaves_queue_and_sequence_unchanged() {
        let project = TempProject::new("durable-try-enqueue-atomic");
        let blocked_parent = project.path("not-a-directory");
        std::fs::write(&blocked_parent, "blocking file").unwrap();
        let queue_path = blocked_parent.join("watcher_events.queue");
        let entry = snapshot(vec![file("src/main.rs", 10, 10, "sha256:src")], Policy::new())
            .entry("src/main.rs")
            .unwrap()
            .clone();
        let mut queue = EventQueue::new();
        queue.enqueue(FsEvent::created(entry.clone()));
        queue.storage_path = Some(queue_path.clone());

        let failed = queue.try_enqueue(FsEvent::deleted(entry));

        assert!(failed.is_err());
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.next_sequence(), 2);
        let pending = queue.peek().unwrap();
        assert_eq!(pending.kind, EventKind::Created);
        assert_eq!(pending.path, "src/main.rs");
        assert_eq!(pending.sequence, 1);

        std::fs::remove_file(&blocked_parent).unwrap();
        queue.flush().unwrap();
        let reloaded = EventQueue::load_from_path(&queue_path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.next_sequence(), 2);
        let persisted = reloaded.peek().unwrap();
        assert_eq!(persisted.kind, EventKind::Created);
        assert_eq!(persisted.path, "src/main.rs");
        assert_eq!(persisted.sequence, 1);
    }

    #[test]
    fn durable_event_queue_survives_reload_with_symlink_target_payload() {
        let project = TempProject::new("durable-symlink-queue");
        let queue_path = project.path("watcher_events.queue");
        let before = snapshot(
            vec![LocalTreeEntry::symlink("links/current", "targets/a", 5, 0o777)],
            Policy::new(),
        );
        let after = snapshot(
            vec![LocalTreeEntry::symlink("links/current", "targets/b", 6, 0o777)],
            Policy::new(),
        );
        let before_entry = before.entry("links/current").unwrap().clone();
        let after_entry = after.entry("links/current").unwrap().clone();

        let mut queue = EventQueue::open(&queue_path).unwrap();
        queue
            .try_enqueue(FsEvent::updated(before_entry.clone(), after_entry.clone()))
            .unwrap();

        let encoded = std::fs::read_to_string(&queue_path).unwrap();
        assert!(encoded.contains("targets/a"));
        assert!(encoded.contains("targets/b"));

        let reloaded = EventQueue::load_from_path(&queue_path).unwrap();
        let event = reloaded.peek().unwrap();
        assert_eq!(event.kind, EventKind::SymlinkChanged);
        assert_eq!(event.before.as_ref(), Some(&before_entry));
        assert_eq!(event.after.as_ref(), Some(&after_entry));
        assert_eq!(
            event.before.as_ref().unwrap().symlink_target.as_deref(),
            Some("targets/a")
        );
        assert_eq!(
            event.after.as_ref().unwrap().symlink_target.as_deref(),
            Some("targets/b")
        );
    }

    #[test]
    fn watcher_events_migration_applies_and_rolls_back_metadata() {
        let runner = watcher_events_migration_runner().unwrap();

        assert_eq!(runner.migrations()[1], watcher_events_initial_migration());

        let mut store = InMemoryMigrationStore::new();
        let applied = runner.apply(&mut store).unwrap();
        let expected_tables = WATCHER_EVENTS_MIGRATION_TABLES
            .iter()
            .map(|table| (*table).to_owned())
            .collect::<Vec<_>>();

        assert_eq!(applied.schema_version, WATCHER_EVENTS_MIGRATION_VERSION);
        assert_eq!(applied.product_tables, expected_tables);
        assert_eq!(store.applied_sql(), WATCHER_EVENTS_MIGRATION_UP_SQL);
        let up_sql = WATCHER_EVENTS_MIGRATION_UP_SQL[0];
        for required_column in [
            "contract_version TEXT NOT NULL",
            "before_policy_action TEXT",
            "before_policy_content_sync TEXT",
            "before_content_hash TEXT",
            "after_content_hash TEXT",
            "before_symlink_target TEXT",
            "after_symlink_target TEXT",
            "before_platform_pin_arch TEXT",
            "after_policy_action TEXT",
            "after_policy_content_sync TEXT",
            "after_platform_pin_arch TEXT",
        ] {
            assert!(up_sql.contains(required_column), "missing {required_column}");
        }

        let rolled_back = runner.rollback(&mut store).unwrap();

        assert_eq!(rolled_back.schema_version, BASELINE_SCHEMA_VERSION);
        assert_eq!(rolled_back.product_table_count(), 0);
        assert_eq!(store.rolled_back_sql(), WATCHER_EVENTS_MIGRATION_DOWN_SQL);
    }

    fn snapshot(entries: Vec<LocalTreeEntry>, policy: Policy) -> IndexedSnapshot {
        let indexer = LocalIndexer::new("project", policy, linux());
        indexer.index_tree(&InMemoryTree::new(entries)).unwrap()
    }

    fn file(
        path: impl Into<String>,
        size_bytes: u64,
        modified_unix_millis: u64,
        content_hash: &str,
    ) -> LocalTreeEntry {
        LocalTreeEntry::file(
            path,
            size_bytes,
            modified_unix_millis,
            0o644,
            Some(content_hash.to_owned()),
        )
    }

    fn dir(path: impl Into<String>, modified_unix_millis: u64, permissions: u32) -> LocalTreeEntry {
        LocalTreeEntry::directory(path, modified_unix_millis, permissions)
    }

    fn single_kind(queue: &EventQueue) -> EventKind {
        assert_eq!(queue.len(), 1);
        queue.peek().unwrap().kind
    }

    fn assert_action(snapshot: &IndexedSnapshot, path: &str, action: &str) {
        assert_eq!(snapshot.entry(path).unwrap().policy.action.as_str(), action);
    }

    struct TempProject {
        root: PathBuf,
    }

    impl TempProject {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(TEST_UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "dropbox-dev-watcher-{label}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.root.join(relative)
        }

        fn write_file(&self, relative: &str, contents: &[u8]) {
            let path = self.path(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, contents).unwrap();
        }

        fn rename(&self, from: &str, to: &str) {
            let to_path = self.path(to);
            if let Some(parent) = to_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::rename(self.path(from), to_path).unwrap();
        }

        fn remove_file(&self, relative: &str) {
            std::fs::remove_file(self.path(relative)).unwrap();
        }

        #[cfg(unix)]
        fn set_mode(&self, relative: &str, mode: u32) {
            use std::os::unix::fs::PermissionsExt;

            let path = self.path(relative);
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(mode);
            std::fs::set_permissions(path, permissions).unwrap();
        }

        #[cfg(not(unix))]
        fn set_mode(&self, _relative: &str, _mode: u32) {}
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn linux() -> Platform {
        let os_family = OsFamily::Linux;
        Platform {
            os_version: Some("6.8".to_owned()),
            capabilities: PlatformCapabilities::for_os(&os_family),
            os_family,
            architecture: Architecture::X86_64,
            machine_id: MachineId {
                value: format!("dropbox-dev-{}", "a".repeat(64)),
                provenance: MachineIdProvenance::Fallback,
            },
        }
    }
}
