//! Transport-independent catalog and structure model.
//!
//! The catalog records project structure and content-address metadata, never file
//! contents. Later chunks can feed this module from watchers/sync transports
//! without coupling the catalog to either source.

use crate::foundation::{Migration, MigrationError, MigrationRunner, Platform};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

pub const MODULE_NAME: &str = "catalog";

/// U2 serialization decision: deterministic, versioned, line-oriented UTF-8 text.
///
/// Rationale: this format is boring stdlib-only Rust, byte-stable across runs,
/// easy to diff in tests/logs, and does not require freezing a binary or JSON
/// dependency before the sync/storage layers exist. Every line is TAB separated;
/// string fields are UTF-8 percent-encoded by byte; entries are sorted by path and
/// metadata before writing.
pub const CATALOG_MANIFEST_FORMAT_VERSION: &str = "catalog-manifest-lines-v1";

pub const CATALOG_MIGRATION_VERSION: &str = "catalog_v1";
pub const CATALOG_MIGRATION_DESCRIPTION: &str =
    "catalog structure metadata schema; stores manifests, entries, machines, placeholders";
pub const CATALOG_PROJECTS_TABLE: &str = "catalog_projects";
pub const CATALOG_MACHINES_TABLE: &str = "catalog_machines";
pub const CATALOG_TREE_MANIFESTS_TABLE: &str = "catalog_tree_manifests";
pub const CATALOG_TREE_ENTRIES_TABLE: &str = "catalog_tree_entries";
pub const CATALOG_PLACEHOLDERS_TABLE: &str = "catalog_placeholders";
pub const CATALOG_MIGRATION_TABLES: &[&str] = &[
    CATALOG_PROJECTS_TABLE,
    CATALOG_MACHINES_TABLE,
    CATALOG_TREE_MANIFESTS_TABLE,
    CATALOG_TREE_ENTRIES_TABLE,
    CATALOG_PLACEHOLDERS_TABLE,
];
pub const CATALOG_MIGRATION_UP_SQL: &[&str] = &[
    "CREATE TABLE catalog_projects (id TEXT PRIMARY KEY, root_path TEXT NOT NULL, structure_manifest_id TEXT NOT NULL);",
    concat!(
        "CREATE TABLE catalog_machines (",
        "project_id TEXT NOT NULL, ",
        "id TEXT NOT NULL, ",
        "platform_os TEXT NOT NULL, ",
        "platform_os_version TEXT, ",
        "platform_arch TEXT NOT NULL, ",
        "platform_machine_id TEXT NOT NULL, ",
        "machine_id_provenance TEXT NOT NULL, ",
        "case_sensitive_paths INTEGER NOT NULL, ",
        "supports_symlinks INTEGER NOT NULL, ",
        "supports_posix_permissions INTEGER NOT NULL, ",
        "supports_file_ids INTEGER NOT NULL, ",
        "supports_fsevents INTEGER NOT NULL, ",
        "supports_inotify INTEGER NOT NULL, ",
        "last_seen_unix_millis INTEGER NOT NULL, ",
        "online INTEGER NOT NULL, ",
        "PRIMARY KEY (project_id, id));",
    ),
    "CREATE TABLE catalog_tree_manifests (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, format_version TEXT NOT NULL);",
    "CREATE TABLE catalog_tree_entries (manifest_id TEXT NOT NULL, path TEXT NOT NULL, entry_kind TEXT NOT NULL, size_bytes INTEGER NOT NULL, modified_unix_millis INTEGER NOT NULL, permissions INTEGER NOT NULL, content_hash TEXT, PRIMARY KEY (manifest_id, path));",
    "CREATE TABLE catalog_placeholders (project_id TEXT NOT NULL, path TEXT NOT NULL, size_bytes INTEGER NOT NULL, content_hash TEXT NOT NULL, hydration_status TEXT NOT NULL, source_machine_id TEXT NOT NULL, PRIMARY KEY (project_id, path));",
];
pub const CATALOG_MIGRATION_DOWN_SQL: &[&str] = &[
    "DROP TABLE catalog_placeholders;",
    "DROP TABLE catalog_tree_entries;",
    "DROP TABLE catalog_tree_manifests;",
    "DROP TABLE catalog_machines;",
    "DROP TABLE catalog_projects;",
];

pub type ProjectId = String;
pub type CatalogMachineId = String;
pub type ManifestId = String;
pub type ContentHash = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    /// Local project root as recorded by the catalog owner. Manifest entry paths
    /// remain normalized slash-separated strings and are transport-independent.
    pub root_path: String,
    pub machines: Vec<Machine>,
    pub structure_manifest_id: ManifestId,
}

impl Project {
    pub fn new(
        id: impl Into<ProjectId>,
        root_path: impl Into<String>,
        machines: Vec<Machine>,
        structure_manifest_id: impl Into<ManifestId>,
    ) -> Self {
        Self {
            id: id.into(),
            root_path: root_path.into(),
            machines,
            structure_manifest_id: structure_manifest_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub id: CatalogMachineId,
    pub platform: Platform,
    pub last_seen_unix_millis: u64,
    pub online: bool,
}

impl Machine {
    pub fn new(
        id: impl Into<CatalogMachineId>,
        platform: Platform,
        last_seen_unix_millis: u64,
        online: bool,
    ) -> Self {
        Self {
            id: id.into(),
            platform,
            last_seen_unix_millis,
            online,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TreeEntryKind {
    Directory,
    File,
    Symlink,
}

impl TreeEntryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Slash-separated path relative to the project root.
    pub path: String,
    pub kind: TreeEntryKind,
    pub size_bytes: u64,
    pub modified_unix_millis: u64,
    pub permissions: u32,
    /// Optional content-address slot. This is metadata used for identity and
    /// later hydration; the catalog never stores content bytes.
    pub content_hash: Option<ContentHash>,
}

impl TreeEntry {
    pub fn new(
        path: impl Into<String>,
        kind: TreeEntryKind,
        size_bytes: u64,
        modified_unix_millis: u64,
        permissions: u32,
        content_hash: Option<ContentHash>,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            size_bytes,
            modified_unix_millis,
            permissions,
            content_hash,
        }
    }

    pub fn directory(
        path: impl Into<String>,
        modified_unix_millis: u64,
        permissions: u32,
    ) -> Self {
        Self::new(
            path,
            TreeEntryKind::Directory,
            0,
            modified_unix_millis,
            permissions,
            None,
        )
    }

    pub fn file(
        path: impl Into<String>,
        size_bytes: u64,
        modified_unix_millis: u64,
        permissions: u32,
        content_hash: Option<ContentHash>,
    ) -> Self {
        Self::new(
            path,
            TreeEntryKind::File,
            size_bytes,
            modified_unix_millis,
            permissions,
            content_hash,
        )
    }

    pub fn symlink(
        path: impl Into<String>,
        modified_unix_millis: u64,
        permissions: u32,
        target_hash: Option<ContentHash>,
    ) -> Self {
        Self::new(
            path,
            TreeEntryKind::Symlink,
            0,
            modified_unix_millis,
            permissions,
            target_hash,
        )
    }

    fn move_identity(&self) -> Option<MoveIdentity> {
        let content_hash = self.content_hash.as_ref()?;
        if content_hash.is_empty() {
            return None;
        }

        Some(MoveIdentity {
            kind: self.kind,
            size_bytes: self.size_bytes,
            modified_unix_millis: self.modified_unix_millis,
            permissions: self.permissions,
            content_hash: content_hash.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeManifest {
    pub id: ManifestId,
    pub project_id: ProjectId,
    pub entries: Vec<TreeEntry>,
}

impl TreeManifest {
    pub fn new(
        id: impl Into<ManifestId>,
        project_id: impl Into<ProjectId>,
        entries: Vec<TreeEntry>,
    ) -> Self {
        Self {
            id: id.into(),
            project_id: project_id.into(),
            entries,
        }
    }

    pub fn canonical_entries(&self) -> Vec<&TreeEntry> {
        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.size_bytes.cmp(&right.size_bytes))
                .then_with(|| left.modified_unix_millis.cmp(&right.modified_unix_millis))
                .then_with(|| left.permissions.cmp(&right.permissions))
                .then_with(|| left.content_hash.cmp(&right.content_hash))
        });
        entries
    }

    pub fn serialize_deterministic(&self) -> String {
        let mut output = String::new();
        output.push_str("format\t");
        push_escaped_field(&mut output, CATALOG_MANIFEST_FORMAT_VERSION);
        output.push('\n');

        output.push_str("manifest\t");
        push_escaped_field(&mut output, &self.id);
        output.push('\t');
        push_escaped_field(&mut output, &self.project_id);
        output.push('\n');

        for entry in self.canonical_entries() {
            output.push_str("entry\t");
            push_escaped_field(&mut output, &entry.path);
            output.push('\t');
            output.push_str(entry.kind.as_str());
            output.push('\t');
            write!(&mut output, "{}", entry.size_bytes).expect("writing to String cannot fail");
            output.push('\t');
            write!(&mut output, "{}", entry.modified_unix_millis)
                .expect("writing to String cannot fail");
            output.push('\t');
            write!(&mut output, "{}", entry.permissions).expect("writing to String cannot fail");
            output.push('\t');
            match &entry.content_hash {
                Some(content_hash) => push_escaped_field(&mut output, content_hash),
                None => output.push('-'),
            }
            output.push('\n');
        }

        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HydrationStatus {
    NotHydrated,
    Hydrating,
    Hydrated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderRecord {
    pub path: String,
    pub size_bytes: u64,
    pub content_hash: ContentHash,
    pub hydration_status: HydrationStatus,
    pub source_machine_id: CatalogMachineId,
}

impl PlaceholderRecord {
    pub fn new(
        path: impl Into<String>,
        size_bytes: u64,
        content_hash: impl Into<ContentHash>,
        hydration_status: HydrationStatus,
        source_machine_id: impl Into<CatalogMachineId>,
    ) -> Self {
        Self {
            path: path.into(),
            size_bytes,
            content_hash: content_hash.into(),
            hydration_status,
            source_machine_id: source_machine_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructureChange {
    Added { entry: TreeEntry },
    Removed { entry: TreeEntry },
    Modified { before: TreeEntry, after: TreeEntry },
    Moved { before: TreeEntry, after: TreeEntry },
}

/// Emits the contentless changes required to make `before` structurally match
/// `after`. Move detection is conservative: a remove/add pair is collapsed into
/// a move only when both sides have the same unique metadata identity including
/// a non-empty content hash slot.
pub fn diff_manifests(before: &TreeManifest, after: &TreeManifest) -> Vec<StructureChange> {
    let before_by_path = entries_by_path(before);
    let after_by_path = entries_by_path(after);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut changes = Vec::new();

    for (path, before_entry) in &before_by_path {
        match after_by_path.get(path) {
            Some(after_entry) if before_entry != after_entry => {
                changes.push(StructureChange::Modified {
                    before: before_entry.clone(),
                    after: after_entry.clone(),
                });
            }
            Some(_) => {}
            None => removed.push(before_entry.clone()),
        }
    }

    for (path, after_entry) in &after_by_path {
        if !before_by_path.contains_key(path) {
            added.push(after_entry.clone());
        }
    }

    let removed_move_candidates = unique_move_candidates(&removed);
    let added_move_candidates = unique_move_candidates(&added);
    let mut moved_removed_indexes = BTreeSet::new();
    let mut moved_added_indexes = BTreeSet::new();

    for (identity, removed_index) in removed_move_candidates {
        if let Some(added_index) = added_move_candidates.get(&identity) {
            moved_removed_indexes.insert(removed_index);
            moved_added_indexes.insert(*added_index);
            changes.push(StructureChange::Moved {
                before: removed[removed_index].clone(),
                after: added[*added_index].clone(),
            });
        }
    }

    for (index, entry) in removed.into_iter().enumerate() {
        if !moved_removed_indexes.contains(&index) {
            changes.push(StructureChange::Removed { entry });
        }
    }

    for (index, entry) in added.into_iter().enumerate() {
        if !moved_added_indexes.contains(&index) {
            changes.push(StructureChange::Added { entry });
        }
    }

    changes.sort_by(|left, right| {
        structure_change_sort_key(left).cmp(&structure_change_sort_key(right))
    });
    changes
}

pub fn reconcile_structures(before: &TreeManifest, after: &TreeManifest) -> Vec<StructureChange> {
    diff_manifests(before, after)
}

pub fn catalog_initial_migration() -> Migration {
    Migration::new(
        CATALOG_MIGRATION_VERSION,
        CATALOG_MIGRATION_DESCRIPTION,
        CATALOG_MIGRATION_UP_SQL,
        CATALOG_MIGRATION_DOWN_SQL,
        CATALOG_MIGRATION_TABLES,
    )
}

pub fn catalog_migration_runner() -> Result<MigrationRunner, MigrationError> {
    MigrationRunner::with_migrations([catalog_initial_migration()])
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MoveIdentity {
    kind: TreeEntryKind,
    size_bytes: u64,
    modified_unix_millis: u64,
    permissions: u32,
    content_hash: ContentHash,
}

fn entries_by_path(manifest: &TreeManifest) -> BTreeMap<String, TreeEntry> {
    manifest
        .canonical_entries()
        .into_iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

fn unique_move_candidates(entries: &[TreeEntry]) -> BTreeMap<MoveIdentity, usize> {
    let mut candidates: BTreeMap<MoveIdentity, Option<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(identity) = entry.move_identity() {
            candidates
                .entry(identity)
                .and_modify(|existing| *existing = None)
                .or_insert(Some(index));
        }
    }

    candidates
        .into_iter()
        .filter_map(|(identity, index)| index.map(|index| (identity, index)))
        .collect()
}

fn structure_change_sort_key(change: &StructureChange) -> (u8, &str, &str) {
    match change {
        StructureChange::Added { entry } => (0, entry.path.as_str(), ""),
        StructureChange::Removed { entry } => (1, entry.path.as_str(), ""),
        StructureChange::Modified { before, after } => {
            (2, before.path.as_str(), after.path.as_str())
        }
        StructureChange::Moved { before, after } => (3, before.path.as_str(), after.path.as_str()),
    }
}

fn push_escaped_field(output: &mut String, value: &str) {
    for byte in value.bytes() {
        if matches!(byte, b' '..=b'~') && byte != b'%' {
            output.push(byte as char);
        } else {
            push_percent_encoded_byte(output, byte);
        }
    }
}

fn push_percent_encoded_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0F) as usize] as char);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::{
        Architecture, BASELINE_SCHEMA_VERSION, InMemoryMigrationStore,
        MachineId as FoundationMachineId, MachineIdProvenance, OsFamily,
        Platform as FoundationPlatform, PlatformCapabilities,
    };

    #[test]
    fn project_and_machine_use_shared_platform_identity() {
        let machine = Machine::new("machine-a", sample_platform(), 1_700_000_000_000, true);
        let project = Project::new(
            "project-a",
            "/workspace/project",
            vec![machine.clone()],
            "manifest-a",
        );

        assert_eq!(project.id, "project-a");
        assert_eq!(project.root_path, "/workspace/project");
        assert_eq!(project.machines, vec![machine]);
        assert_eq!(project.structure_manifest_id, "manifest-a");
    }

    #[test]
    fn manifest_serialization_is_deterministic_and_sorted() {
        let first = TreeManifest::new(
            "manifest-a",
            "project-a",
            vec![
                TreeEntry::file(
                    "src/main.rs",
                    10,
                    20,
                    0o644,
                    Some("sha256:main".to_owned()),
                ),
                TreeEntry::directory("src", 10, 0o755),
            ],
        );
        let second = TreeManifest::new(
            "manifest-a",
            "project-a",
            vec![
                TreeEntry::directory("src", 10, 0o755),
                TreeEntry::file(
                    "src/main.rs",
                    10,
                    20,
                    0o644,
                    Some("sha256:main".to_owned()),
                ),
            ],
        );

        let serialized = first.serialize_deterministic();

        assert_eq!(serialized, second.serialize_deterministic());
        assert!(serialized.starts_with("format\tcatalog-manifest-lines-v1\n"));
        assert!(
            serialized.find("entry\tsrc\tdirectory").unwrap()
                < serialized.find("entry\tsrc/main.rs\tfile").unwrap()
        );
    }

    #[test]
    fn manifest_serialization_excludes_file_content_bytes() {
        let content_bytes = "console.log('secret local content');";
        let manifest = TreeManifest::new(
            "manifest-a",
            "project-a",
            vec![TreeEntry::file(
                "src/app.js",
                content_bytes.len() as u64,
                30,
                0o644,
                Some("sha256:metadata-only".to_owned()),
            )],
        );
        let serialized = manifest.serialize_deterministic();

        assert!(!serialized.contains(content_bytes));
        assert!(serialized.contains("sha256:metadata-only"));
    }

    #[test]
    fn diff_reports_add_remove_modify_and_move_without_contents() {
        let unchanged =
            TreeEntry::file("src/main.rs", 10, 20, 0o644, Some("sha256:main".to_owned()));
        let removed = TreeEntry::file(
            "tmp/delete.me",
            5,
            21,
            0o600,
            Some("sha256:removed".to_owned()),
        );
        let modified_before = TreeEntry::file(
            "src/lib.rs",
            10,
            22,
            0o644,
            Some("sha256:lib-v1".to_owned()),
        );
        let modified_after = TreeEntry::file(
            "src/lib.rs",
            11,
            23,
            0o644,
            Some("sha256:lib-v2".to_owned()),
        );
        let moved_before = TreeEntry::file(
            "docs/old.md",
            7,
            24,
            0o644,
            Some("sha256:moved".to_owned()),
        );
        let moved_after = TreeEntry::file(
            "docs/new.md",
            7,
            24,
            0o644,
            Some("sha256:moved".to_owned()),
        );
        let added = TreeEntry::file(
            "src/new.rs",
            3,
            25,
            0o644,
            Some("sha256:added".to_owned()),
        );
        let before = TreeManifest::new(
            "before",
            "project-a",
            vec![
                unchanged.clone(),
                removed.clone(),
                modified_before.clone(),
                moved_before.clone(),
            ],
        );
        let after = TreeManifest::new(
            "after",
            "project-a",
            vec![unchanged, modified_after.clone(), moved_after.clone(), added.clone()],
        );

        let changes = diff_manifests(&before, &after);

        assert_eq!(changes.len(), 4);
        assert!(changes.contains(&StructureChange::Added { entry: added }));
        assert!(changes.contains(&StructureChange::Removed { entry: removed }));
        assert!(changes.contains(&StructureChange::Modified {
            before: modified_before,
            after: modified_after,
        }));
        assert!(changes.contains(&StructureChange::Moved {
            before: moved_before,
            after: moved_after,
        }));
    }

    #[test]
    fn identical_trees_reconcile_to_no_changes() {
        let manifest = TreeManifest::new(
            "manifest-a",
            "project-a",
            vec![
                TreeEntry::directory("src", 10, 0o755),
                TreeEntry::file("src/main.rs", 10, 20, 0o644, Some("sha256:main".to_owned())),
            ],
        );

        assert!(reconcile_structures(&manifest, &manifest).is_empty());
    }

    #[test]
    fn move_detection_requires_unique_hashed_identity() {
        let before = TreeManifest::new(
            "before",
            "project-a",
            vec![TreeEntry::file("old-name", 42, 10, 0o644, None)],
        );
        let after = TreeManifest::new(
            "after",
            "project-a",
            vec![TreeEntry::file("new-name", 42, 10, 0o644, None)],
        );
        let changes = diff_manifests(&before, &after);

        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[0], StructureChange::Added { .. }));
        assert!(matches!(&changes[1], StructureChange::Removed { .. }));
    }

    #[test]
    fn placeholder_record_carries_hydration_fetch_fields() {
        let placeholder = PlaceholderRecord::new(
            "src/big.bin",
            1_048_576,
            "sha256:big",
            HydrationStatus::NotHydrated,
            "machine-a",
        );

        assert_eq!(placeholder.path, "src/big.bin");
        assert_eq!(placeholder.size_bytes, 1_048_576);
        assert_eq!(placeholder.content_hash, "sha256:big");
        assert_eq!(placeholder.hydration_status, HydrationStatus::NotHydrated);
        assert_eq!(placeholder.source_machine_id, "machine-a");
    }

    #[test]
    fn catalog_migration_descriptor_applies_and_rolls_back_metadata() {
        let runner = catalog_migration_runner().unwrap();

        assert_eq!(runner.migrations()[1], catalog_initial_migration());

        let mut store = InMemoryMigrationStore::new();
        let applied = runner.apply(&mut store).unwrap();
        let expected_tables = CATALOG_MIGRATION_TABLES
            .iter()
            .map(|table| (*table).to_owned())
            .collect::<Vec<_>>();

        assert_eq!(applied.schema_version, CATALOG_MIGRATION_VERSION);
        assert_eq!(applied.product_tables, expected_tables);
        assert_eq!(store.applied_sql(), CATALOG_MIGRATION_UP_SQL);

        let rolled_back = runner.rollback(&mut store).unwrap();

        assert_eq!(rolled_back.schema_version, BASELINE_SCHEMA_VERSION);
        assert_eq!(rolled_back.product_table_count(), 0);
        assert_eq!(store.rolled_back_sql(), CATALOG_MIGRATION_DOWN_SQL);
    }

    fn sample_platform() -> FoundationPlatform {
        let os_family = OsFamily::Linux;
        let capabilities = PlatformCapabilities::for_os(&os_family);
        FoundationPlatform {
            os_family,
            os_version: Some("6.8".to_owned()),
            architecture: Architecture::X86_64,
            capabilities,
            machine_id: FoundationMachineId {
                value: format!("dropbox-dev-{}", "a".repeat(64)),
                provenance: MachineIdProvenance::Fallback,
            },
        }
    }
}
