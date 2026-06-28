//! Lazy-hydration virtual filesystem projection.
//!
//! U6 resolution: this stdlib-only baseline uses a transparent stub-file plus
//! metadata-cache model. Structure sync builds a complete project tree of
//! directories, symlink metadata stubs, and file placeholders without reading
//! remote content bytes. The first file read goes through the frozen
//! [`Hydrator`] contract, stores the bytes under the current [`CacheVersion`],
//! and later reads of the same version are served from the in-process cache.
//! A FUSE/native adapter can replace the stub-file materializer later without
//! changing [`Hydrator`] or [`VfsMount`].
//!
//! Policy decisions are intentionally not recomputed here and this module does
//! not import CHUNK-03 policy types. VFS consumes CHUNK-05 convergence metadata
//! ([`ConvergencePlan`] and [`ReplayState`]) and enforces the resulting outcomes
//! while preserving the full remote structure as metadata.
//!
//! Latency and failure behavior are explicit: metadata access is always local,
//! uncached content reads may block on a remote [`Hydrator`], cached reads are
//! local, and every denied/unavailable path reports an [`AccessFailureMode`].

use crate::catalog::{
    ContentHash, HydrationStatus, ManifestId, PlaceholderRecord, ProjectId, TreeEntry,
    TreeEntryKind, TreeManifest,
};
use crate::foundation::{Migration, MigrationError, MigrationRunner, VfsError};
use crate::sync::{ConvergenceAction, ConvergencePlan, ReplayState, SYNC_REMOTE_MANIFEST_MACHINE_ID};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

pub const MODULE_NAME: &str = "vfs";

pub const U6_LAZY_HYDRATION_DECISION: &str = "transparent stub-file metadata cache: structure is materialized without content fetches; first file access hydrates through the Hydrator contract and caches exactly one content payload per CacheVersion; FUSE/native adapters may replace the mount adapter behind the same contract";

pub const VFS_CACHE_LAYOUT_VERSION: &str = "vfs-cache-layout-v1";
pub const VFS_CACHE_ROOT_DIR: &str = "vfs_cache_v1";
pub const VFS_CONTENT_CACHE_DIR: &str = "content";
pub const VFS_PLACEHOLDER_SUFFIX: &str = ".vfs-placeholder";

pub const VFS_MIGRATION_VERSION: &str = "vfs_cache_v1";
pub const VFS_MIGRATION_DESCRIPTION: &str =
    "vfs lazy hydration metadata and content cache descriptors";
pub const VFS_PLACEHOLDERS_TABLE: &str = "vfs_placeholders";
pub const VFS_CONTENT_CACHE_TABLE: &str = "vfs_content_cache";
pub const VFS_MIGRATION_TABLES: &[&str] = &[VFS_PLACEHOLDERS_TABLE, VFS_CONTENT_CACHE_TABLE];
pub const VFS_MIGRATION_UP_SQL: &[&str] = &[
    "CREATE TABLE vfs_placeholders (project_id TEXT NOT NULL, path TEXT NOT NULL, manifest_id TEXT NOT NULL, entry_kind TEXT NOT NULL, size_bytes INTEGER NOT NULL, modified_unix_millis INTEGER NOT NULL, permissions INTEGER NOT NULL, content_hash TEXT, hydration_status TEXT NOT NULL, convergence_outcome TEXT NOT NULL, source_machine_id TEXT NOT NULL, PRIMARY KEY (project_id, path));",
    "CREATE TABLE vfs_content_cache (project_id TEXT NOT NULL, path TEXT NOT NULL, manifest_id TEXT NOT NULL, content_hash TEXT NOT NULL, size_bytes INTEGER NOT NULL, modified_unix_millis INTEGER NOT NULL, permissions INTEGER NOT NULL, cache_layout_version TEXT NOT NULL, PRIMARY KEY (project_id, path));",
];
pub const VFS_MIGRATION_DOWN_SQL: &[&str] = &[
    "DROP TABLE vfs_content_cache;",
    "DROP TABLE vfs_placeholders;",
];

pub type VfsResult<T> = Result<T, VfsAccessError>;

/// Describes the cache shape a durable adapter should use for this VFS layer.
///
/// The current implementation is intentionally in-memory for tests and for the
/// stdlib baseline, but the descriptor freezes the durable layout names so a
/// SQLite/file-backed adapter can persist the same placeholder and content-cache
/// records without changing the public mount contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsCacheLayoutDescriptor {
    pub version: &'static str,
    pub cache_root_dir: &'static str,
    pub content_cache_dir: &'static str,
    pub placeholder_suffix: &'static str,
    pub placeholder_table: &'static str,
    pub content_cache_table: &'static str,
    pub strategy: &'static str,
}

impl VfsCacheLayoutDescriptor {
    pub const fn transparent_stub_metadata_cache() -> Self {
        Self {
            version: VFS_CACHE_LAYOUT_VERSION,
            cache_root_dir: VFS_CACHE_ROOT_DIR,
            content_cache_dir: VFS_CONTENT_CACHE_DIR,
            placeholder_suffix: VFS_PLACEHOLDER_SUFFIX,
            placeholder_table: VFS_PLACEHOLDERS_TABLE,
            content_cache_table: VFS_CONTENT_CACHE_TABLE,
            strategy: U6_LAZY_HYDRATION_DECISION,
        }
    }
}

pub const VFS_CACHE_LAYOUT: VfsCacheLayoutDescriptor =
    VfsCacheLayoutDescriptor::transparent_stub_metadata_cache();

pub fn vfs_initial_migration() -> Migration {
    Migration::new(
        VFS_MIGRATION_VERSION,
        VFS_MIGRATION_DESCRIPTION,
        VFS_MIGRATION_UP_SQL,
        VFS_MIGRATION_DOWN_SQL,
        VFS_MIGRATION_TABLES,
    )
}

pub fn vfs_migration_runner() -> Result<MigrationRunner, MigrationError> {
    MigrationRunner::with_migrations([vfs_initial_migration()])
}

/// Version key for one cacheable remote file body.
///
/// VFS intentionally treats a remote manifest replacement as a new coherency
/// version even when the blob id is reused. That makes invalidation simple and
/// conservative for the stdlib baseline: after a remote update, cached content
/// is reused only when project, manifest, content id, size, mtime, and
/// permissions still match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CacheVersion {
    pub project_id: ProjectId,
    pub manifest_id: ManifestId,
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    pub modified_unix_millis: u64,
    pub permissions: u32,
}

impl CacheVersion {
    fn for_file(project_id: &str, manifest_id: &str, entry: &TreeEntry) -> Option<Self> {
        if entry.kind != TreeEntryKind::File {
            return None;
        }
        let content_hash = entry.content_hash.clone()?;
        Some(Self {
            project_id: project_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
            content_hash,
            size_bytes: entry.size_bytes,
            modified_unix_millis: entry.modified_unix_millis,
            permissions: entry.permissions,
        })
    }
}

/// Expected latency for an access attempt.
///
/// `RemoteFetch` is the only mode allowed to call [`Hydrator::fetch_content`].
/// Metadata operations and cached reads must not perform remote I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLatency {
    MetadataOnly,
    CachedContent,
    RemoteFetch,
}

/// Why a VFS access cannot return local content.
///
/// These variants are deliberately user-visible so callers can distinguish a
/// policy denial from a transient hydrator failure or a metadata-only stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessFailureMode {
    NotFound,
    NotAFile { kind: TreeEntryKind },
    MissingContentHash,
    IgnoredByPolicy,
    GitMetadataLocalOnly,
    PlatformRedirected {
        required_os: String,
        required_architecture: String,
    },
    RebuildRequired,
    ConflictSidecar { sidecar_path: String },
    SymlinkTargetMetadataPending { target_hash: Option<ContentHash> },
    RemoteFetchFailed { message: String },
    SizeMismatch {
        expected_size_bytes: u64,
        actual_size_bytes: u64,
    },
}

impl fmt::Display for AccessFailureMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(formatter, "path is not materialized"),
            Self::NotAFile { kind } => write!(formatter, "entry is a {}, not a file", kind.as_str()),
            Self::MissingContentHash => write!(formatter, "file has no remote content hash"),
            Self::IgnoredByPolicy => write!(formatter, "path is ignored by convergence policy"),
            Self::GitMetadataLocalOnly => write!(formatter, "Git metadata is local-only"),
            Self::PlatformRedirected {
                required_os,
                required_architecture,
            } => write!(
                formatter,
                "path is pinned to {required_os}/{required_architecture} and must be redirected"
            ),
            Self::RebuildRequired => write!(formatter, "path must be rebuilt locally"),
            Self::ConflictSidecar { sidecar_path } => write!(
                formatter,
                "conflict sidecar artifact `{sidecar_path}` is advisory metadata"
            ),
            Self::SymlinkTargetMetadataPending { target_hash } => match target_hash {
                Some(target_hash) => write!(
                    formatter,
                    "symlink target metadata `{target_hash}` must be fetched before access"
                ),
                None => write!(formatter, "symlink target metadata must be fetched before access"),
            },
            Self::RemoteFetchFailed { message } => write!(formatter, "remote fetch failed: {message}"),
            Self::SizeMismatch {
                expected_size_bytes,
                actual_size_bytes,
            } => write!(
                formatter,
                "hydrated size {actual_size_bytes} did not match metadata size {expected_size_bytes}"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsAccessError {
    pub path: String,
    pub failure_mode: AccessFailureMode,
}

impl VfsAccessError {
    pub fn new(path: impl Into<String>, failure_mode: AccessFailureMode) -> Self {
        Self {
            path: path.into(),
            failure_mode,
        }
    }

    pub fn remote_fetch_failed(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(
            path,
            AccessFailureMode::RemoteFetchFailed {
                message: message.into(),
            },
        )
    }
}

impl fmt::Display for VfsAccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "vfs access `{}` failed: {}",
            self.path, self.failure_mode
        )
    }
}

impl Error for VfsAccessError {}

impl From<VfsAccessError> for VfsError {
    fn from(error: VfsAccessError) -> Self {
        VfsError::unsupported(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsAccessProfile {
    pub latency: AccessLatency,
    pub failure_mode: Option<AccessFailureMode>,
}

impl VfsAccessProfile {
    pub fn success(latency: AccessLatency) -> Self {
        Self {
            latency,
            failure_mode: None,
        }
    }

    pub fn failure(latency: AccessLatency, failure_mode: AccessFailureMode) -> Self {
        Self {
            latency,
            failure_mode: Some(failure_mode),
        }
    }
}

/// CHUNK-05 convergence result as consumed by VFS.
///
/// This enum is a projection of sync metadata, not a policy engine. Keeping it
/// here avoids a direct dependency on policy internals while still forcing VFS
/// reads to respect blocking decisions and surfacing advisory conflict sidecar
/// metadata for same-path divergence resolved by sync convergence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VfsAccessOutcome {
    #[default]
    Allowed,
    PlatformAccepted,
    Ignored,
    GitMetadataLocalOnly,
    PlatformRedirected {
        required_os: String,
        required_architecture: String,
    },
    RebuildRequired,
    ConflictSidecar { sidecar_path: String },
    SymlinkTargetMetadataPending { target_hash: Option<ContentHash> },
}

impl VfsAccessOutcome {
    fn allows_hydration_for_path(&self, path: &str) -> bool {
        match self {
            Self::Allowed | Self::PlatformAccepted => true,
            Self::ConflictSidecar { sidecar_path } => sidecar_path.as_str() != path,
            _ => false,
        }
    }

    fn replay_platform_redirected() -> Self {
        // ReplayState only carries the redirected path, not the required branch tuple.
        // Keep replay-derived platform pins denied rather than treating them as accepted.
        Self::PlatformRedirected {
            required_os: "unknown".to_owned(),
            required_architecture: "unknown".to_owned(),
        }
    }

    fn failure_mode_for_path(&self, path: &str) -> Option<AccessFailureMode> {
        match self {
            Self::Allowed | Self::PlatformAccepted => None,
            Self::Ignored => Some(AccessFailureMode::IgnoredByPolicy),
            Self::GitMetadataLocalOnly => Some(AccessFailureMode::GitMetadataLocalOnly),
            Self::PlatformRedirected {
                required_os,
                required_architecture,
            } => Some(AccessFailureMode::PlatformRedirected {
                required_os: required_os.clone(),
                required_architecture: required_architecture.clone(),
            }),
            Self::RebuildRequired => Some(AccessFailureMode::RebuildRequired),
            Self::ConflictSidecar { sidecar_path } if sidecar_path.as_str() == path => {
                Some(AccessFailureMode::ConflictSidecar {
                    sidecar_path: sidecar_path.clone(),
                })
            }
            Self::ConflictSidecar { .. } => None,
            Self::SymlinkTargetMetadataPending { target_hash } => {
                Some(AccessFailureMode::SymlinkTargetMetadataPending {
                    target_hash: target_hash.clone(),
                })
            }
        }
    }

    fn precedence(&self) -> u8 {
        match self {
            Self::Allowed => 0,
            Self::PlatformAccepted => 1,
            Self::SymlinkTargetMetadataPending { .. } => 2,
            Self::ConflictSidecar { .. } => 3,
            Self::RebuildRequired => 4,
            Self::PlatformRedirected { .. } => 5,
            Self::Ignored => 6,
            Self::GitMetadataLocalOnly => 7,
        }
    }

    fn as_persisted_str(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::PlatformAccepted => "platform_accepted",
            Self::Ignored => "ignored",
            Self::GitMetadataLocalOnly => "git_metadata_local_only",
            Self::PlatformRedirected { .. } => "platform_redirected",
            Self::RebuildRequired => "rebuild_required",
            Self::ConflictSidecar { .. } => "conflict_sidecar",
            Self::SymlinkTargetMetadataPending { .. } => "symlink_target_metadata_pending",
        }
    }
}

/// Sync/convergence metadata needed by the lazy VFS materializer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VfsSyncMetadata {
    outcomes: BTreeMap<String, VfsAccessOutcome>,
    source_machines: BTreeMap<String, String>,
}

impl VfsSyncMetadata {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn from_convergence_plan(plan: &ConvergencePlan) -> Self {
        let mut metadata = Self::default();
        metadata.merge_convergence_plan(plan);
        metadata
    }

    pub fn from_replay_state(replay: &ReplayState) -> Self {
        let mut metadata = Self::default();

        for (path, entry) in &replay.entries {
            metadata.record_source_machine(path.clone(), entry.source_machine_id.clone());
        }
        for path in &replay.ignored_paths {
            metadata.record_outcome(path.clone(), VfsAccessOutcome::Ignored);
        }
        for path in &replay.rebuild_paths {
            metadata.record_outcome(path.clone(), VfsAccessOutcome::RebuildRequired);
        }
        for path in &replay.platform_pinned_paths {
            metadata.record_outcome(
                path.clone(),
                VfsAccessOutcome::replay_platform_redirected(),
            );
        }
        for path in &replay.git_metadata_paths {
            metadata.record_outcome(path.clone(), VfsAccessOutcome::GitMetadataLocalOnly);
        }
        for sidecar_path in &replay.conflict_sidecars {
            metadata.record_outcome(
                sidecar_path.clone(),
                VfsAccessOutcome::ConflictSidecar {
                    sidecar_path: sidecar_path.clone(),
                },
            );
        }

        metadata
    }

    pub fn merge_convergence_plan(&mut self, plan: &ConvergencePlan) {
        for action in &plan.actions {
            match action {
                ConvergenceAction::Ignore { path } => {
                    self.record_outcome(path.clone(), VfsAccessOutcome::Ignored);
                }
                ConvergenceAction::GitMetadataLocalOnly { path } => {
                    self.record_outcome(path.clone(), VfsAccessOutcome::GitMetadataLocalOnly);
                }
                ConvergenceAction::PlatformPinAccepted { path } => {
                    self.record_outcome(path.clone(), VfsAccessOutcome::PlatformAccepted);
                }
                ConvergenceAction::PlatformPinRedirected {
                    path,
                    required_os,
                    required_architecture,
                } => self.record_outcome(
                    path.clone(),
                    VfsAccessOutcome::PlatformRedirected {
                        required_os: required_os.clone(),
                        required_architecture: required_architecture.clone(),
                    },
                ),
                ConvergenceAction::RebuildLocally { path } => {
                    self.record_outcome(path.clone(), VfsAccessOutcome::RebuildRequired);
                }
                ConvergenceAction::ConflictSidecar { path, sidecar_path, .. } => {
                    let outcome = VfsAccessOutcome::ConflictSidecar {
                        sidecar_path: sidecar_path.clone(),
                    };
                    self.record_outcome(path.clone(), outcome.clone());
                    self.record_outcome(sidecar_path.clone(), outcome);
                }
                ConvergenceAction::FetchSymlinkTargetMetadata { path, target_hash } => self
                    .record_outcome(
                        path.clone(),
                        VfsAccessOutcome::SymlinkTargetMetadataPending {
                            target_hash: target_hash.clone(),
                        },
                    ),
                ConvergenceAction::PushContent { .. }
                | ConvergenceAction::FetchContent { .. }
                | ConvergenceAction::DeleteLocal { .. }
                | ConvergenceAction::DeleteRemote { .. }
                | ConvergenceAction::MovePath { .. }
                | ConvergenceAction::PropagatePermissions { .. }
                | ConvergenceAction::PropagateSymlink { .. }
                | ConvergenceAction::QueueOffline { .. }
                | ConvergenceAction::Noop { .. } => {}
            }
        }
    }

    pub fn with_outcome(mut self, path: impl Into<String>, outcome: VfsAccessOutcome) -> Self {
        self.record_outcome(path, outcome);
        self
    }

    pub fn with_source_machine(
        mut self,
        path: impl Into<String>,
        source_machine_id: impl Into<String>,
    ) -> Self {
        self.record_source_machine(path, source_machine_id);
        self
    }

    pub fn outcome_for_path(&self, path: &str) -> VfsAccessOutcome {
        self.outcomes.get(path).cloned().unwrap_or_default()
    }

    pub fn source_machine_for_path(&self, path: &str) -> &str {
        self.source_machines
            .get(path)
            .map(String::as_str)
            .unwrap_or(SYNC_REMOTE_MANIFEST_MACHINE_ID)
    }

    pub fn outcomes(&self) -> &BTreeMap<String, VfsAccessOutcome> {
        &self.outcomes
    }

    fn record_outcome(&mut self, path: impl Into<String>, outcome: VfsAccessOutcome) {
        let path = path.into();
        match self.outcomes.get(&path) {
            Some(existing) if existing.precedence() > outcome.precedence() => {}
            _ => {
                self.outcomes.insert(path, outcome);
            }
        }
    }

    fn record_source_machine(
        &mut self,
        path: impl Into<String>,
        source_machine_id: impl Into<String>,
    ) {
        self.source_machines
            .insert(path.into(), source_machine_id.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsNode {
    pub path: String,
    pub kind: TreeEntryKind,
    pub size_bytes: u64,
    pub modified_unix_millis: u64,
    pub permissions: u32,
    pub content_hash: Option<ContentHash>,
    pub cache_version: Option<CacheVersion>,
    pub hydration_status: HydrationStatus,
    pub convergence_outcome: VfsAccessOutcome,
    pub source_machine_id: String,
    pub synthetic: bool,
}

impl VfsNode {
    fn from_entry(
        project_id: &str,
        manifest_id: &str,
        entry: &TreeEntry,
        convergence_outcome: VfsAccessOutcome,
        source_machine_id: String,
    ) -> Self {
        let cache_version = CacheVersion::for_file(project_id, manifest_id, entry);
        let hydration_status = if entry.kind == TreeEntryKind::File {
            HydrationStatus::NotHydrated
        } else {
            HydrationStatus::Hydrated
        };

        Self {
            path: entry.path.clone(),
            kind: entry.kind,
            size_bytes: entry.size_bytes,
            modified_unix_millis: entry.modified_unix_millis,
            permissions: entry.permissions,
            content_hash: entry.content_hash.clone(),
            cache_version,
            hydration_status,
            convergence_outcome,
            source_machine_id,
            synthetic: false,
        }
    }

    fn synthetic_directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: TreeEntryKind::Directory,
            size_bytes: 0,
            modified_unix_millis: 0,
            permissions: 0o755,
            content_hash: None,
            cache_version: None,
            hydration_status: HydrationStatus::Hydrated,
            convergence_outcome: VfsAccessOutcome::Allowed,
            source_machine_id: SYNC_REMOTE_MANIFEST_MACHINE_ID.to_owned(),
            synthetic: true,
        }
    }

    pub fn is_file_placeholder(&self) -> bool {
        self.kind == TreeEntryKind::File
            && self.cache_version.is_some()
            && self.convergence_outcome.allows_hydration_for_path(&self.path)
    }

    pub fn persisted_outcome(&self) -> &'static str {
        self.convergence_outcome.as_persisted_str()
    }
}

/// Request passed to a hydrator for the first read of a cache version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HydrationRequest<'a> {
    pub project_id: &'a str,
    pub path: &'a str,
    pub content_hash: &'a str,
    pub cache_version: &'a CacheVersion,
    pub size_bytes: u64,
    pub expected_latency: AccessLatency,
}

/// Remote-content adapter used by [`VfsMount`] on first read.
///
/// Implementations normally delegate to CHUNK-05's sync store fetch API. The
/// trait returns owned bytes exactly once per [`CacheVersion`]; VFS validates
/// the byte length before caching so corrupted or stale payloads do not become
/// local hits.
pub trait Hydrator {
    fn fetch_content(&mut self, request: HydrationRequest<'_>) -> VfsResult<Vec<u8>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedContent {
    pub version: CacheVersion,
    bytes: Vec<u8>,
}

impl CachedContent {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug)]
pub struct VfsRead<'a> {
    pub bytes: &'a [u8],
    pub cache_version: &'a CacheVersion,
    pub latency: AccessLatency,
    pub fetched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsMount {
    project_id: ProjectId,
    manifest_id: ManifestId,
    nodes: BTreeMap<String, VfsNode>,
    content_cache: BTreeMap<String, CachedContent>,
    layout: VfsCacheLayoutDescriptor,
}

impl VfsMount {
    pub fn materialize(manifest: &TreeManifest, sync_metadata: &VfsSyncMetadata) -> Self {
        Self {
            project_id: manifest.project_id.clone(),
            manifest_id: manifest.id.clone(),
            nodes: materialize_nodes(manifest, sync_metadata),
            content_cache: BTreeMap::new(),
            layout: VFS_CACHE_LAYOUT,
        }
    }

    pub fn apply_remote_update(&mut self, manifest: &TreeManifest, sync_metadata: &VfsSyncMetadata) {
        let mut nodes = materialize_nodes(manifest, sync_metadata);
        self.content_cache.retain(|path, cached| {
            matches!(
                nodes.get(path),
                Some(node)
                    if node.cache_version.as_ref() == Some(&cached.version)
                        && node.convergence_outcome.allows_hydration_for_path(path)
            )
        });
        for (path, cached) in &self.content_cache {
            if let Some(node) = nodes.get_mut(path) {
                if node.cache_version.as_ref() == Some(&cached.version) {
                    node.hydration_status = HydrationStatus::Hydrated;
                }
            }
        }
        self.project_id = manifest.project_id.clone();
        self.manifest_id = manifest.id.clone();
        self.nodes = nodes;
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub fn cache_layout(&self) -> VfsCacheLayoutDescriptor {
        self.layout
    }

    pub fn nodes(&self) -> &BTreeMap<String, VfsNode> {
        &self.nodes
    }

    pub fn node(&self, path: &str) -> Option<&VfsNode> {
        self.nodes.get(path)
    }

    pub fn cached_content_count(&self) -> usize {
        self.content_cache.len()
    }

    pub fn is_content_cached(&self, path: &str) -> bool {
        match self.nodes.get(path).and_then(|node| node.cache_version.as_ref()) {
            Some(version) => self.cache_matches_version(path, version),
            None => false,
        }
    }

    pub fn placeholder_records(&self) -> Vec<PlaceholderRecord> {
        self.nodes
            .values()
            .filter(|node| node.is_file_placeholder())
            .filter_map(|node| {
                let version = node.cache_version.as_ref()?;
                Some(PlaceholderRecord::new(
                    node.path.clone(),
                    node.size_bytes,
                    version.content_hash.clone(),
                    node.hydration_status.clone(),
                    node.source_machine_id.clone(),
                ))
            })
            .collect()
    }

    pub fn access_profile(&self, path: &str) -> VfsResult<VfsAccessProfile> {
        let node = self
            .nodes
            .get(path)
            .ok_or_else(|| VfsAccessError::new(path, AccessFailureMode::NotFound))?;

        if let Some(failure_mode) = node.convergence_outcome.failure_mode_for_path(path) {
            return Ok(VfsAccessProfile::failure(
                AccessLatency::MetadataOnly,
                failure_mode,
            ));
        }

        match node.kind {
            TreeEntryKind::Directory | TreeEntryKind::Symlink => {
                Ok(VfsAccessProfile::success(AccessLatency::MetadataOnly))
            }
            TreeEntryKind::File => match node.cache_version.as_ref() {
                None => Ok(VfsAccessProfile::failure(
                    AccessLatency::MetadataOnly,
                    AccessFailureMode::MissingContentHash,
                )),
                Some(version) if self.cache_matches_version(path, version) => {
                    Ok(VfsAccessProfile::success(AccessLatency::CachedContent))
                }
                Some(_) => Ok(VfsAccessProfile::success(AccessLatency::RemoteFetch)),
            },
        }
    }

    pub fn read_file<'a, H: Hydrator + ?Sized>(
        &'a mut self,
        path: &str,
        hydrator: &mut H,
    ) -> VfsResult<VfsRead<'a>> {
        let decision = self.read_decision(path)?;
        let fetched = if self.cache_matches_version(path, &decision.cache_version) {
            false
        } else {
            if let Some(node) = self.nodes.get_mut(path) {
                node.hydration_status = HydrationStatus::Hydrating;
            }

            let fetch_result = {
                let request = HydrationRequest {
                    project_id: &self.project_id,
                    path,
                    content_hash: &decision.cache_version.content_hash,
                    cache_version: &decision.cache_version,
                    size_bytes: decision.size_bytes,
                    expected_latency: AccessLatency::RemoteFetch,
                };
                hydrator.fetch_content(request)
            };

            let bytes = match fetch_result {
                Ok(bytes) => bytes,
                Err(error) => {
                    if let Some(node) = self.nodes.get_mut(path) {
                        node.hydration_status = HydrationStatus::NotHydrated;
                    }
                    return Err(error);
                }
            };
            let actual_size_bytes = bytes.len() as u64;
            if actual_size_bytes != decision.size_bytes {
                if let Some(node) = self.nodes.get_mut(path) {
                    node.hydration_status = HydrationStatus::NotHydrated;
                }
                return Err(VfsAccessError::new(
                    path,
                    AccessFailureMode::SizeMismatch {
                        expected_size_bytes: decision.size_bytes,
                        actual_size_bytes,
                    },
                ));
            }

            self.content_cache.insert(
                path.to_owned(),
                CachedContent {
                    version: decision.cache_version,
                    bytes,
                },
            );
            if let Some(node) = self.nodes.get_mut(path) {
                node.hydration_status = HydrationStatus::Hydrated;
            }
            true
        };

        let cached = self
            .content_cache
            .get(path)
            .expect("cached content must exist after successful hydration decision");
        Ok(VfsRead {
            bytes: cached.bytes(),
            cache_version: &cached.version,
            latency: if fetched {
                AccessLatency::RemoteFetch
            } else {
                AccessLatency::CachedContent
            },
            fetched,
        })
    }

    fn read_decision(&self, path: &str) -> VfsResult<ReadDecision> {
        let node = self
            .nodes
            .get(path)
            .ok_or_else(|| VfsAccessError::new(path, AccessFailureMode::NotFound))?;

        if let Some(failure_mode) = node.convergence_outcome.failure_mode_for_path(path) {
            return Err(VfsAccessError::new(path, failure_mode));
        }

        if node.kind != TreeEntryKind::File {
            return Err(VfsAccessError::new(
                path,
                AccessFailureMode::NotAFile { kind: node.kind },
            ));
        }

        let cache_version = node
            .cache_version
            .clone()
            .ok_or_else(|| VfsAccessError::new(path, AccessFailureMode::MissingContentHash))?;

        Ok(ReadDecision {
            cache_version,
            size_bytes: node.size_bytes,
        })
    }

    fn cache_matches_version(&self, path: &str, version: &CacheVersion) -> bool {
        self.content_cache
            .get(path)
            .map(|cached| &cached.version == version)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadDecision {
    cache_version: CacheVersion,
    size_bytes: u64,
}

fn materialize_nodes(
    manifest: &TreeManifest,
    sync_metadata: &VfsSyncMetadata,
) -> BTreeMap<String, VfsNode> {
    let mut nodes = BTreeMap::new();
    for entry in manifest.canonical_entries() {
        insert_synthetic_parents(&mut nodes, &entry.path);
        let outcome = sync_metadata.outcome_for_path(&entry.path);
        let source_machine_id = sync_metadata.source_machine_for_path(&entry.path).to_owned();
        nodes.insert(
            entry.path.clone(),
            VfsNode::from_entry(
                &manifest.project_id,
                &manifest.id,
                entry,
                outcome,
                source_machine_id,
            ),
        );
    }
    nodes
}

fn insert_synthetic_parents(nodes: &mut BTreeMap<String, VfsNode>, path: &str) {
    let mut current = String::new();
    let mut components = path.split('/').filter(|component| !component.is_empty()).peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        nodes
            .entry(current.clone())
            .or_insert_with(|| VfsNode::synthetic_directory(current.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TreeEntry;
    use crate::sync::{filter_manifest_replay_tombstones, DeleteTombstone};
    use std::collections::BTreeMap;

    #[derive(Debug, Default)]
    struct CountingHydrator {
        blobs: BTreeMap<ContentHash, Vec<u8>>,
        fetches: BTreeMap<ContentHash, usize>,
        requests: Vec<(String, ManifestId)>,
    }

    impl CountingHydrator {
        fn with_blob(mut self, content_hash: &str, bytes: &[u8]) -> Self {
            self.blobs.insert(content_hash.to_owned(), bytes.to_vec());
            self
        }

        fn total_fetches(&self) -> usize {
            self.fetches.values().sum()
        }
    }

    impl Hydrator for CountingHydrator {
        fn fetch_content(&mut self, request: HydrationRequest<'_>) -> VfsResult<Vec<u8>> {
            *self
                .fetches
                .entry(request.content_hash.to_owned())
                .or_insert(0) += 1;
            self.requests.push((
                request.path.to_owned(),
                request.cache_version.manifest_id.clone(),
            ));
            self.blobs
                .get(request.content_hash)
                .cloned()
                .ok_or_else(|| VfsAccessError::remote_fetch_failed(request.path, "missing test blob"))
        }
    }

    fn manifest(id: &str, entries: Vec<TreeEntry>) -> TreeManifest {
        TreeManifest::new(id, "project-alpha", entries)
    }

    fn replay_with_tombstone(path: &str, modified_unix_millis: u64) -> ReplayState {
        let mut replay = ReplayState::default();
        replay.tombstones.insert(
            path.to_owned(),
            DeleteTombstone {
                path: path.to_owned(),
                modified_unix_millis,
                source_machine_id: "machine-deleter".to_owned(),
            },
        );
        replay
    }

    #[test]
    fn fresh_structure_materializes_placeholders_without_fetching_content() {
        let tree = manifest(
            "manifest-1",
            vec![TreeEntry::file(
                "src/lib.rs",
                4,
                10,
                0o644,
                Some("blob-1".to_owned()),
            )],
        );
        let hydrator = CountingHydrator::default().with_blob("blob-1", b"rust");

        let mount = VfsMount::materialize(&tree, &VfsSyncMetadata::none());

        assert_eq!(hydrator.total_fetches(), 0);
        assert_eq!(mount.project_id(), "project-alpha");
        assert_eq!(mount.manifest_id(), "manifest-1");
        assert_eq!(mount.cache_layout(), VFS_CACHE_LAYOUT);
        assert_eq!(
            mount.node("src").map(|node| (node.kind, node.synthetic)),
            Some((TreeEntryKind::Directory, true))
        );

        let file = mount.node("src/lib.rs").expect("file placeholder");
        assert_eq!(file.hydration_status, HydrationStatus::NotHydrated);
        assert_eq!(file.content_hash.as_deref(), Some("blob-1"));
        assert!(!mount.is_content_cached("src/lib.rs"));
        assert_eq!(
            mount.access_profile("src/lib.rs").expect("profile").latency,
            AccessLatency::RemoteFetch
        );

        let placeholders = mount.placeholder_records();
        assert_eq!(placeholders.len(), 1);
        assert_eq!(placeholders[0].path, "src/lib.rs");
        assert_eq!(placeholders[0].hydration_status, HydrationStatus::NotHydrated);
        assert_eq!(placeholders[0].source_machine_id, SYNC_REMOTE_MANIFEST_MACHINE_ID);
    }

    #[test]
    fn read_fetches_once_per_cache_version_then_uses_cache() {
        let tree = manifest(
            "manifest-1",
            vec![TreeEntry::file(
                "app.txt",
                5,
                11,
                0o644,
                Some("blob-a".to_owned()),
            )],
        );
        let mut mount = VfsMount::materialize(&tree, &VfsSyncMetadata::none());
        let mut hydrator = CountingHydrator::default().with_blob("blob-a", b"hello");

        {
            let first = mount.read_file("app.txt", &mut hydrator).expect("first read");
            assert_eq!(first.bytes, b"hello");
            assert_eq!(first.latency, AccessLatency::RemoteFetch);
            assert!(first.fetched);
            assert_eq!(first.cache_version.content_hash, "blob-a");
        }
        assert_eq!(hydrator.total_fetches(), 1);
        assert_eq!(mount.cached_content_count(), 1);
        assert_eq!(
            mount.node("app.txt").expect("node").hydration_status,
            HydrationStatus::Hydrated
        );
        assert_eq!(
            mount.access_profile("app.txt").expect("profile").latency,
            AccessLatency::CachedContent
        );

        {
            let second = mount.read_file("app.txt", &mut hydrator).expect("second read");
            assert_eq!(second.bytes, b"hello");
            assert_eq!(second.latency, AccessLatency::CachedContent);
            assert!(!second.fetched);
        }
        assert_eq!(hydrator.total_fetches(), 1);
    }

    #[test]
    fn remote_update_invalidates_cached_content_until_new_version_is_hydrated() {
        let first_tree = manifest(
            "manifest-1",
            vec![TreeEntry::file(
                "app.txt",
                5,
                11,
                0o644,
                Some("blob-a".to_owned()),
            )],
        );
        let second_tree = manifest(
            "manifest-2",
            vec![TreeEntry::file(
                "app.txt",
                7,
                12,
                0o600,
                Some("blob-b".to_owned()),
            )],
        );
        let mut mount = VfsMount::materialize(&first_tree, &VfsSyncMetadata::none());
        let mut hydrator = CountingHydrator::default()
            .with_blob("blob-a", b"hello")
            .with_blob("blob-b", b"goodbye");

        assert_eq!(
            mount.read_file("app.txt", &mut hydrator).expect("read").bytes,
            b"hello"
        );
        assert_eq!(hydrator.total_fetches(), 1);
        assert!(mount.is_content_cached("app.txt"));

        mount.apply_remote_update(&second_tree, &VfsSyncMetadata::none());

        assert_eq!(mount.cached_content_count(), 0);
        assert!(!mount.is_content_cached("app.txt"));
        assert_eq!(
            mount.access_profile("app.txt").expect("profile").latency,
            AccessLatency::RemoteFetch
        );

        assert_eq!(
            mount.read_file("app.txt", &mut hydrator).expect("new read").bytes,
            b"goodbye"
        );
        assert_eq!(hydrator.total_fetches(), 2);
        assert_eq!(
            mount.read_file("app.txt", &mut hydrator).expect("cached read").bytes,
            b"goodbye"
        );
        assert_eq!(hydrator.total_fetches(), 2);
    }

    #[test]
    fn convergence_policy_and_git_outcomes_block_hydration_without_policy_imports() {
        let tree = manifest(
            "manifest-1",
            vec![
                TreeEntry::file("ignored.log", 3, 10, 0o644, Some("ignored".to_owned())),
                TreeEntry::file(".git/config", 6, 10, 0o600, Some("git".to_owned())),
                TreeEntry::file("bin/tool", 4, 10, 0o755, Some("tool".to_owned())),
                TreeEntry::file("ok.txt", 2, 10, 0o644, Some("ok".to_owned())),
            ],
        );
        let plan = ConvergencePlan {
            actions: vec![
                ConvergenceAction::Ignore {
                    path: "ignored.log".to_owned(),
                },
                ConvergenceAction::GitMetadataLocalOnly {
                    path: ".git/config".to_owned(),
                },
                ConvergenceAction::PlatformPinRedirected {
                    path: "bin/tool".to_owned(),
                    required_os: "macos".to_owned(),
                    required_architecture: "aarch64".to_owned(),
                },
                ConvergenceAction::PlatformPinAccepted {
                    path: "ok.txt".to_owned(),
                },
            ],
            queued_operations: Vec::new(),
        };
        let metadata = VfsSyncMetadata::from_convergence_plan(&plan);
        let mut mount = VfsMount::materialize(&tree, &metadata);
        let mut hydrator = CountingHydrator::default()
            .with_blob("ignored", b"log")
            .with_blob("git", b"config")
            .with_blob("tool", b"tool")
            .with_blob("ok", b"ok");

        assert_eq!(
            mount
                .access_profile("ignored.log")
                .expect("profile")
                .failure_mode,
            Some(AccessFailureMode::IgnoredByPolicy)
        );
        assert_eq!(
            mount
                .access_profile(".git/config")
                .expect("profile")
                .failure_mode,
            Some(AccessFailureMode::GitMetadataLocalOnly)
        );
        assert_eq!(
            mount.read_file("ignored.log", &mut hydrator)
                .expect_err("ignored read")
                .failure_mode,
            AccessFailureMode::IgnoredByPolicy
        );
        assert_eq!(
            mount.read_file(".git/config", &mut hydrator)
                .expect_err("git read")
                .failure_mode,
            AccessFailureMode::GitMetadataLocalOnly
        );
        assert_eq!(
            mount.read_file("bin/tool", &mut hydrator)
                .expect_err("platform redirect")
                .failure_mode,
            AccessFailureMode::PlatformRedirected {
                required_os: "macos".to_owned(),
                required_architecture: "aarch64".to_owned(),
            }
        );
        assert_eq!(hydrator.total_fetches(), 0);

        assert_eq!(
            mount.read_file("ok.txt", &mut hydrator).expect("accepted read").bytes,
            b"ok"
        );
        assert_eq!(hydrator.total_fetches(), 1);
        assert_eq!(
            mount.placeholder_records()
                .iter()
                .map(|record| record.path.as_str())
                .collect::<Vec<_>>(),
            vec!["ok.txt"]
        );
    }

    #[test]
    fn conflict_sidecar_metadata_does_not_block_winner_hydration() {
        let sidecar_path = "docs/report.txt.conflict.machine-b.12".to_owned();
        let tree = manifest(
            "manifest-1",
            vec![
                TreeEntry::file("docs/report.txt", 6, 20, 0o644, Some("winner".to_owned())),
                TreeEntry::file(sidecar_path.clone(), 5, 19, 0o600, Some("loser".to_owned())),
            ],
        );
        let plan = ConvergencePlan {
            actions: vec![ConvergenceAction::ConflictSidecar {
                path: "docs/report.txt".to_owned(),
                sidecar_path: sidecar_path.clone(),
                winner_machine_id: SYNC_REMOTE_MANIFEST_MACHINE_ID.to_owned(),
                loser_machine_id: "machine-b".to_owned(),
                loser_content_hash: "loser".to_owned(),
                loser_bytes: b"loser".to_vec(),
            }],
            queued_operations: Vec::new(),
        };
        let metadata = VfsSyncMetadata::from_convergence_plan(&plan);
        let expected_outcome = VfsAccessOutcome::ConflictSidecar {
            sidecar_path: sidecar_path.clone(),
        };

        assert_eq!(
            metadata.outcome_for_path("docs/report.txt"),
            expected_outcome.clone()
        );
        assert_eq!(
            metadata.outcome_for_path(&sidecar_path),
            expected_outcome.clone()
        );

        let mut mount = VfsMount::materialize(&tree, &metadata);
        let mut hydrator = CountingHydrator::default()
            .with_blob("winner", b"winner")
            .with_blob("loser", b"loser");

        assert_eq!(
            &mount
                .node("docs/report.txt")
                .expect("winner node")
                .convergence_outcome,
            &expected_outcome
        );
        assert_eq!(
            mount
                .access_profile("docs/report.txt")
                .expect("winner profile"),
            VfsAccessProfile::success(AccessLatency::RemoteFetch)
        );
        {
            let read = mount
                .read_file("docs/report.txt", &mut hydrator)
                .expect("winner read");
            assert_eq!(read.bytes, b"winner");
            assert_eq!(read.latency, AccessLatency::RemoteFetch);
            assert!(read.fetched);
        }
        assert_eq!(hydrator.total_fetches(), 1);
        assert!(mount.is_content_cached("docs/report.txt"));
        assert_eq!(
            mount.node("docs/report.txt").expect("winner node").hydration_status,
            HydrationStatus::Hydrated
        );
        assert_eq!(
            mount
                .read_file(&sidecar_path, &mut hydrator)
                .expect_err("sidecar artifact read")
                .failure_mode,
            AccessFailureMode::ConflictSidecar {
                sidecar_path: sidecar_path.clone(),
            }
        );
        assert_eq!(hydrator.total_fetches(), 1);
        assert_eq!(
            mount
                .placeholder_records()
                .iter()
                .map(|record| record.path.as_str())
                .collect::<Vec<_>>(),
            vec!["docs/report.txt"]
        );
    }

    #[test]
    fn replay_platform_redirected_path_blocks_hydration() {
        let tree = manifest(
            "manifest-1",
            vec![TreeEntry::file(
                "bin/tool",
                4,
                10,
                0o755,
                Some("tool".to_owned()),
            )],
        );
        let mut replay = ReplayState::default();
        replay.platform_pinned_paths.insert("bin/tool".to_owned());
        let metadata = VfsSyncMetadata::from_replay_state(&replay);
        let expected_outcome = VfsAccessOutcome::PlatformRedirected {
            required_os: "unknown".to_owned(),
            required_architecture: "unknown".to_owned(),
        };
        let expected_failure = AccessFailureMode::PlatformRedirected {
            required_os: "unknown".to_owned(),
            required_architecture: "unknown".to_owned(),
        };

        assert_eq!(metadata.outcome_for_path("bin/tool"), expected_outcome);

        let mut mount = VfsMount::materialize(&tree, &metadata);
        let mut hydrator = CountingHydrator::default().with_blob("tool", b"tool");

        assert_eq!(
            mount
                .access_profile("bin/tool")
                .expect("profile")
                .failure_mode,
            Some(expected_failure.clone())
        );
        assert_eq!(
            mount
                .read_file("bin/tool", &mut hydrator)
                .expect_err("platform replay redirect")
                .failure_mode,
            expected_failure
        );
        assert_eq!(hydrator.total_fetches(), 0);
        assert!(!mount.is_content_cached("bin/tool"));
        assert_eq!(
            mount.node("bin/tool").expect("node").hydration_status,
            HydrationStatus::NotHydrated
        );
    }

    #[test]
    fn filtered_manifest_does_not_materialize_or_fetch_stale_tombstoned_file() {
        let stale_tree = manifest(
            "manifest-stale-file",
            vec![TreeEntry::file(
                "deleted.txt",
                5,
                10,
                0o644,
                Some("stale".to_owned()),
            )],
        );
        let replay = replay_with_tombstone("deleted.txt", 20);
        let filtered_tree = filter_manifest_replay_tombstones(&stale_tree, &replay);
        let metadata = VfsSyncMetadata::from_replay_state(&replay);
        let mut mount = VfsMount::materialize(&filtered_tree, &metadata);
        let mut hydrator = CountingHydrator::default().with_blob("stale", b"stale");

        assert!(mount.node("deleted.txt").is_none());
        assert!(mount.placeholder_records().is_empty());
        assert_eq!(
            mount
                .read_file("deleted.txt", &mut hydrator)
                .expect_err("stale tombstoned file should not materialize")
                .failure_mode,
            AccessFailureMode::NotFound
        );
        assert_eq!(hydrator.total_fetches(), 0);
    }

    #[test]
    fn filtered_manifest_does_not_materialize_or_fetch_directory_child_under_tombstone() {
        let stale_tree = manifest(
            "manifest-stale-directory",
            vec![TreeEntry::file(
                "deleted-dir/child.txt",
                5,
                10,
                0o644,
                Some("stale-child".to_owned()),
            )],
        );
        let replay = replay_with_tombstone("deleted-dir", 20);
        let filtered_tree = filter_manifest_replay_tombstones(&stale_tree, &replay);
        let metadata = VfsSyncMetadata::from_replay_state(&replay);
        let mut mount = VfsMount::materialize(&filtered_tree, &metadata);
        let mut hydrator = CountingHydrator::default().with_blob("stale-child", b"stale");

        assert!(mount.node("deleted-dir").is_none());
        assert!(mount.node("deleted-dir/child.txt").is_none());
        assert!(mount.placeholder_records().is_empty());
        assert_eq!(
            mount
                .read_file("deleted-dir/child.txt", &mut hydrator)
                .expect_err("stale child under tombstoned directory should not materialize")
                .failure_mode,
            AccessFailureMode::NotFound
        );
        assert_eq!(hydrator.total_fetches(), 0);
    }

    #[test]
    fn access_latency_and_failure_metadata_are_reported() {
        let tree = manifest(
            "manifest-1",
            vec![
                TreeEntry::directory("docs", 10, 0o755),
                TreeEntry::file("metadata-only.txt", 0, 10, 0o644, None),
                TreeEntry::file("bad-size.txt", 5, 10, 0o644, Some("short".to_owned())),
            ],
        );
        let mut mount = VfsMount::materialize(&tree, &VfsSyncMetadata::none());
        let mut hydrator = CountingHydrator::default().with_blob("short", b"bad");

        assert_eq!(
            mount.access_profile("docs").expect("directory").latency,
            AccessLatency::MetadataOnly
        );
        assert_eq!(
            mount
                .access_profile("metadata-only.txt")
                .expect("profile")
                .failure_mode,
            Some(AccessFailureMode::MissingContentHash)
        );
        assert_eq!(
            mount.read_file("metadata-only.txt", &mut hydrator)
                .expect_err("missing hash")
                .failure_mode,
            AccessFailureMode::MissingContentHash
        );
        assert_eq!(hydrator.total_fetches(), 0);

        assert_eq!(
            mount.read_file("bad-size.txt", &mut hydrator)
                .expect_err("bad size")
                .failure_mode,
            AccessFailureMode::SizeMismatch {
                expected_size_bytes: 5,
                actual_size_bytes: 3,
            }
        );
        assert_eq!(hydrator.total_fetches(), 1);
        assert_eq!(mount.cached_content_count(), 0);
        assert_eq!(
            mount.node("bad-size.txt").expect("node").hydration_status,
            HydrationStatus::NotHydrated
        );
    }

    #[test]
    fn cache_layout_descriptor_and_migration_are_exposed() {
        let layout = VfsCacheLayoutDescriptor::transparent_stub_metadata_cache();
        assert_eq!(layout.version, VFS_CACHE_LAYOUT_VERSION);
        assert_eq!(layout.placeholder_table, VFS_PLACEHOLDERS_TABLE);
        assert_eq!(layout.content_cache_table, VFS_CONTENT_CACHE_TABLE);
        assert!(layout.strategy.contains("transparent stub-file"));

        let migration = vfs_initial_migration();
        assert_eq!(migration.version, VFS_MIGRATION_VERSION);
        assert_eq!(migration.product_tables, VFS_MIGRATION_TABLES);
        assert_eq!(migration.up_sql.len(), 2);
    }
}
