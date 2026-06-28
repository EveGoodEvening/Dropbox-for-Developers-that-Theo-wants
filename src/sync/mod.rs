//! Sync store, transport, and convergence foundation for CHUNK-05.
//!
//! U10 resolution: the production backend is an append-only, file-backed sync
//! store rooted at a configurable shared directory. That directory can be a
//! mounted network share, a replicated object-store-like prefix, or another
//! cross-machine filesystem path. The local harness used by tests points the
//! same [`FileBackedSyncStore`] contract at temporary directories; loopback is
//! therefore only a harness detail, not the production transport answer.
//!
//! The transport writes encrypted/authenticated envelopes for manifests,
//! content blobs, operation records, and future typed payloads. The CHUNK-05
//! stdlib-only crypto baseline intentionally uses SHA-256/HMAC implemented in
//! this module plus an XOR stream derived from the shared pairing secret and a
//! nonce. This keeps plaintext off the wire and gives deterministic tests while
//! leaving a small, documented seam for CHUNK-07 to replace with hardened key
//! management/AEAD without changing the sync-store protocol.
//!
//! Conflict policy (resolved U5): same-path divergent edits use last-writer-wins
//! and always retain the losing version as a conflict sidecar. Manual recovery
//! is represented explicitly by [`ManualConflictResolution`]; callers must pick
//! a branch instead of silently discarding data.

use crate::catalog::{
    ContentHash, ManifestId, ProjectId, TreeEntry, TreeEntryKind, TreeManifest,
    CATALOG_MANIFEST_FORMAT_VERSION,
};
use crate::foundation::{MachineId, Migration, MigrationError, MigrationRunner, Platform};
use crate::policy::{Action, PlatformPin, Policy};
use crate::watcher::{
    ContentSyncDisposition, EventKind, FsEvent, IndexedSnapshot, PolicyMetadata, SnapshotEntry,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const MODULE_NAME: &str = "sync";
pub const SYNC_PROTOCOL_VERSION: &str = "sync-protocol-v1";
pub const SYNC_ENVELOPE_VERSION: &str = "sync-envelope-v1";
pub const SYNC_OPERATION_FORMAT_VERSION: &str = "sync-operation-lines-v1";
pub const SYNC_STORE_DIRECTORY: &str = "sync_store_v1";
pub const SYNC_CONFLICT_SIDECAR_SUFFIX: &str = ".sync-conflict";
pub const SYNC_PARTIAL_SUFFIX: &str = ".sync-partial";
pub const SYNC_REMOTE_MANIFEST_MACHINE_ID: &str = "remote-manifest";
static APPEND_TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const U10_PRODUCTION_BACKEND_RATIONALE: &str = "production sync uses an append-only file-backed store rooted at a configured shared directory, mounted network path, or object-store-like directory; local tempdirs exercise the same contract and loopback is test-only";
pub const TRANSPORT_CRYPTO_BASELINE_RATIONALE: &str = "CHUNK-05 stdlib-only baseline encrypts payload bytes with a shared-secret-derived XOR stream and authenticates envelopes with domain-separated HMAC-SHA256; CHUNK-07 may replace the primitive without changing envelope semantics";
pub const U5_CONFLICT_POLICY: &str =
    "last-writer-wins with conflict sidecar and explicit manual escape hatch";

pub const SYNC_MIGRATION_VERSION: &str = "sync_v1";
pub const SYNC_MIGRATION_DESCRIPTION: &str =
    "sync store schema; machines, manifests, encrypted payloads, operation log, pending operations, backups";
pub const SYNC_MACHINES_TABLE: &str = "sync_machines";
pub const SYNC_MANIFESTS_TABLE: &str = "sync_manifests";
pub const SYNC_CONTENT_PAYLOADS_TABLE: &str = "sync_content_payloads";
pub const SYNC_OPERATION_LOG_TABLE: &str = "sync_operation_log";
pub const SYNC_PENDING_OPERATIONS_TABLE: &str = "sync_pending_operations";
pub const SYNC_BACKUPS_TABLE: &str = "sync_backups";
pub const SYNC_MIGRATION_TABLES: &[&str] = &[
    SYNC_MACHINES_TABLE,
    SYNC_MANIFESTS_TABLE,
    SYNC_CONTENT_PAYLOADS_TABLE,
    SYNC_OPERATION_LOG_TABLE,
    SYNC_PENDING_OPERATIONS_TABLE,
    SYNC_BACKUPS_TABLE,
];
pub const SYNC_MIGRATION_UP_SQL: &[&str] = &[
    concat!(
        "CREATE TABLE sync_machines (",
        "machine_id TEXT PRIMARY KEY, ",
        "platform_os TEXT NOT NULL, ",
        "platform_arch TEXT NOT NULL, ",
        "secret_digest TEXT NOT NULL, ",
        "enrolled_at_logical_millis INTEGER NOT NULL, ",
        "last_seen_logical_millis INTEGER NOT NULL);"
    ),
    concat!(
        "CREATE TABLE sync_manifests (",
        "manifest_id TEXT PRIMARY KEY, ",
        "project_id TEXT NOT NULL, ",
        "payload_id TEXT NOT NULL, ",
        "operation_id TEXT NOT NULL, ",
        "created_logical_millis INTEGER NOT NULL);"
    ),
    concat!(
        "CREATE TABLE sync_content_payloads (",
        "content_hash TEXT PRIMARY KEY, ",
        "payload_id TEXT NOT NULL, ",
        "size_bytes INTEGER NOT NULL, ",
        "operation_id TEXT NOT NULL, ",
        "created_logical_millis INTEGER NOT NULL);"
    ),
    concat!(
        "CREATE TABLE sync_operation_log (",
        "operation_id TEXT PRIMARY KEY, ",
        "sequence INTEGER NOT NULL, ",
        "project_id TEXT NOT NULL, ",
        "machine_id TEXT NOT NULL, ",
        "kind TEXT NOT NULL, ",
        "path TEXT NOT NULL, ",
        "previous_path TEXT, ",
        "content_hash TEXT, ",
        "manifest_id TEXT, ",
        "payload_id TEXT, ",
        "modified_unix_millis INTEGER NOT NULL, ",
        "permissions INTEGER, ",
        "symlink_target TEXT, ",
        "policy_branch TEXT, ",
        "git_metadata INTEGER NOT NULL);"
    ),
    concat!(
        "CREATE TABLE sync_pending_operations (",
        "operation_id TEXT PRIMARY KEY, ",
        "queued_logical_millis INTEGER NOT NULL, ",
        "operation_payload TEXT NOT NULL);"
    ),
    concat!(
        "CREATE TABLE sync_backups (",
        "backup_id TEXT PRIMARY KEY, ",
        "source_root TEXT NOT NULL, ",
        "backup_root TEXT NOT NULL, ",
        "created_logical_millis INTEGER NOT NULL);"
    ),
];
pub const SYNC_MIGRATION_DOWN_SQL: &[&str] = &[
    "DROP TABLE sync_backups;",
    "DROP TABLE sync_pending_operations;",
    "DROP TABLE sync_operation_log;",
    "DROP TABLE sync_content_payloads;",
    "DROP TABLE sync_manifests;",
    "DROP TABLE sync_machines;",
];

type SyncResult<T> = Result<T, SyncStoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStoreError {
    InvalidConfig(String),
    AuthenticationFailed(String),
    Integrity(String),
    Io(String),
    Decode(String),
    Missing(String),
    Conflict(String),
    Policy(String),
}

impl SyncStoreError {
    fn invalid_config(message: impl Into<String>) -> Self {
        Self::InvalidConfig(message.into())
    }

    fn authentication(message: impl Into<String>) -> Self {
        Self::AuthenticationFailed(message.into())
    }

    fn integrity(message: impl Into<String>) -> Self {
        Self::Integrity(message.into())
    }

    fn decode(message: impl Into<String>) -> Self {
        Self::Decode(message.into())
    }

    fn io(action: &str, path: &Path, error: io::Error) -> Self {
        Self::Io(format!("{action} `{}` failed: {error}", path.display()))
    }
}

impl fmt::Display for SyncStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "SYNC_CONFIG_INVALID: {message}"),
            Self::AuthenticationFailed(message) => {
                write!(formatter, "SYNC_AUTHENTICATION_FAILED: {message}")
            }
            Self::Integrity(message) => write!(formatter, "SYNC_INTEGRITY_FAILED: {message}"),
            Self::Io(message) => write!(formatter, "SYNC_IO: {message}"),
            Self::Decode(message) => write!(formatter, "SYNC_DECODE: {message}"),
            Self::Missing(message) => write!(formatter, "SYNC_MISSING: {message}"),
            Self::Conflict(message) => write!(formatter, "SYNC_CONFLICT: {message}"),
            Self::Policy(message) => write!(formatter, "SYNC_POLICY: {message}"),
        }
    }
}

impl Error for SyncStoreError {}

#[derive(Clone, PartialEq, Eq)]
pub struct SharedSecret {
    key: Vec<u8>,
}

impl SharedSecret {
    pub fn from_pairing_token(token: impl AsRef<str>) -> SyncResult<Self> {
        let token = token.as_ref().trim();
        if token.is_empty() {
            return Err(SyncStoreError::invalid_config(
                "pairing token must not be empty",
            ));
        }
        let mut material = Vec::new();
        push_domain(&mut material, b"sync:pairing-secret:v1");
        push_len_prefixed(&mut material, token.as_bytes());
        Ok(Self {
            key: sha256(&material).to_vec(),
        })
    }

    pub fn from_key_bytes(key: impl AsRef<[u8]>) -> SyncResult<Self> {
        let key = key.as_ref();
        if key.is_empty() {
            return Err(SyncStoreError::invalid_config(
                "shared secret key bytes must not be empty",
            ));
        }
        Ok(Self {
            key: sha256(key).to_vec(),
        })
    }

    pub fn digest_hex(&self) -> String {
        stable_digest_hex(b"sync:secret-digest:v1", &self.key)
    }

    fn key(&self) -> &[u8] {
        &self.key
    }
}

impl fmt::Debug for SharedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedSecret")
            .field("digest", &self.digest_hex())
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineEnrollment {
    pub machine_id: String,
    pub platform: Platform,
    pub secret_digest: String,
    pub enrolled_at_logical_millis: u64,
    pub last_seen_logical_millis: u64,
}

impl MachineEnrollment {
    pub fn enroll(
        machine_source: &str,
        platform: Platform,
        secret: &SharedSecret,
        logical_millis: u64,
    ) -> SyncResult<Self> {
        let machine_id = app_scoped_machine_id(machine_source)?;
        Ok(Self {
            machine_id,
            platform,
            secret_digest: secret.digest_hex(),
            enrolled_at_logical_millis: logical_millis,
            last_seen_logical_millis: logical_millis,
        })
    }

    pub fn authenticate(&self, secret: &SharedSecret) -> SyncResult<()> {
        if constant_time_eq(self.secret_digest.as_bytes(), secret.digest_hex().as_bytes()) {
            Ok(())
        } else {
            Err(SyncStoreError::authentication(
                "pairing secret does not match enrolled machine",
            ))
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairingRegistry {
    machines: BTreeMap<String, MachineEnrollment>,
}

impl PairingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enroll_machine(&mut self, enrollment: MachineEnrollment) -> SyncResult<()> {
        if let Some(existing) = self.machines.get(&enrollment.machine_id) {
            if existing.secret_digest != enrollment.secret_digest {
                return Err(SyncStoreError::authentication(
                    "machine id is already enrolled with a different pairing secret",
                ));
            }
        }
        self.machines
            .insert(enrollment.machine_id.clone(), enrollment);
        Ok(())
    }

    pub fn authenticate(&self, machine_id: &str, secret: &SharedSecret) -> SyncResult<()> {
        let enrollment = self
            .machines
            .get(machine_id)
            .ok_or_else(|| SyncStoreError::authentication("machine is not enrolled"))?;
        enrollment.authenticate(secret)
    }

    pub fn machines(&self) -> impl Iterator<Item = &MachineEnrollment> {
        self.machines.values()
    }
}

pub fn app_scoped_machine_id(source: &str) -> SyncResult<String> {
    let source = source.trim();
    if source.is_empty() {
        return Err(SyncStoreError::invalid_config(
            "machine id source must not be empty",
        ));
    }
    if MachineId::is_app_scoped_value(source) {
        return Ok(source.to_owned());
    }
    MachineId::derive_app_scoped(source).ok_or_else(|| {
        SyncStoreError::invalid_config("machine id source could not derive app-scoped id")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportMode {
    Production,
    LocalHarness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportBackend {
    FileBackedSharedDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportSecurity {
    pub authenticated: bool,
    pub encrypted: bool,
}

impl TransportSecurity {
    pub const fn authenticated_encrypted() -> Self {
        Self {
            authenticated: true,
            encrypted: true,
        }
    }

    pub const fn plaintext_forbidden() -> Self {
        Self {
            authenticated: false,
            encrypted: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSecurityConfig {
    pub backend: TransportBackend,
    pub mode: TransportMode,
    pub shared_root: PathBuf,
    pub authorized_machine_ids: Vec<String>,
    pub pairing_secret: SharedSecret,
    pub security: TransportSecurity,
}

impl EndpointSecurityConfig {
    pub fn file_backed(
        mode: TransportMode,
        shared_root: impl Into<PathBuf>,
        authorized_machine_ids: Vec<String>,
        pairing_secret: SharedSecret,
    ) -> Self {
        Self {
            backend: TransportBackend::FileBackedSharedDirectory,
            mode,
            shared_root: shared_root.into(),
            authorized_machine_ids,
            pairing_secret,
            security: TransportSecurity::authenticated_encrypted(),
        }
    }

    pub fn validate(&self) -> SyncResult<()> {
        if self.shared_root.as_os_str().is_empty() {
            return Err(SyncStoreError::invalid_config(
                "sync shared root must not be empty",
            ));
        }
        if self.mode == TransportMode::Production && !self.shared_root.is_absolute() {
            return Err(SyncStoreError::invalid_config(
                "production sync shared root must be an absolute mounted/network/object-store path",
            ));
        }
        if !self.security.authenticated || !self.security.encrypted {
            return Err(SyncStoreError::invalid_config(
                "production sync transport must authenticate and encrypt every envelope",
            ));
        }
        if self.authorized_machine_ids.is_empty() {
            return Err(SyncStoreError::invalid_config(
                "at least one authorized machine id is required",
            ));
        }
        for machine_id in &self.authorized_machine_ids {
            if !MachineId::is_app_scoped_value(machine_id) {
                return Err(SyncStoreError::invalid_config(format!(
                    "authorized machine id `{machine_id}` is not app-scoped"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadKind {
    Manifest,
    Content,
    Operation,
    Generic(String),
}

impl PayloadKind {
    pub fn generic(payload_type: impl Into<String>) -> SyncResult<Self> {
        let payload_type = payload_type.into();
        if payload_type.trim().is_empty() {
            return Err(SyncStoreError::invalid_config(
                "generic payload type must not be empty",
            ));
        }
        Ok(Self::Generic(payload_type))
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Manifest => "manifest".to_owned(),
            Self::Content => "content".to_owned(),
            Self::Operation => "operation".to_owned(),
            Self::Generic(payload_type) => format!("generic:{payload_type}"),
        }
    }

    fn from_wire(value: &str) -> SyncResult<Self> {
        match value {
            "manifest" => Ok(Self::Manifest),
            "content" => Ok(Self::Content),
            "operation" => Ok(Self::Operation),
            generic if generic.starts_with("generic:") => {
                let payload_type = generic.trim_start_matches("generic:");
                Self::generic(payload_type)
            }
            other => Err(SyncStoreError::decode(format!(
                "unknown payload kind `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadEnvelope {
    pub version: String,
    pub kind: PayloadKind,
    pub sender_machine_id: String,
    pub recipient_machine_id: Option<String>,
    pub nonce: [u8; 16],
    pub ciphertext: Vec<u8>,
    pub mac_hex: String,
}

impl PayloadEnvelope {
    pub fn seal(
        kind: PayloadKind,
        sender_machine_id: impl Into<String>,
        recipient_machine_id: Option<String>,
        plaintext: &[u8],
        secret: &SharedSecret,
    ) -> SyncResult<Self> {
        let sender_machine_id = sender_machine_id.into();
        let nonce = derive_nonce(&kind, &sender_machine_id, recipient_machine_id.as_deref(), plaintext, secret);
        Self::seal_with_nonce(
            kind,
            sender_machine_id,
            recipient_machine_id,
            plaintext,
            secret,
            nonce,
        )
    }

    pub fn seal_with_nonce(
        kind: PayloadKind,
        sender_machine_id: impl Into<String>,
        recipient_machine_id: Option<String>,
        plaintext: &[u8],
        secret: &SharedSecret,
        nonce: [u8; 16],
    ) -> SyncResult<Self> {
        let sender_machine_id = sender_machine_id.into();
        if sender_machine_id.trim().is_empty() {
            return Err(SyncStoreError::invalid_config(
                "envelope sender machine id must not be empty",
            ));
        }
        let mut ciphertext = plaintext.to_vec();
        xor_stream_in_place(&mut ciphertext, secret, &nonce);
        let mac = envelope_mac(
            secret,
            SYNC_ENVELOPE_VERSION,
            &kind,
            &sender_machine_id,
            recipient_machine_id.as_deref(),
            &nonce,
            &ciphertext,
        );
        Ok(Self {
            version: SYNC_ENVELOPE_VERSION.to_owned(),
            kind,
            sender_machine_id,
            recipient_machine_id,
            nonce,
            ciphertext,
            mac_hex: hex_bytes(&mac),
        })
    }

    pub fn open(&self, secret: &SharedSecret) -> SyncResult<Vec<u8>> {
        if self.version != SYNC_ENVELOPE_VERSION {
            return Err(SyncStoreError::decode(format!(
                "unsupported envelope version `{}`",
                self.version
            )));
        }
        let expected_mac = envelope_mac(
            secret,
            &self.version,
            &self.kind,
            &self.sender_machine_id,
            self.recipient_machine_id.as_deref(),
            &self.nonce,
            &self.ciphertext,
        );
        let expected_mac_hex = hex_bytes(&expected_mac);
        if !constant_time_eq(expected_mac_hex.as_bytes(), self.mac_hex.as_bytes()) {
            return Err(SyncStoreError::integrity(
                "envelope MAC did not verify with shared secret",
            ));
        }
        let mut plaintext = self.ciphertext.clone();
        xor_stream_in_place(&mut plaintext, secret, &self.nonce);
        Ok(plaintext)
    }

    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let mut output = String::new();
        push_kv(&mut output, "version", &self.version);
        push_kv(&mut output, "kind", &self.kind.as_wire());
        push_kv(&mut output, "sender", &self.sender_machine_id);
        push_optional_kv(
            &mut output,
            "recipient",
            self.recipient_machine_id.as_deref(),
        );
        push_kv(&mut output, "nonce", &hex_bytes(&self.nonce));
        push_kv(&mut output, "ciphertext", &hex_bytes(&self.ciphertext));
        push_kv(&mut output, "mac", &self.mac_hex);
        output.into_bytes()
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> SyncResult<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|error| SyncStoreError::decode(format!("envelope is not UTF-8: {error}")))?;
        let fields = parse_kv_lines(text)?;
        let version = required_field(&fields, "version")?.to_owned();
        let kind = PayloadKind::from_wire(required_field(&fields, "kind")?)?;
        let sender_machine_id = required_field(&fields, "sender")?.to_owned();
        let recipient_machine_id = optional_field(&fields, "recipient")?;
        let nonce_bytes = decode_hex(required_field(&fields, "nonce")?)?;
        if nonce_bytes.len() != 16 {
            return Err(SyncStoreError::decode("envelope nonce must be 16 bytes"));
        }
        let mut nonce = [0_u8; 16];
        nonce.copy_from_slice(&nonce_bytes);
        let ciphertext = decode_hex(required_field(&fields, "ciphertext")?)?;
        let mac_hex = required_field(&fields, "mac")?.to_owned();
        Ok(Self {
            version,
            kind,
            sender_machine_id,
            recipient_machine_id,
            nonce,
            ciphertext,
            mac_hex,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FileBackedSyncStore {
    root: PathBuf,
    project_id: ProjectId,
    local_machine_id: String,
    authorized_machine_ids: BTreeSet<String>,
    secret: SharedSecret,
}

impl FileBackedSyncStore {
    pub fn new(
        config: EndpointSecurityConfig,
        project_id: impl Into<ProjectId>,
        local_machine_id: impl Into<String>,
    ) -> SyncResult<Self> {
        config.validate()?;
        let local_machine_id = local_machine_id.into();
        let authorized_machine_ids = config
            .authorized_machine_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if !authorized_machine_ids.contains(&local_machine_id) {
            return Err(SyncStoreError::authentication(
                "local machine id is not authorized for sync endpoint",
            ));
        }
        let store = Self {
            root: config.shared_root,
            project_id: project_id.into(),
            local_machine_id,
            authorized_machine_ids,
            secret: config.pairing_secret,
        };
        store.ensure_directories()?;
        Ok(store)
    }

    pub fn shared_root(&self) -> &Path {
        &self.root
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn local_machine_id(&self) -> &str {
        &self.local_machine_id
    }

    pub fn put_manifest(&self, manifest: &TreeManifest) -> SyncResult<PayloadId> {
        if manifest.project_id != self.project_id {
            return Err(SyncStoreError::integrity(format!(
                "manifest `{}` belongs to project `{}`, not `{}`",
                manifest.id, manifest.project_id, self.project_id
            )));
        }
        let payload_id = manifest.id.clone();
        self.persist_payload(
            PayloadKind::Manifest,
            &payload_id,
            "manifest",
            manifest.serialize_deterministic().as_bytes(),
        )?;
        Ok(payload_id)
    }
    pub fn fetch_manifest(&self, manifest_id: &str) -> SyncResult<TreeManifest> {
        let bytes = self.read_payload(PayloadKind::Manifest, manifest_id, "manifest")?;
        let manifest = deserialize_manifest(&bytes)?;
        if manifest.id != manifest_id {
            return Err(SyncStoreError::integrity(format!(
                "manifest payload id `{manifest_id}` contained manifest `{}`",
                manifest.id
            )));
        }
        if manifest.project_id != self.project_id {
            return Err(SyncStoreError::integrity(format!(
                "manifest `{manifest_id}` belongs to project `{}`, not `{}`",
                manifest.project_id, self.project_id
            )));
        }
        Ok(manifest)
    }


    pub fn put_content_blob(&self, content_hash: &str, bytes: &[u8]) -> SyncResult<PayloadId> {
        self.persist_payload(PayloadKind::Content, content_hash, "blob", bytes)?;
        Ok(content_hash.to_owned())
    }

    pub fn put_generic_payload(&self, payload_type: &str, bytes: &[u8]) -> SyncResult<PayloadId> {
        let kind = PayloadKind::generic(payload_type.to_owned())?;
        let payload_id = sync_generic_payload_id(payload_type, bytes);
        self.persist_payload(kind, &payload_id, "payload", bytes)?;
        Ok(payload_id)
    }

    pub fn fetch_content_blob(&self, content_hash: &str) -> SyncResult<Vec<u8>> {
        self.read_payload(PayloadKind::Content, content_hash, "blob")
    }

    pub fn fetch_generic_payload(&self, payload_type: &str, payload_id: &str) -> SyncResult<Vec<u8>> {
        self.read_payload(
            PayloadKind::generic(payload_type.to_owned())?,
            payload_id,
            "payload",
        )
    }

    pub fn append_operation(&self, operation: &OperationRecord) -> SyncResult<PayloadId> {
        if operation.project_id != self.project_id {
            return Err(SyncStoreError::integrity(format!(
                "operation `{}` belongs to project `{}`, not `{}`",
                operation.id, operation.project_id, self.project_id
            )));
        }
        if operation.machine_id != self.local_machine_id {
            return Err(SyncStoreError::authentication(format!(
                "operation `{}` was authored by `{}`, not local machine `{}`",
                operation.id, operation.machine_id, self.local_machine_id
            )));
        }
        let payload_id = operation.id.clone();
        let envelope = PayloadEnvelope::seal(
            PayloadKind::Operation,
            self.local_machine_id.clone(),
            None,
            operation.serialize_deterministic().as_bytes(),
            &self.secret,
        )?;
        let path = self.operation_path(&payload_id);
        write_append_only(&path, &envelope.to_wire_bytes())?;
        Ok(payload_id)
    }

    pub fn load_operation_log(&self) -> SyncResult<Vec<OperationRecord>> {
        let mut operation_paths = sorted_files(&self.operation_dir())?;
        operation_paths.sort();
        let mut operations = Vec::new();
        for path in operation_paths {
            if path.extension().and_then(|extension| extension.to_str()) != Some("syncop") {
                continue;
            }
            let Some(operation_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty())
            else {
                continue;
            };
            let bytes = fs::read(&path).map_err(|error| SyncStoreError::io("read", &path, error))?;
            let envelope = PayloadEnvelope::from_wire_bytes(&bytes)?;
            self.validate_envelope_for_read(&envelope, &PayloadKind::Operation, operation_id)?;
            let plaintext = envelope.open(&self.secret)?;
            let operation = OperationRecord::deserialize(&plaintext)?;
            if operation.id != operation_id {
                return Err(SyncStoreError::integrity(format!(
                    "operation file `{}` filename stem `{operation_id}` does not match contained operation id `{}`",
                    path.display(),
                    operation.id
                )));
            }
            self.validate_loaded_operation(&operation, &envelope)?;
            operations.push(operation);
        }
        operations.sort_by(compare_operations);
        Ok(operations)
    }

    fn persist_payload(
        &self,
        kind: PayloadKind,
        payload_id: &str,
        extension: &str,
        plaintext: &[u8],
    ) -> SyncResult<()> {
        self.validate_payload_identity(&kind, payload_id, plaintext)?;
        let envelope = PayloadEnvelope::seal(
            kind.clone(),
            self.local_machine_id.clone(),
            None,
            plaintext,
            &self.secret,
        )?;
        let envelope_bytes = envelope.to_wire_bytes();
        let path = self.payload_path(payload_id, extension);
        self.write_payload_allowing_idempotent_plaintext(
            &path,
            &envelope_bytes,
            &kind,
            payload_id,
            plaintext,
        )
    }

    fn read_payload(&self, kind: PayloadKind, payload_id: &str, extension: &str) -> SyncResult<Vec<u8>> {
        let path = self.payload_path(payload_id, extension);
        let bytes = fs::read(&path).map_err(|error| SyncStoreError::io("read", &path, error))?;
        let envelope = PayloadEnvelope::from_wire_bytes(&bytes)?;
        self.validate_envelope_for_read(&envelope, &kind, payload_id)?;
        let plaintext = envelope.open(&self.secret)?;
        self.validate_payload_identity(&kind, payload_id, &plaintext)?;
        Ok(plaintext)
    }

    fn validate_payload_identity(
        &self,
        kind: &PayloadKind,
        payload_id: &str,
        plaintext: &[u8],
    ) -> SyncResult<()> {
        let (expected_payload_id, label) = match kind {
            PayloadKind::Content => (sync_content_hash(plaintext), "content"),
            PayloadKind::Generic(payload_type) => (sync_generic_payload_id(payload_type, plaintext), "generic"),
            PayloadKind::Manifest | PayloadKind::Operation => return Ok(()),
        };
        if expected_payload_id == payload_id {
            Ok(())
        } else {
            Err(SyncStoreError::integrity(format!(
                "{label} payload id `{payload_id}` does not match plaintext id `{expected_payload_id}`"
            )))
        }
    }

    fn write_payload_allowing_idempotent_plaintext(
        &self,
        path: &Path,
        envelope_bytes: &[u8],
        kind: &PayloadKind,
        payload_id: &str,
        plaintext: &[u8],
    ) -> SyncResult<()> {
        match write_append_only(path, envelope_bytes) {
            Ok(()) => Ok(()),
            Err(SyncStoreError::Integrity(_)) => {
                self.accept_idempotent_existing_payload(path, envelope_bytes, kind, payload_id, plaintext)
            }
            Err(error) => Err(error),
        }
    }

    fn accept_idempotent_existing_payload(
        &self,
        path: &Path,
        envelope_bytes: &[u8],
        kind: &PayloadKind,
        payload_id: &str,
        plaintext: &[u8],
    ) -> SyncResult<()> {
        let existing = fs::read(path).map_err(|error| SyncStoreError::io("read", path, error))?;
        if existing == envelope_bytes {
            return Ok(());
        }
        let existing_envelope = PayloadEnvelope::from_wire_bytes(&existing)?;
        self.validate_envelope_for_read(&existing_envelope, kind, payload_id)?;
        let existing_plaintext = existing_envelope.open(&self.secret)?;
        if existing_plaintext == plaintext {
            Ok(())
        } else {
            Err(SyncStoreError::integrity(format!(
                "append-only payload `{}` already exists with different plaintext",
                path.display()
            )))
        }
    }

    fn validate_envelope_for_read(
        &self,
        envelope: &PayloadEnvelope,
        expected_kind: &PayloadKind,
        payload_id: &str,
    ) -> SyncResult<()> {
        if &envelope.kind != expected_kind {
            return Err(SyncStoreError::decode(format!(
                "payload `{payload_id}` kind mismatch: expected {}, found {}",
                expected_kind.as_wire(),
                envelope.kind.as_wire()
            )));
        }
        self.validate_envelope_endpoint(envelope, payload_id)
    }

    fn validate_envelope_endpoint(
        &self,
        envelope: &PayloadEnvelope,
        payload_id: &str,
    ) -> SyncResult<()> {
        if !self
            .authorized_machine_ids
            .contains(&envelope.sender_machine_id)
        {
            return Err(SyncStoreError::authentication(format!(
                "payload `{payload_id}` sender `{}` is not authorized",
                envelope.sender_machine_id
            )));
        }
        if let Some(recipient) = &envelope.recipient_machine_id {
            if recipient != &self.local_machine_id {
                return Err(SyncStoreError::authentication(format!(
                    "payload `{payload_id}` recipient `{recipient}` does not match local machine `{}`",
                    self.local_machine_id
                )));
            }
        }
        Ok(())
    }

    fn validate_loaded_operation(
        &self,
        operation: &OperationRecord,
        envelope: &PayloadEnvelope,
    ) -> SyncResult<()> {
        if operation.project_id != self.project_id {
            return Err(SyncStoreError::integrity(format!(
                "operation `{}` belongs to project `{}`, not `{}`",
                operation.id, operation.project_id, self.project_id
            )));
        }
        if operation.machine_id != envelope.sender_machine_id {
            return Err(SyncStoreError::integrity(format!(
                "operation `{}` machine `{}` does not match envelope sender `{}`",
                operation.id, operation.machine_id, envelope.sender_machine_id
            )));
        }
        if !self.authorized_machine_ids.contains(&operation.machine_id) {
            return Err(SyncStoreError::authentication(format!(
                "operation `{}` sender `{}` is not authorized",
                operation.id, operation.machine_id
            )));
        }
        Ok(())
    }

    fn ensure_directories(&self) -> SyncResult<()> {
        for path in [self.payload_dir(), self.operation_dir()] {
            fs::create_dir_all(&path)
                .map_err(|error| SyncStoreError::io("create directory", &path, error))?;
        }
        Ok(())
    }

    fn project_root(&self) -> PathBuf {
        self.root
            .join(SYNC_STORE_DIRECTORY)
            .join("projects")
            .join(safe_path_component(&self.project_id))
    }

    fn payload_dir(&self) -> PathBuf {
        self.project_root().join("payloads")
    }

    fn operation_dir(&self) -> PathBuf {
        self.project_root().join("oplog")
    }

    fn payload_path(&self, payload_id: &str, extension: &str) -> PathBuf {
        self.payload_dir()
            .join(format!("{}.{}", safe_path_component(payload_id), extension))
    }

    fn operation_path(&self, operation_id: &str) -> PathBuf {
        self.operation_dir()
            .join(format!("{}.syncop", safe_path_component(operation_id)))
    }
}

pub type PayloadId = String;
pub type OperationId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationKind {
    PutManifest,
    PutContent,
    DeletePath,
    MovePath,
    PermissionChanged,
    SymlinkChanged,
    RebuildLocally,
    IgnoreKnown,
    PlatformPinRedirected,
    GitMetadataLocalOnly,
    ConflictSidecar,
    GenericPayload,
    PolicyMarker,
}

impl OperationKind {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::PutManifest => "put-manifest",
            Self::PutContent => "put-content",
            Self::DeletePath => "delete-path",
            Self::MovePath => "move-path",
            Self::PermissionChanged => "permission-changed",
            Self::SymlinkChanged => "symlink-changed",
            Self::RebuildLocally => "rebuild-locally",
            Self::IgnoreKnown => "ignore-known",
            Self::PlatformPinRedirected => "platform-pin-redirected",
            Self::GitMetadataLocalOnly => "git-metadata-local-only",
            Self::ConflictSidecar => "conflict-sidecar",
            Self::GenericPayload => "generic-payload",
            Self::PolicyMarker => "policy-marker",
        }
    }

    fn from_wire(value: &str) -> SyncResult<Self> {
        match value {
            "put-manifest" => Ok(Self::PutManifest),
            "put-content" => Ok(Self::PutContent),
            "delete-path" => Ok(Self::DeletePath),
            "move-path" => Ok(Self::MovePath),
            "permission-changed" => Ok(Self::PermissionChanged),
            "symlink-changed" => Ok(Self::SymlinkChanged),
            "rebuild-locally" => Ok(Self::RebuildLocally),
            "ignore-known" => Ok(Self::IgnoreKnown),
            "platform-pin-redirected" => Ok(Self::PlatformPinRedirected),
            "git-metadata-local-only" => Ok(Self::GitMetadataLocalOnly),
            "conflict-sidecar" => Ok(Self::ConflictSidecar),
            "generic-payload" => Ok(Self::GenericPayload),
            "policy-marker" => Ok(Self::PolicyMarker),
            other => Err(SyncStoreError::decode(format!(
                "unknown operation kind `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyBranch {
    Sync,
    Ignore,
    RebuildLocally,
    PlatformPin { os_family: String, architecture: String },
    GitMetadataLocalOnly,
}

impl PolicyBranch {
    pub fn from_metadata(path: &str, metadata: &PolicyMetadata) -> Self {
        if is_git_metadata_path(path) {
            return Self::GitMetadataLocalOnly;
        }
        match &metadata.action {
            Action::Sync => Self::Sync,
            Action::Ignore => Self::Ignore,
            Action::RebuildLocally => Self::RebuildLocally,
            Action::PlatformPin(pin) => Self::from_platform_pin(pin),
        }
    }

    pub fn from_entry(entry: &SnapshotEntry) -> Self {
        Self::from_metadata(entry.path(), &entry.policy)
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Sync => "sync".to_owned(),
            Self::Ignore => "ignore".to_owned(),
            Self::RebuildLocally => "rebuild-locally".to_owned(),
            Self::PlatformPin {
                os_family,
                architecture,
            } => format!("platform-pin:{os_family}:{architecture}"),
            Self::GitMetadataLocalOnly => "git-metadata-local-only".to_owned(),
        }
    }

    pub fn matches_platform(&self, platform: &Platform) -> bool {
        match self {
            Self::PlatformPin {
                os_family,
                architecture,
            } => os_family == platform.os_family.as_str() && architecture == platform.architecture.as_str(),
            Self::Sync | Self::Ignore | Self::RebuildLocally | Self::GitMetadataLocalOnly => true,
        }
    }

    fn from_platform_pin(pin: &PlatformPin) -> Self {
        Self::PlatformPin {
            os_family: pin.os_family.as_str().to_owned(),
            architecture: pin.architecture.as_str().to_owned(),
        }
    }

    fn from_wire(value: &str) -> SyncResult<Self> {
        match value {
            "sync" => Ok(Self::Sync),
            "ignore" => Ok(Self::Ignore),
            "rebuild-locally" => Ok(Self::RebuildLocally),
            "git-metadata-local-only" => Ok(Self::GitMetadataLocalOnly),
            platform_pin if platform_pin.starts_with("platform-pin:") => {
                let mut parts = platform_pin.splitn(3, ':');
                let _prefix = parts.next();
                let os_family = parts
                    .next()
                    .ok_or_else(|| SyncStoreError::decode("platform pin is missing os"))?;
                let architecture = parts
                    .next()
                    .ok_or_else(|| SyncStoreError::decode("platform pin is missing architecture"))?;
                Ok(Self::PlatformPin {
                    os_family: os_family.to_owned(),
                    architecture: architecture.to_owned(),
                })
            }
            other => Err(SyncStoreError::decode(format!(
                "unknown policy branch `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDraft {
    pub sequence: u64,
    pub project_id: ProjectId,
    pub machine_id: String,
    pub kind: OperationKind,
    pub path: String,
    pub previous_path: Option<String>,
    pub content_hash: Option<ContentHash>,
    pub manifest_id: Option<ManifestId>,
    pub payload_id: Option<PayloadId>,
    pub modified_unix_millis: u64,
    pub permissions: Option<u32>,
    pub symlink_target: Option<String>,
    pub policy_branch: Option<PolicyBranch>,
    pub git_metadata: bool,
}

impl OperationDraft {
    pub fn new(
        sequence: u64,
        project_id: impl Into<ProjectId>,
        machine_id: impl Into<String>,
        kind: OperationKind,
        path: impl Into<String>,
    ) -> Self {
        let path = path.into();
        Self {
            sequence,
            project_id: project_id.into(),
            machine_id: machine_id.into(),
            kind,
            git_metadata: is_git_metadata_path(&path),
            path,
            previous_path: None,
            content_hash: None,
            manifest_id: None,
            payload_id: None,
            modified_unix_millis: 0,
            permissions: None,
            symlink_target: None,
            policy_branch: None,
        }
    }

    pub fn previous_path(mut self, previous_path: impl Into<String>) -> Self {
        self.previous_path = Some(previous_path.into());
        self
    }

    pub fn content_hash(mut self, content_hash: impl Into<ContentHash>) -> Self {
        self.content_hash = Some(content_hash.into());
        self
    }

    pub fn manifest_id(mut self, manifest_id: impl Into<ManifestId>) -> Self {
        self.manifest_id = Some(manifest_id.into());
        self
    }

    pub fn payload_id(mut self, payload_id: impl Into<PayloadId>) -> Self {
        self.payload_id = Some(payload_id.into());
        self
    }

    pub fn modified_unix_millis(mut self, modified_unix_millis: u64) -> Self {
        self.modified_unix_millis = modified_unix_millis;
        self
    }

    pub fn permissions(mut self, permissions: u32) -> Self {
        self.permissions = Some(permissions);
        self
    }

    pub fn symlink_target(mut self, symlink_target: impl Into<String>) -> Self {
        self.symlink_target = Some(symlink_target.into());
        self
    }

    pub fn policy_branch(mut self, policy_branch: PolicyBranch) -> Self {
        if policy_branch == PolicyBranch::GitMetadataLocalOnly {
            self.git_metadata = true;
        }
        self.policy_branch = Some(policy_branch);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub id: OperationId,
    pub sequence: u64,
    pub project_id: ProjectId,
    pub machine_id: String,
    pub kind: OperationKind,
    pub path: String,
    pub previous_path: Option<String>,
    pub content_hash: Option<ContentHash>,
    pub manifest_id: Option<ManifestId>,
    pub payload_id: Option<PayloadId>,
    pub modified_unix_millis: u64,
    pub permissions: Option<u32>,
    pub symlink_target: Option<String>,
    pub policy_branch: Option<PolicyBranch>,
    pub git_metadata: bool,
}

impl OperationRecord {
    pub fn from_draft(draft: OperationDraft) -> Self {
        let mut record = Self {
            id: String::new(),
            sequence: draft.sequence,
            project_id: draft.project_id,
            machine_id: draft.machine_id,
            kind: draft.kind,
            path: draft.path,
            previous_path: draft.previous_path,
            content_hash: draft.content_hash,
            manifest_id: draft.manifest_id,
            payload_id: draft.payload_id,
            modified_unix_millis: draft.modified_unix_millis,
            permissions: draft.permissions,
            symlink_target: draft.symlink_target,
            policy_branch: draft.policy_branch,
            git_metadata: draft.git_metadata,
        };
        record.id = record.compute_id();
        record
    }

    pub fn from_watcher_event(
        project_id: impl Into<ProjectId>,
        machine_id: impl Into<String>,
        sequence: u64,
        event: &FsEvent,
    ) -> Self {
        let source_entry = event.after.as_ref().or(event.before.as_ref());
        let kind = match event.kind {
            EventKind::Created | EventKind::Edited => {
                operation_kind_for_created_or_edited_event(source_entry)
            }
            EventKind::Moved => OperationKind::MovePath,
            EventKind::Deleted => OperationKind::DeletePath,
            EventKind::PermissionChanged => OperationKind::PermissionChanged,
            EventKind::SymlinkChanged => OperationKind::SymlinkChanged,
            EventKind::PolicyChanged => OperationKind::PolicyMarker,
        };
        let mut draft = OperationDraft::new(sequence, project_id, machine_id, kind, event.path.clone());
        if let Some(previous_path) = &event.previous_path {
            draft = draft.previous_path(previous_path.clone());
        }
        if let Some(entry) = source_entry {
            if let Some(content_hash) = &entry.catalog_entry.content_hash {
                draft = draft.content_hash(content_hash.clone());
            }
            draft = draft
                .modified_unix_millis(entry.catalog_entry.modified_unix_millis)
                .permissions(entry.catalog_entry.permissions)
                .policy_branch(PolicyBranch::from_entry(entry));
            if let Some(symlink_target) = &entry.symlink_target {
                draft = draft.symlink_target(symlink_target.clone());
            }
        }
        Self::from_draft(draft)
    }

    pub fn serialize_deterministic(&self) -> String {
        let mut output = String::new();
        push_kv(&mut output, "format", SYNC_OPERATION_FORMAT_VERSION);
        push_kv(&mut output, "id", &self.id);
        self.push_identity_fields(&mut output);
        output
    }

    pub fn deserialize(bytes: &[u8]) -> SyncResult<Self> {
        let text = std::str::from_utf8(bytes).map_err(|error| {
            SyncStoreError::decode(format!("operation record is not UTF-8: {error}"))
        })?;
        let fields = parse_kv_lines(text)?;
        let format = required_field(&fields, "format")?;
        if format != SYNC_OPERATION_FORMAT_VERSION {
            return Err(SyncStoreError::decode(format!(
                "unsupported operation format `{format}`"
            )));
        }
        let kind = OperationKind::from_wire(required_field(&fields, "kind")?)?;
        let policy_branch = optional_field(&fields, "policy_branch")?
            .map(|value| PolicyBranch::from_wire(&value))
            .transpose()?;
        let record = Self {
            id: required_field(&fields, "id")?.to_owned(),
            sequence: parse_u64(required_field(&fields, "sequence")?, "sequence")?,
            project_id: required_field(&fields, "project_id")?.to_owned(),
            machine_id: required_field(&fields, "machine_id")?.to_owned(),
            kind,
            path: required_field(&fields, "path")?.to_owned(),
            previous_path: optional_field(&fields, "previous_path")?,
            content_hash: optional_field(&fields, "content_hash")?,
            manifest_id: optional_field(&fields, "manifest_id")?,
            payload_id: optional_field(&fields, "payload_id")?,
            modified_unix_millis: parse_u64(
                required_field(&fields, "modified_unix_millis")?,
                "modified_unix_millis",
            )?,
            permissions: optional_field(&fields, "permissions")?
                .map(|value| parse_u32(&value, "permissions"))
                .transpose()?,
            symlink_target: optional_field(&fields, "symlink_target")?,
            policy_branch,
            git_metadata: parse_bool(required_field(&fields, "git_metadata")?, "git_metadata")?,
        };
        let expected_id = record.compute_id();
        if record.id != expected_id {
            return Err(SyncStoreError::integrity(format!(
                "operation id mismatch: expected {expected_id}, found {}",
                record.id
            )));
        }
        Ok(record)
    }

    pub fn compute_id(&self) -> String {
        stable_digest_hex(
            b"sync:operation-id:v1",
            self.identity_material().as_bytes(),
        )
    }

    fn identity_material(&self) -> String {
        let mut output = String::new();
        self.push_identity_fields(&mut output);
        output
    }

    fn push_identity_fields(&self, output: &mut String) {
        push_kv(output, "sequence", &self.sequence.to_string());
        push_kv(output, "project_id", &self.project_id);
        push_kv(output, "machine_id", &self.machine_id);
        push_kv(output, "kind", self.kind.as_wire());
        push_kv(output, "path", &self.path);
        push_optional_kv(output, "previous_path", self.previous_path.as_deref());
        push_optional_kv(output, "content_hash", self.content_hash.as_deref());
        push_optional_kv(output, "manifest_id", self.manifest_id.as_deref());
        push_optional_kv(output, "payload_id", self.payload_id.as_deref());
        push_kv(
            output,
            "modified_unix_millis",
            &self.modified_unix_millis.to_string(),
        );
        let permissions = self.permissions.map(|value| value.to_string());
        push_optional_kv(output, "permissions", permissions.as_deref());
        push_optional_kv(output, "symlink_target", self.symlink_target.as_deref());
        let policy_branch = self.policy_branch.as_ref().map(PolicyBranch::as_wire);
        push_optional_kv(output, "policy_branch", policy_branch.as_deref());
        push_kv(output, "git_metadata", bool_wire(self.git_metadata));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthoritativeSyncStore {
    pub project_id: ProjectId,
    pub machines: BTreeMap<String, MachineEnrollment>,
    pub manifests: BTreeMap<ManifestId, TreeManifest>,
    pub content_payloads: BTreeMap<ContentHash, Vec<u8>>,
    pub generic_payloads: BTreeMap<PayloadId, Vec<u8>>,
    operations: Vec<OperationRecord>,
}

impl AuthoritativeSyncStore {
    pub fn new(project_id: impl Into<ProjectId>) -> Self {
        Self {
            project_id: project_id.into(),
            machines: BTreeMap::new(),
            manifests: BTreeMap::new(),
            content_payloads: BTreeMap::new(),
            generic_payloads: BTreeMap::new(),
            operations: Vec::new(),
        }
    }

    pub fn register_machine(&mut self, enrollment: MachineEnrollment) -> SyncResult<()> {
        if let Some(existing) = self.machines.get(&enrollment.machine_id) {
            if existing.secret_digest != enrollment.secret_digest {
                return Err(SyncStoreError::authentication(
                    "enrolled machine secret digest changed",
                ));
            }
        }
        self.machines
            .insert(enrollment.machine_id.clone(), enrollment);
        Ok(())
    }

    pub fn put_manifest(&mut self, manifest: TreeManifest) {
        self.manifests.insert(manifest.id.clone(), manifest);
    }

    pub fn put_content(&mut self, content_hash: impl Into<ContentHash>, bytes: Vec<u8>) {
        self.content_payloads.insert(content_hash.into(), bytes);
    }

    pub fn append_operation(&mut self, operation: OperationRecord) {
        self.operations.push(operation);
        self.operations.sort_by(compare_operations);
    }

    pub fn operation_log(&self) -> &[OperationRecord] {
        &self.operations
    }

    pub fn replay(&self) -> ReplayState {
        replay_operation_log(&self.operations)
    }

    pub fn backup(&self, label: &str) -> SyncStoreBackup {
        let mut material = Vec::new();
        push_len_prefixed(&mut material, self.project_id.as_bytes());
        push_len_prefixed(&mut material, label.as_bytes());
        for operation in &self.operations {
            push_len_prefixed(&mut material, operation.id.as_bytes());
        }
        SyncStoreBackup {
            backup_id: stable_digest_hex(b"sync:store-backup-id:v1", &material),
            label: label.to_owned(),
            project_id: self.project_id.clone(),
            machines: self.machines.clone(),
            manifests: self.manifests.clone(),
            content_payloads: self.content_payloads.clone(),
            generic_payloads: self.generic_payloads.clone(),
            operations: self.operations.clone(),
        }
    }

    pub fn restore(&mut self, backup: SyncStoreBackup) {
        self.project_id = backup.project_id;
        self.machines = backup.machines;
        self.manifests = backup.manifests;
        self.content_payloads = backup.content_payloads;
        self.generic_payloads = backup.generic_payloads;
        self.operations = backup.operations;
        self.operations.sort_by(compare_operations);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStoreBackup {
    pub backup_id: String,
    pub label: String,
    pub project_id: ProjectId,
    pub machines: BTreeMap<String, MachineEnrollment>,
    pub manifests: BTreeMap<ManifestId, TreeManifest>,
    pub content_payloads: BTreeMap<ContentHash, Vec<u8>>,
    pub generic_payloads: BTreeMap<PayloadId, Vec<u8>>,
    pub operations: Vec<OperationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayState {
    pub entries: BTreeMap<String, ReplayedEntry>,
    pub tombstones: BTreeMap<String, DeleteTombstone>,
    pub ignored_paths: BTreeSet<String>,
    pub rebuild_paths: BTreeSet<String>,
    pub platform_pinned_paths: BTreeSet<String>,
    pub git_metadata_paths: BTreeSet<String>,
    pub conflict_sidecars: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteTombstone {
    pub path: String,
    pub modified_unix_millis: u64,
    pub source_machine_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedEntry {
    pub path: String,
    /// For file entries, this is the source fingerprint carried by the operation
    /// log. It is not the manifest store blob id.
    pub content_hash: Option<ContentHash>,
    /// Store blob id associated with the source fingerprint when the operation carried one.
    pub store_blob_id: Option<ContentHash>,
    pub modified_unix_millis: u64,
    pub permissions: Option<u32>,
    pub symlink_target: Option<String>,
    pub source_machine_id: String,
}

impl ReplayedEntry {
    fn from_put_operation(operation: &OperationRecord) -> Self {
        Self {
            path: operation.path.clone(),
            content_hash: operation.content_hash.clone(),
            store_blob_id: operation.payload_id.clone(),
            modified_unix_millis: operation.modified_unix_millis,
            permissions: operation.permissions,
            symlink_target: operation.symlink_target.clone(),
            source_machine_id: operation.machine_id.clone(),
        }
    }

    fn from_move_operation(operation: &OperationRecord) -> Option<Self> {
        if operation.content_hash.is_none()
            && operation.permissions.is_none()
            && operation.symlink_target.is_none()
        {
            return None;
        }
        Some(Self {
            path: operation.path.clone(),
            content_hash: operation.content_hash.clone(),
            store_blob_id: operation.payload_id.clone(),
            modified_unix_millis: operation.modified_unix_millis,
            permissions: operation.permissions,
            symlink_target: operation.symlink_target.clone(),
            source_machine_id: operation.machine_id.clone(),
        })
    }

    fn from_metadata_operation(operation: &OperationRecord) -> Option<Self> {
        if operation.content_hash.is_none()
            && operation.permissions.is_none()
            && operation.symlink_target.is_none()
        {
            return None;
        }
        Some(Self {
            path: operation.path.clone(),
            content_hash: operation.content_hash.clone(),
            store_blob_id: operation.payload_id.clone(),
            modified_unix_millis: operation.modified_unix_millis,
            permissions: operation.permissions,
            symlink_target: operation.symlink_target.clone(),
            source_machine_id: operation.machine_id.clone(),
        })
    }
}

pub fn replay_operation_log(operations: &[OperationRecord]) -> ReplayState {
    let mut sorted = operations.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| compare_operations(left, right));
    let mut state = ReplayState::default();
    for operation in sorted {
        match operation.kind {
            OperationKind::PutManifest | OperationKind::GenericPayload | OperationKind::PolicyMarker => {}
            OperationKind::PutContent => {
                apply_replayed_entry(&mut state, ReplayedEntry::from_put_operation(operation));
            }
            OperationKind::DeletePath => {
                let tombstone = tombstone_from_operation(operation, &operation.path);
                apply_delete_tombstone(&mut state, tombstone);
            }
            OperationKind::MovePath => {
                let moved_entry = operation.previous_path.as_ref().and_then(|previous_path| {
                    let tombstone = tombstone_from_operation(operation, previous_path);
                    apply_delete_tombstone(&mut state, tombstone)
                });
                if let Some(mut entry) = moved_entry.or_else(|| ReplayedEntry::from_move_operation(operation)) {
                    entry.path = operation.path.clone();
                    entry.source_machine_id = operation.machine_id.clone();
                    if operation.content_hash.is_some() {
                        entry.content_hash = operation.content_hash.clone();
                    }
                    if operation.payload_id.is_some() {
                        entry.store_blob_id = operation.payload_id.clone();
                    }
                    entry.modified_unix_millis = operation.modified_unix_millis;
                    entry.permissions = operation.permissions.or(entry.permissions);
                    entry.symlink_target = operation.symlink_target.clone().or(entry.symlink_target);
                    apply_replayed_entry(&mut state, entry);
                }
            }
            OperationKind::PermissionChanged => {
                if let Some(entry) = state.entries.get_mut(&operation.path) {
                    if operation_metadata_can_update_entry(operation, entry) {
                        entry.permissions = operation.permissions;
                        entry.modified_unix_millis = operation.modified_unix_millis;
                        entry.source_machine_id = operation.machine_id.clone();
                    }
                } else if let Some(entry) = ReplayedEntry::from_metadata_operation(operation) {
                    apply_replayed_entry(&mut state, entry);
                }
            }
            OperationKind::SymlinkChanged => {
                if let Some(entry) = state.entries.get_mut(&operation.path) {
                    if operation_metadata_can_update_entry(operation, entry) {
                        if let Some(content_hash) = &operation.content_hash {
                            entry.content_hash = Some(content_hash.clone());
                        }
                        if operation.permissions.is_some() {
                            entry.permissions = operation.permissions;
                        }
                        if let Some(symlink_target) = &operation.symlink_target {
                            entry.symlink_target = Some(symlink_target.clone());
                        }
                        entry.modified_unix_millis = operation.modified_unix_millis;
                        entry.source_machine_id = operation.machine_id.clone();
                    }
                } else if let Some(entry) = ReplayedEntry::from_metadata_operation(operation) {
                    apply_replayed_entry(&mut state, entry);
                }
            }
            OperationKind::RebuildLocally => {
                state.rebuild_paths.insert(operation.path.clone());
            }
            OperationKind::IgnoreKnown => {
                state.ignored_paths.insert(operation.path.clone());
            }
            OperationKind::PlatformPinRedirected => {
                state.platform_pinned_paths.insert(operation.path.clone());
            }
            OperationKind::GitMetadataLocalOnly => {
                state.git_metadata_paths.insert(operation.path.clone());
            }
            OperationKind::ConflictSidecar => {
                state.conflict_sidecars.push(operation.path.clone());
            }
        }
    }
    state.conflict_sidecars.sort();
    state
}

fn apply_replayed_entry(state: &mut ReplayState, incoming: ReplayedEntry) {
    if tombstone_obsoletes_entry(state.tombstones.get(&incoming.path), &incoming) {
        return;
    }
    state.tombstones.remove(&incoming.path);
    match state.entries.get(&incoming.path).cloned() {
        Some(existing) if existing.content_hash != incoming.content_hash => {
            let (winner, loser) = replay_content_winner(existing, incoming);
            state.conflict_sidecars.push(conflict_sidecar_path(
                &winner.path,
                &loser.source_machine_id,
                loser.modified_unix_millis,
            ));
            state.entries.insert(winner.path.clone(), winner);
        }
        Some(existing) => {
            let winner = if replayed_entry_wins(&incoming, &existing) {
                incoming
            } else {
                existing
            };
            state.entries.insert(winner.path.clone(), winner);
        }
        None => {
            state.entries.insert(incoming.path.clone(), incoming);
        }
    }
}

fn apply_delete_tombstone(state: &mut ReplayState, tombstone: DeleteTombstone) -> Option<ReplayedEntry> {
    let loses_to_entry = state
        .entries
        .get(&tombstone.path)
        .map(|entry| !tombstone_obsoletes_entry(Some(&tombstone), entry))
        .unwrap_or(false);
    if loses_to_entry {
        return None;
    }
    let loses_to_tombstone = state
        .tombstones
        .get(&tombstone.path)
        .map(|existing| !tombstone_obsoletes_tombstone(&tombstone, existing))
        .unwrap_or(false);
    if loses_to_tombstone {
        return None;
    }
    let removed = state.entries.remove(&tombstone.path);
    state.tombstones.insert(tombstone.path.clone(), tombstone);
    removed
}

fn tombstone_from_operation(operation: &OperationRecord, path: &str) -> DeleteTombstone {
    DeleteTombstone {
        path: path.to_owned(),
        modified_unix_millis: operation.modified_unix_millis,
        source_machine_id: operation.machine_id.clone(),
    }
}

fn tombstone_obsoletes_entry(
    tombstone: Option<&DeleteTombstone>,
    entry: &ReplayedEntry,
) -> bool {
    tombstone
        .map(|tombstone| {
            // Delete and move records intentionally reuse the removed source
            // entry's modified time. A source-machine tie-break is meaningful
            // between two materialized entries, but it resurrects stale content
            // when an equal-time tombstone was replayed after the original put.
            // Treat equal mtimes as deleted/moved for replay, matching snapshot
            // tombstone handling below.
            tombstone.modified_unix_millis >= entry.modified_unix_millis
        })
        .unwrap_or(false)
}

fn tombstone_obsoletes_snapshot_entry(
    tombstone: Option<&DeleteTombstone>,
    entry: &SnapshotEntry,
    _local_machine_id: &str,
) -> bool {
    tombstone
        .map(|tombstone| {
            // A snapshot entry does not carry the machine id that last wrote
            // its mtime, so the local machine id is not a valid source-machine
            // tie-break against a remote delete/move tombstone. Watcher delete
            // and move records intentionally reuse the deleted source entry's
            // modified time; on an equal timestamp, prefer the tombstone so a
            // stale local path is deleted instead of re-uploaded.
            tombstone.modified_unix_millis >= entry.catalog_entry.modified_unix_millis
        })
        .unwrap_or(false)
}

fn tombstone_obsoletes_manifest_entry(
    remote_state: Option<&ReplayState>,
    entry: &TreeEntry,
) -> bool {
    let Some(remote_state) = remote_state else {
        return false;
    };
    tombstone_obsoletes_tree_entry(remote_state.tombstones.get(entry.path.as_str()), entry)
        || ancestor_tombstone_obsoletes_manifest_entry(remote_state, entry)
}

fn ancestor_tombstone_obsoletes_manifest_entry(
    remote_state: &ReplayState,
    entry: &TreeEntry,
) -> bool {
    let path = entry.path.as_str();
    let mut end = path.len();
    while let Some(separator_index) = path[..end].rfind('/') {
        let tombstone = remote_state.tombstones.get(&path[..separator_index]);
        if tombstone_obsoletes_tree_entry(tombstone, entry) {
            return true;
        }
        end = separator_index;
    }
    false
}

fn tombstone_obsoletes_tree_entry(
    tombstone: Option<&DeleteTombstone>,
    entry: &TreeEntry,
) -> bool {
    tombstone
        .map(|tombstone| tombstone.modified_unix_millis >= entry.modified_unix_millis)
        .unwrap_or(false)
}

fn tombstone_obsoletes_tombstone(left: &DeleteTombstone, right: &DeleteTombstone) -> bool {
    left.modified_unix_millis
        .cmp(&right.modified_unix_millis)
        .then_with(|| left.source_machine_id.cmp(&right.source_machine_id))
        != std::cmp::Ordering::Less
}


fn replay_content_winner(
    existing: ReplayedEntry,
    incoming: ReplayedEntry,
) -> (ReplayedEntry, ReplayedEntry) {
    if replayed_entry_wins(&existing, &incoming) {
        (existing, incoming)
    } else {
        (incoming, existing)
    }
}

fn replayed_entry_wins(left: &ReplayedEntry, right: &ReplayedEntry) -> bool {
    left.modified_unix_millis
        .cmp(&right.modified_unix_millis)
        .then_with(|| left.source_machine_id.cmp(&right.source_machine_id))
        == std::cmp::Ordering::Greater
}

fn operation_metadata_can_update_entry(operation: &OperationRecord, entry: &ReplayedEntry) -> bool {
    operation
        .modified_unix_millis
        .cmp(&entry.modified_unix_millis)
        .then_with(|| operation.machine_id.cmp(&entry.source_machine_id))
        != std::cmp::Ordering::Less
}

struct SnapshotConflictResolutionContext<'a> {
    project_id: &'a str,
    online: bool,
    next_sequence: &'a mut u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceEngine {
    pub local_machine_id: String,
    pub platform: Platform,
}

impl ConvergenceEngine {
    pub fn new(local_machine_id: impl Into<String>, platform: Platform) -> Self {
        Self {
            local_machine_id: local_machine_id.into(),
            platform,
        }
    }

    pub fn plan_snapshot(
        &self,
        snapshot: &IndexedSnapshot,
        remote_manifest: Option<&TreeManifest>,
        online: bool,
        start_sequence: u64,
        policy: &Policy,
    ) -> ConvergencePlan {
        self.plan_snapshot_with_remote_state(
            snapshot,
            remote_manifest,
            None,
            online,
            start_sequence,
            policy,
        )
    }

    pub fn plan_snapshot_with_remote_state(
        &self,
        snapshot: &IndexedSnapshot,
        remote_manifest: Option<&TreeManifest>,
        remote_state: Option<&ReplayState>,
        online: bool,
        start_sequence: u64,
        policy: &Policy,
    ) -> ConvergencePlan {
        let remote_entries = remote_manifest
            .map(entries_by_path)
            .unwrap_or_default();
        let local_paths = snapshot
            .entries
            .iter()
            .map(|entry| entry.path().to_owned())
            .collect::<BTreeSet<_>>();
        let mut plan = ConvergencePlan::default();
        let mut next_sequence = start_sequence;
        for entry in &snapshot.entries {
            if is_sync_partial_artifact(entry.path()) {
                push_sync_partial_artifact_ignore(&mut plan, entry.path());
                continue;
            }
            let branch = PolicyBranch::from_entry(entry);
            self.push_policy_action(&mut plan, entry.path(), &branch);
            if branch != PolicyBranch::Sync {
                continue;
            }
            let remote_entry = remote_entries.get(entry.path());
            let remote_tombstone = remote_state.and_then(|state| state.tombstones.get(entry.path()));
            let remote_replayed_entry = remote_state.and_then(|state| state.entries.get(entry.path()));
            if tombstone_obsoletes_snapshot_entry(remote_tombstone, entry, &self.local_machine_id) {
                plan.actions.push(ConvergenceAction::DeleteLocal {
                    path: entry.path().to_owned(),
                });
                continue;
            }
            if let Some(remote_entry) = remote_entry {
                if self.push_snapshot_conflict_resolution(
                    &mut plan,
                    entry,
                    remote_entry,
                    remote_replayed_entry,
                    SnapshotConflictResolutionContext {
                        project_id: &snapshot.project_id,
                        online,
                        next_sequence: &mut next_sequence,
                    },
                ) {
                    continue;
                }
            }
            if should_push_content_entry(entry, remote_entry) {
                let action = ConvergenceAction::PushContent {
                    path: entry.path().to_owned(),
                    source_content_hash: entry.catalog_entry.content_hash.clone(),
                };
                if online {
                    plan.actions.push(action);
                } else {
                    next_sequence += 1;
                    let operation = operation_for_snapshot_entry(
                        &snapshot.project_id,
                        &self.local_machine_id,
                        next_sequence,
                        entry,
                    );
                    plan.queue_offline(operation, action.path().to_owned());
                }
            }
            push_snapshot_metadata_actions(
                &mut plan,
                entry,
                remote_entry,
                remote_replayed_entry,
                &self.local_machine_id,
            );
        }
        for (path, remote_entry) in remote_entries {
            if is_sync_partial_artifact(&path) {
                push_sync_partial_artifact_ignore(&mut plan, &path);
                continue;
            }
            if !local_paths.contains(&path) {
                let branch = self.local_policy_branch_for_remote_only_path(snapshot, &path, policy);
                self.push_policy_action(&mut plan, &path, &branch);
                if branch != PolicyBranch::Sync {
                    continue;
                }
                if tombstone_obsoletes_manifest_entry(remote_state, &remote_entry) {
                    continue;
                }
                let remote_replayed_entry = remote_state.and_then(|state| state.entries.get(&path));
                push_remote_only_metadata_actions(&mut plan, &path, &remote_entry, remote_replayed_entry);
                if should_fetch_content_entry(&remote_entry) {
                    let action = ConvergenceAction::FetchContent {
                        path: path.clone(),
                        store_blob_id: remote_entry.content_hash.clone(),
                        temp_suffix: SYNC_PARTIAL_SUFFIX.to_owned(),
                    };
                    if online {
                        plan.actions.push(action);
                    } else {
                        plan.actions.push(ConvergenceAction::QueueOffline {
                            operation_id: format!("fetch:{path}"),
                            path,
                        });
                    }
                }
            }
        }
        plan.sort_actions();
        plan
    }

    pub fn plan_event(
        &self,
        project_id: &str,
        event: &FsEvent,
        online: bool,
        sequence: u64,
    ) -> ConvergencePlan {
        let mut plan = ConvergencePlan::default();
        if event_touches_sync_partial_artifact(event) {
            plan.actions.push(ConvergenceAction::Noop {
                reason: "internal sync partial artifact ignored".to_owned(),
            });
            plan.sort_actions();
            return plan;
        }
        let branch = event
            .policy_metadata()
            .map(|metadata| PolicyBranch::from_metadata(&event.path, metadata))
            .unwrap_or_else(|| {
                if is_git_metadata_path(&event.path) {
                    PolicyBranch::GitMetadataLocalOnly
                } else {
                    PolicyBranch::Sync
                }
            });
        self.push_policy_action(&mut plan, &event.path, &branch);
        if !event.allows_content_sync() {
            plan.sort_actions();
            return plan;
        }
        if branch != PolicyBranch::Sync {
            self.push_moved_source_cleanup(&mut plan, project_id, event, online, sequence);
            plan.sort_actions();
            return plan;
        }
        if online {
            match event.kind {
                EventKind::Created | EventKind::Edited => {
                    push_created_or_edited_event_actions(&mut plan, event);
                }
                EventKind::Moved => plan.actions.push(ConvergenceAction::MovePath {
                    from: event.previous_path.clone().unwrap_or_default(),
                    to: event.path.clone(),
                }),
                EventKind::Deleted => plan.actions.push(ConvergenceAction::DeleteRemote {
                    path: event.path.clone(),
                }),
                EventKind::PermissionChanged => plan.actions.push(ConvergenceAction::PropagatePermissions {
                    path: event.path.clone(),
                    permissions: event
                        .after
                        .as_ref()
                        .or(event.before.as_ref())
                        .map(|entry| entry.catalog_entry.permissions)
                        .unwrap_or_default(),
                }),
                EventKind::SymlinkChanged => {
                    let source_entry = event.after.as_ref().or(event.before.as_ref());
                    let target = source_entry.and_then(|entry| entry.symlink_target.clone());
                    let target_hash = source_entry.and_then(|entry| entry.catalog_entry.content_hash.clone());
                    push_symlink_action_or_requirement(
                        &mut plan,
                        event.path.clone(),
                        target,
                        target_hash,
                    );
                }
                EventKind::PolicyChanged => plan.actions.push(ConvergenceAction::Noop {
                    reason: "policy marker recorded".to_owned(),
                }),
            }
        } else {
            let operation = OperationRecord::from_watcher_event(
                project_id.to_owned(),
                self.local_machine_id.clone(),
                sequence,
                event,
            );
            plan.queue_offline(operation, event.path.clone());
        }
        plan.sort_actions();
        plan
    }

    fn push_snapshot_conflict_resolution(
        &self,
        plan: &mut ConvergencePlan,
        entry: &SnapshotEntry,
        remote_entry: &TreeEntry,
        remote_replayed_entry: Option<&ReplayedEntry>,
        context: SnapshotConflictResolutionContext<'_>,
    ) -> bool {
        if entry.catalog_entry.kind != TreeEntryKind::File || remote_entry.kind != TreeEntryKind::File {
            return false;
        }
        let Some(local_hash) = &entry.catalog_entry.content_hash else {
            return false;
        };
        let Some(remote_hash) = &remote_entry.content_hash else {
            return false;
        };
        match compare_snapshot_file_content(local_hash, remote_entry, remote_replayed_entry) {
            SnapshotFileContentComparison::Same => return false,
            SnapshotFileContentComparison::NeedsRemoteVerification => {
                push_snapshot_fetch_content_action(plan, remote_entry, context.online);
                return true;
            }
            SnapshotFileContentComparison::Different => {}
        }
        let remote_conflict_hash = remote_replayed_file_source_content_hash(
            remote_entry,
            remote_replayed_entry,
        )
        .unwrap_or(remote_hash.as_str());

        let local_version = FileVersion::from_known_content_hash(
            entry.path(),
            self.local_machine_id.clone(),
            entry.catalog_entry.modified_unix_millis,
            local_hash.clone(),
            Vec::new(),
        );
        let remote_version = FileVersion::from_known_content_hash(
            remote_entry.path.clone(),
            SYNC_REMOTE_MANIFEST_MACHINE_ID,
            remote_entry.modified_unix_millis,
            remote_conflict_hash.to_owned(),
            Vec::new(),
        );
        let Ok(resolution) = resolve_same_file_conflict(local_version, remote_version) else {
            return false;
        };
        if let Some(sidecar) = resolution.sidecar {
            let fetch_loser_payload = sidecar.loser_machine_id == SYNC_REMOTE_MANIFEST_MACHINE_ID
                && sidecar.loser_bytes.is_empty();
            let sidecar_path = sidecar.path.clone();
            let loser_content_hash = if sidecar.loser_machine_id == SYNC_REMOTE_MANIFEST_MACHINE_ID {
                remote_hash.clone()
            } else {
                local_hash.clone()
            };
            plan.actions.push(ConvergenceAction::ConflictSidecar {
                path: sidecar.original_path,
                sidecar_path: sidecar.path,
                winner_machine_id: sidecar.winner_machine_id,
                loser_machine_id: sidecar.loser_machine_id,
                loser_content_hash: loser_content_hash.clone(),
                loser_bytes: sidecar.loser_bytes,
            });
            if fetch_loser_payload && context.online {
                plan.actions.push(ConvergenceAction::FetchContent {
                    path: sidecar_path,
                    store_blob_id: Some(loser_content_hash),
                    temp_suffix: SYNC_PARTIAL_SUFFIX.to_owned(),
                });
            }
        }

        if resolution.winner.machine_id == self.local_machine_id {
            let action = ConvergenceAction::PushContent {
                path: entry.path().to_owned(),
                source_content_hash: entry.catalog_entry.content_hash.clone(),
            };
            if context.online {
                plan.actions.push(action);
            } else {
                *context.next_sequence += 1;
                let operation = operation_for_snapshot_entry(
                    context.project_id,
                    &self.local_machine_id,
                    *context.next_sequence,
                    entry,
                );
                plan.queue_offline(operation, action.path().to_owned());
            }
        } else {
            let action = ConvergenceAction::FetchContent {
                path: remote_entry.path.clone(),
                store_blob_id: remote_entry.content_hash.clone(),
                temp_suffix: SYNC_PARTIAL_SUFFIX.to_owned(),
            };
            if context.online {
                plan.actions.push(action);
            } else {
                plan.actions.push(ConvergenceAction::QueueOffline {
                    operation_id: format!("fetch:{}", remote_entry.path),
                    path: remote_entry.path.clone(),
                });
            }
        }
        true
    }

    fn push_moved_source_cleanup(
        &self,
        plan: &mut ConvergencePlan,
        project_id: &str,
        event: &FsEvent,
        online: bool,
        sequence: u64,
    ) {
        if event.kind != EventKind::Moved {
            return;
        }
        let Some(previous_path) = &event.previous_path else {
            return;
        };
        let source_branch = event
            .before
            .as_ref()
            .map(PolicyBranch::from_entry)
            .unwrap_or_else(|| {
                if is_git_metadata_path(previous_path) {
                    PolicyBranch::GitMetadataLocalOnly
                } else {
                    PolicyBranch::Sync
                }
            });
        if source_branch != PolicyBranch::Sync {
            return;
        }
        if online {
            plan.actions.push(ConvergenceAction::DeleteRemote {
                path: previous_path.clone(),
            });
        } else {
            let modified_unix_millis = event
                .before
                .as_ref()
                .map(|entry| entry.catalog_entry.modified_unix_millis)
                .unwrap_or_default();
            let operation = OperationRecord::from_draft(
                OperationDraft::new(
                    sequence,
                    project_id.to_owned(),
                    self.local_machine_id.clone(),
                    OperationKind::DeletePath,
                    previous_path.clone(),
                )
                .modified_unix_millis(modified_unix_millis),
            );
            plan.queue_offline(operation, previous_path.clone());
        }
    }

    fn local_policy_branch_for_remote_only_path(
        &self,
        snapshot: &IndexedSnapshot,
        path: &str,
        policy: &Policy,
    ) -> PolicyBranch {
        // Non-overridable Git metadata stays local-only regardless of any
        // configured re-inclusion rule (e.g. `!dist/.git/config` cannot sync).
        if is_git_metadata_path(path) {
            return PolicyBranch::GitMetadataLocalOnly;
        }

        // Evaluate the configured leaf policy first. A configured re-inclusion
        // (`!dist/keep.js` under an ignored `dist/`) returns Sync and must
        // fetch even though the local snapshot only has the ignored ancestor.
        let leaf_metadata = PolicyMetadata::from_action(policy.evaluate(path, &self.platform));
        let leaf = PolicyBranch::from_metadata(path, &leaf_metadata);

        // Longest local non-sync ancestor, if any. Used for ancestor
        // suppression when the leaf is suppressed, and to detect when a
        // re-inclusion actually overrides a configured-suppressed ancestor.
        let longest_non_sync_ancestor = snapshot
            .entries
            .iter()
            .filter_map(|entry| {
                let suffix = path.strip_prefix(entry.path())?;
                if !suffix.starts_with('/') {
                    return None;
                }
                let branch = PolicyBranch::from_entry(entry);
                if branch == PolicyBranch::Sync && entry.policy.allows_content_sync() {
                    None
                } else {
                    Some((entry.path().len(), entry.path(), branch))
                }
            })
            .max_by_key(|(length, _, _)| *length);

        if leaf == PolicyBranch::Sync {
            if let Some((_, ancestor_path, ancestor_branch)) = &longest_non_sync_ancestor {
                // Honor the configured re-inclusion only when the configured
                // policy itself suppresses the ancestor (proving a real
                // `!descendant` override). When the ancestor is non-sync only
                // in the snapshot and the configured policy would sync it,
                // ancestor suppression still wins so locally-ignored
                // remote-only paths stay ignored.
                let ancestor_configured_action = policy.evaluate(*ancestor_path, &self.platform);
                if !matches!(ancestor_configured_action, Action::Sync) {
                    return PolicyBranch::Sync;
                }
                return ancestor_branch.clone();
            }
            return PolicyBranch::Sync;
        }

        // Leaf is suppressed: ancestor suppression still applies, falling back
        // to the configured leaf when there is no non-sync local ancestor.
        if let Some((_, _, ancestor_branch)) = longest_non_sync_ancestor {
            return ancestor_branch;
        }
        leaf
    }

    fn push_policy_action(&self, plan: &mut ConvergencePlan, path: &str, branch: &PolicyBranch) {
        match branch {
            PolicyBranch::Sync => {}
            PolicyBranch::Ignore => plan.actions.push(ConvergenceAction::Ignore {
                path: path.to_owned(),
            }),
            PolicyBranch::RebuildLocally => plan.actions.push(ConvergenceAction::RebuildLocally {
                path: path.to_owned(),
            }),
            PolicyBranch::PlatformPin {
                os_family,
                architecture,
            } => {
                if branch.matches_platform(&self.platform) {
                    plan.actions.push(ConvergenceAction::PlatformPinAccepted {
                        path: path.to_owned(),
                    });
                } else {
                    plan.actions.push(ConvergenceAction::PlatformPinRedirected {
                        path: path.to_owned(),
                        required_os: os_family.clone(),
                        required_architecture: architecture.clone(),
                    });
                }
            }
            PolicyBranch::GitMetadataLocalOnly => plan.actions.push(ConvergenceAction::GitMetadataLocalOnly {
                path: path.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConvergencePlan {
    pub actions: Vec<ConvergenceAction>,
    pub queued_operations: Vec<OperationRecord>,
}

impl ConvergencePlan {
    fn queue_offline(&mut self, operation: OperationRecord, path: String) {
        self.actions.push(ConvergenceAction::QueueOffline {
            operation_id: operation.id.clone(),
            path,
        });
        self.queued_operations.push(operation);
    }

    fn sort_actions(&mut self) {
        self.actions.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        self.queued_operations.sort_by(compare_operations);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvergenceAction {
    /// Push local content to the remote store.
    ///
    /// `source_content_hash` is the watcher/catalog fingerprint of the local
    /// file content (e.g. an `fnv64:...` snapshot hash). It is NOT the
    /// file-backed store blob id: the store addresses content blobs by
    /// [`store_blob_id_for_content`], which is computed from the payload bytes
    /// at push time. The push executor must read the bytes at `path`, compute
    /// `store_blob_id_for_content(bytes)`, store the blob under that id via
    /// [`FileBackedSyncStore::put_content_blob`], and record that store blob id
    /// (not the source fingerprint) in the remote manifest's `TreeEntry`.
    PushContent {
        path: String,
        source_content_hash: Option<ContentHash>,
    },
    /// Fetch remote content from the store.
    ///
    /// `store_blob_id` is the canonical file-backed store blob id recorded in
    /// the remote manifest's `TreeEntry.content_hash` (i.e. a
    /// `sync_content_hash` value), not the raw watcher FNV fingerprint. The
    /// fetch executor passes it directly to
    /// [`FileBackedSyncStore::fetch_content_blob`].
    /// Snapshot planning also uses this action as the safe first step for an
    /// existing same-path file whose remote manifest has only a store blob id and
    /// no source-fingerprint sidecar: fetch/stage the remote bytes for
    /// verification before declaring a conflict.
    FetchContent {
        path: String,
        store_blob_id: Option<ContentHash>,
        temp_suffix: String,
    },
    /// Fetch or reconstruct explicit symlink target metadata before materializing a symlink.
    ///
    /// Symlink manifest entries may carry only a target hash in `content_hash`;
    /// executors must not create a local symlink until they have the actual
    /// target string.
    FetchSymlinkTargetMetadata {
        path: String,
        target_hash: Option<ContentHash>,
    },
    DeleteLocal {
        path: String,
    },
    DeleteRemote {
        path: String,
    },
    MovePath {
        from: String,
        to: String,
    },
    PropagatePermissions {
        path: String,
        permissions: u32,
    },
    PropagateSymlink {
        path: String,
        target: String,
    },
    RebuildLocally {
        path: String,
    },
    Ignore {
        path: String,
    },
    PlatformPinAccepted {
        path: String,
    },
    PlatformPinRedirected {
        path: String,
        required_os: String,
        required_architecture: String,
    },
    GitMetadataLocalOnly {
        path: String,
    },
    QueueOffline {
        operation_id: String,
        path: String,
    },
    ConflictSidecar {
        path: String,
        sidecar_path: String,
        winner_machine_id: String,
        loser_machine_id: String,
        loser_content_hash: ContentHash,
        loser_bytes: Vec<u8>,
    },
    Noop {
        reason: String,
    },
}

impl ConvergenceAction {
    fn path(&self) -> &str {
        match self {
            Self::PushContent { path, .. }
            | Self::FetchContent { path, .. }
            | Self::FetchSymlinkTargetMetadata { path, .. }
            | Self::DeleteLocal { path }
            | Self::DeleteRemote { path }
            | Self::PropagatePermissions { path, .. }
            | Self::PropagateSymlink { path, .. }
            | Self::RebuildLocally { path }
            | Self::Ignore { path }
            | Self::PlatformPinAccepted { path }
            | Self::PlatformPinRedirected { path, .. }
            | Self::GitMetadataLocalOnly { path }
            | Self::QueueOffline { path, .. }
            | Self::ConflictSidecar { path, .. } => path,
            Self::MovePath { to, .. } => to,
            Self::Noop { reason } => reason,
        }
    }

    fn sort_key(&self) -> (u8, &str) {
        let rank = match self {
            Self::GitMetadataLocalOnly { .. } => 0,
            Self::Ignore { .. } => 1,
            Self::RebuildLocally { .. } => 2,
            Self::PlatformPinRedirected { .. } => 3,
            Self::PlatformPinAccepted { .. } => 4,
            Self::ConflictSidecar { .. } => 5,
            Self::DeleteRemote { .. } | Self::DeleteLocal { .. } => 6,
            Self::MovePath { .. } => 7,
            Self::FetchContent { .. } | Self::FetchSymlinkTargetMetadata { .. } => 8,
            Self::PushContent { .. } => 9,
            Self::PropagatePermissions { .. } => 10,
            Self::PropagateSymlink { .. } => 11,
            Self::QueueOffline { .. } => 12,
            Self::Noop { .. } => 13,
        };
        (rank, self.path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfflineOperationQueue {
    operations: VecDeque<OperationRecord>,
}

impl OfflineOperationQueue {
    pub fn enqueue(&mut self, operation: OperationRecord) {
        self.operations.push_back(operation);
    }

    pub fn len(&self) -> usize {
        self.operations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn drain_on_reconnect(&mut self, store: &mut AuthoritativeSyncStore) -> Vec<OperationRecord> {
        let mut drained = Vec::new();
        while let Some(operation) = self.operations.pop_front() {
            store.append_operation(operation.clone());
            drained.push(operation);
        }
        drained
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSession {
    pub online: bool,
    pub offline_queue: OfflineOperationQueue,
}

impl SyncSession {
    pub fn online() -> Self {
        Self {
            online: true,
            offline_queue: OfflineOperationQueue::default(),
        }
    }

    pub fn offline() -> Self {
        Self {
            online: false,
            offline_queue: OfflineOperationQueue::default(),
        }
    }

    pub fn submit_operation(
        &mut self,
        operation: OperationRecord,
        store: &mut AuthoritativeSyncStore,
    ) -> SyncSubmitResult {
        if self.online {
            store.append_operation(operation.clone());
            SyncSubmitResult::Applied(operation)
        } else {
            self.offline_queue.enqueue(operation.clone());
            SyncSubmitResult::Queued(operation)
        }
    }

    pub fn reconnect(&mut self, store: &mut AuthoritativeSyncStore) -> Vec<OperationRecord> {
        self.online = true;
        self.offline_queue.drain_on_reconnect(store)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncSubmitResult {
    Applied(OperationRecord),
    Queued(OperationRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    pub path: String,
    pub machine_id: String,
    pub content_hash: ContentHash,
    pub modified_unix_millis: u64,
    pub bytes: Vec<u8>,
}

impl FileVersion {
    pub fn new(
        path: impl Into<String>,
        machine_id: impl Into<String>,
        modified_unix_millis: u64,
        bytes: Vec<u8>,
    ) -> Self {
        let content_hash = sync_content_hash(&bytes);
        Self {
            path: path.into(),
            machine_id: machine_id.into(),
            content_hash,
            modified_unix_millis,
            bytes,
        }
    }
    pub fn from_known_content_hash(
        path: impl Into<String>,
        machine_id: impl Into<String>,
        modified_unix_millis: u64,
        content_hash: impl Into<ContentHash>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            path: path.into(),
            machine_id: machine_id.into(),
            content_hash: content_hash.into(),
            modified_unix_millis,
            bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictSidecar {
    pub path: String,
    pub original_path: String,
    pub loser_machine_id: String,
    pub winner_machine_id: String,
    pub loser_content_hash: ContentHash,
    pub loser_bytes: Vec<u8>,
    pub manual_escape_hatch: ManualConflictResolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualConflictResolution {
    KeepWinner,
    RestoreSidecar,
    KeepBoth,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResolution {
    pub policy: &'static str,
    pub winner: FileVersion,
    pub sidecar: Option<ConflictSidecar>,
}

pub fn resolve_same_file_conflict(left: FileVersion, right: FileVersion) -> SyncResult<ConflictResolution> {
    if left.path != right.path {
        return Err(SyncStoreError::Conflict(
            "conflict resolver requires two versions of the same path".to_owned(),
        ));
    }
    if left.content_hash == right.content_hash {
        return Ok(ConflictResolution {
            policy: U5_CONFLICT_POLICY,
            winner: left,
            sidecar: None,
        });
    }
    let (winner, loser) = if version_wins(&left, &right) {
        (left, right)
    } else {
        (right, left)
    };
    let sidecar = ConflictSidecar {
        path: conflict_sidecar_path(&winner.path, &loser.machine_id, loser.modified_unix_millis),
        original_path: winner.path.clone(),
        loser_machine_id: loser.machine_id.clone(),
        winner_machine_id: winner.machine_id.clone(),
        loser_content_hash: loser.content_hash.clone(),
        loser_bytes: loser.bytes.clone(),
        manual_escape_hatch: ManualConflictResolution::KeepBoth,
    };
    Ok(ConflictResolution {
        policy: U5_CONFLICT_POLICY,
        winner,
        sidecar: Some(sidecar),
    })
}

pub fn conflict_sidecar_path(path: &str, machine_id: &str, modified_unix_millis: u64) -> String {
    format!(
        "{path}{SYNC_CONFLICT_SIDECAR_SUFFIX}.{}.{}",
        safe_path_component(machine_id),
        modified_unix_millis
    )
}

pub fn write_content_atomically(final_path: &Path, bytes: &[u8]) -> SyncResult<()> {
    let parent = final_path.parent().ok_or_else(|| {
        SyncStoreError::invalid_config(format!(
            "final path `{}` has no parent directory",
            final_path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| SyncStoreError::io("create directory", parent, error))?;
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SyncStoreError::invalid_config("final path file name is not valid UTF-8"))?;
    let temp_path = final_path.with_file_name(format!(".{file_name}{SYNC_PARTIAL_SUFFIX}"));
    {
        let mut file = fs::File::create(&temp_path)
            .map_err(|error| SyncStoreError::io("create", &temp_path, error))?;
        file.write_all(bytes)
            .map_err(|error| SyncStoreError::io("write", &temp_path, error))?;
    }
    fs::rename(&temp_path, final_path).map_err(|error| SyncStoreError::io("rename", final_path, error))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBackedBackupReport {
    pub source_root: PathBuf,
    pub backup_root: PathBuf,
    pub copied_files: Vec<PathBuf>,
}

/// CHUNK-05 merge-boundary backup procedure for the file-backed store.
///
/// Copy the append-only sync-store root to a new backup root before merging or
/// applying risky changes. The backup root must not already exist, which keeps
/// rollback evidence immutable and avoids overwriting a known-good snapshot.
pub fn backup_file_backed_sync_store(
    source_root: &Path,
    backup_root: &Path,
) -> SyncResult<FileBackedBackupReport> {
    if backup_root.exists() {
        return Err(SyncStoreError::invalid_config(format!(
            "backup root `{}` already exists",
            backup_root.display()
        )));
    }
    let mut copied_files = Vec::new();
    copy_directory_tree(source_root, backup_root, source_root, &mut copied_files)?;
    copied_files.sort();
    Ok(FileBackedBackupReport {
        source_root: source_root.to_path_buf(),
        backup_root: backup_root.to_path_buf(),
        copied_files,
    })
}

/// Restore a previously captured file-backed sync-store backup.
///
/// The destination root is replaced wholesale so the restored operation log,
/// payload envelopes, and manifests form one consistent known-good state.
pub fn restore_file_backed_sync_store(
    backup_root: &Path,
    destination_root: &Path,
) -> SyncResult<FileBackedBackupReport> {
    if !backup_root.is_dir() {
        return Err(SyncStoreError::Missing(format!(
            "backup root `{}` is not a directory",
            backup_root.display()
        )));
    }
    if let Some(parent) = destination_root.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| SyncStoreError::io("create directory", parent, error))?;
    }

    let temp_root = restore_swap_path(destination_root, "restore-copy");
    let replaced_root = restore_swap_path(destination_root, "restore-replaced");
    remove_path_if_exists(&temp_root)?;
    remove_path_if_exists(&replaced_root)?;

    let mut copied_files = Vec::new();
    if let Err(error) = copy_directory_tree(backup_root, &temp_root, backup_root, &mut copied_files) {
        let _ = remove_path_if_exists(&temp_root);
        return Err(error);
    }
    copied_files.sort();

    if destination_root.exists() {
        if let Err(error) = fs::rename(destination_root, &replaced_root) {
            let _ = remove_path_if_exists(&temp_root);
            return Err(SyncStoreError::io("rename", destination_root, error));
        }
        if let Err(error) = fs::rename(&temp_root, destination_root) {
            let restore_error = SyncStoreError::io("rename", destination_root, error);
            if let Err(rollback_error) = fs::rename(&replaced_root, destination_root) {
                return Err(SyncStoreError::io(
                    "rollback rename",
                    destination_root,
                    rollback_error,
                ));
            }
            return Err(restore_error);
        }
        remove_path_if_exists(&replaced_root)?;
    } else {
        fs::rename(&temp_root, destination_root)
            .map_err(|error| SyncStoreError::io("rename", destination_root, error))?;
    }

    Ok(FileBackedBackupReport {
        source_root: backup_root.to_path_buf(),
        backup_root: destination_root.to_path_buf(),
        copied_files,
    })
}

pub fn sync_content_hash(bytes: &[u8]) -> ContentHash {
    stable_digest_hex(b"sync:content:v1", bytes)
}

/// Canonical file-backed store blob id for content payload bytes.
///
/// This is the address under which [`FileBackedSyncStore::put_content_blob`]
/// stores content and [`FileBackedSyncStore::fetch_content_blob`] reads it,
/// and it is the value the remote manifest records in
/// `TreeEntry.content_hash`. It is distinct from the watcher/catalog source
/// fingerprint carried by [`ConvergenceAction::PushContent`]
/// (`source_content_hash`, typically an `fnv64:...` snapshot hash): the push
/// executor computes this store blob id from the payload bytes and uses it,
/// not the raw watcher fingerprint, to address the store blob and to populate
/// the remote manifest. This keeps the watcher FNV snapshot fingerprint and the
/// store content-addressable blob id in separate domains so a real watcher
/// push through the file-backed store cannot fail the store's
/// `sync_content_hash` integrity check.
pub fn store_blob_id_for_content(bytes: &[u8]) -> ContentHash {
    sync_content_hash(bytes)
}

fn sync_generic_payload_id(payload_type: &str, bytes: &[u8]) -> PayloadId {
    let mut material = Vec::new();
    let kind = PayloadKind::Generic(payload_type.to_owned());
    push_len_prefixed(&mut material, kind.as_wire().as_bytes());
    push_len_prefixed(&mut material, bytes);
    stable_digest_hex(b"sync:generic-payload-id:v1", &material)
}

pub fn sync_initial_migration() -> Migration {
    Migration::new(
        SYNC_MIGRATION_VERSION,
        SYNC_MIGRATION_DESCRIPTION,
        SYNC_MIGRATION_UP_SQL,
        SYNC_MIGRATION_DOWN_SQL,
        SYNC_MIGRATION_TABLES,
    )
}

pub fn sync_migration_runner() -> Result<MigrationRunner, MigrationError> {
    MigrationRunner::with_migrations([sync_initial_migration()])
}

fn operation_for_snapshot_entry(
    project_id: &str,
    machine_id: &str,
    sequence: u64,
    entry: &SnapshotEntry,
) -> OperationRecord {
    let mut draft = OperationDraft::new(
        sequence,
        project_id.to_owned(),
        machine_id.to_owned(),
        OperationKind::PutContent,
        entry.path().to_owned(),
    )
    .modified_unix_millis(entry.catalog_entry.modified_unix_millis)
    .permissions(entry.catalog_entry.permissions)
    .policy_branch(PolicyBranch::from_entry(entry));
    if let Some(content_hash) = &entry.catalog_entry.content_hash {
        draft = draft.content_hash(content_hash.clone());
    }
    if let Some(symlink_target) = &entry.symlink_target {
        draft = draft.symlink_target(symlink_target.clone());
    }
    OperationRecord::from_draft(draft)
}

fn operation_kind_for_created_or_edited_event(source_entry: Option<&SnapshotEntry>) -> OperationKind {
    match source_entry {
        Some(entry)
            if entry.catalog_entry.kind == TreeEntryKind::File
                && entry.catalog_entry.content_hash.is_some() => OperationKind::PutContent,
        Some(entry) if entry.catalog_entry.kind == TreeEntryKind::Symlink => {
            OperationKind::SymlinkChanged
        }
        Some(_) | None => OperationKind::PermissionChanged,
    }
}

fn push_created_or_edited_event_actions(plan: &mut ConvergencePlan, event: &FsEvent) {
    let Some(entry) = event.after.as_ref().or(event.before.as_ref()) else {
        plan.actions.push(ConvergenceAction::Noop {
            reason: "content event missing catalog entry".to_owned(),
        });
        return;
    };

    match entry.catalog_entry.kind {
        TreeEntryKind::File if entry.catalog_entry.content_hash.is_some() => {
            plan.actions.push(ConvergenceAction::PushContent {
                path: event.path.clone(),
                source_content_hash: entry.catalog_entry.content_hash.clone(),
            });
        }
        TreeEntryKind::File | TreeEntryKind::Directory => {
            plan.actions.push(ConvergenceAction::PropagatePermissions {
                path: event.path.clone(),
                permissions: entry.catalog_entry.permissions,
            });
        }
        TreeEntryKind::Symlink => {
            plan.actions.push(ConvergenceAction::PropagatePermissions {
                path: event.path.clone(),
                permissions: entry.catalog_entry.permissions,
            });
            push_symlink_action_or_requirement(
                plan,
                event.path.clone(),
                entry.symlink_target.clone(),
                entry.catalog_entry.content_hash.clone(),
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotFileContentComparison {
    Same,
    Different,
    NeedsRemoteVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentHashDomain {
    SourceFingerprint,
    StoreBlobId,
    Unknown,
}

fn compare_snapshot_file_content(
    local_source_hash: &str,
    remote_entry: &TreeEntry,
    remote_replayed_entry: Option<&ReplayedEntry>,
) -> SnapshotFileContentComparison {
    if let Some(remote_source_hash) =
        remote_replayed_file_source_content_hash(remote_entry, remote_replayed_entry)
    {
        return if remote_source_hash == local_source_hash {
            SnapshotFileContentComparison::Same
        } else {
            SnapshotFileContentComparison::Different
        };
    }

    let Some(remote_manifest_hash) = &remote_entry.content_hash else {
        return SnapshotFileContentComparison::NeedsRemoteVerification;
    };
    if content_hashes_share_domain(local_source_hash, remote_manifest_hash.as_str()) {
        if local_source_hash == remote_manifest_hash.as_str() {
            SnapshotFileContentComparison::Same
        } else {
            SnapshotFileContentComparison::Different
        }
    } else {
        SnapshotFileContentComparison::NeedsRemoteVerification
    }
}

fn remote_replayed_file_source_content_hash<'a>(
    remote_entry: &TreeEntry,
    remote_replayed_entry: Option<&'a ReplayedEntry>,
) -> Option<&'a str> {
    let replayed_entry = remote_replayed_entry?;
    if replayed_entry.symlink_target.is_some() {
        return None;
    }
    if let (Some(replayed_store_blob_id), Some(remote_store_blob_id)) =
        (&replayed_entry.store_blob_id, &remote_entry.content_hash)
    {
        if replayed_store_blob_id != remote_store_blob_id {
            return None;
        }
    }
    replayed_entry.content_hash.as_deref()
}

fn content_hashes_share_domain(left: &str, right: &str) -> bool {
    content_hash_domain(left) == content_hash_domain(right)
}

fn content_hash_domain(hash: &str) -> ContentHashDomain {
    if hash.starts_with("fnv64:") {
        ContentHashDomain::SourceFingerprint
    } else if is_hex_content_digest(hash) {
        ContentHashDomain::StoreBlobId
    } else {
        ContentHashDomain::Unknown
    }
}

fn is_hex_content_digest(hash: &str) -> bool {
    hash.len() == 64 && hash.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
}

fn push_snapshot_fetch_content_action(
    plan: &mut ConvergencePlan,
    remote_entry: &TreeEntry,
    online: bool,
) {
    let action = ConvergenceAction::FetchContent {
        path: remote_entry.path.clone(),
        store_blob_id: remote_entry.content_hash.clone(),
        temp_suffix: SYNC_PARTIAL_SUFFIX.to_owned(),
    };
    if online {
        plan.actions.push(action);
    } else {
        plan.actions.push(ConvergenceAction::QueueOffline {
            operation_id: format!("fetch:{}", remote_entry.path),
            path: remote_entry.path.clone(),
        });
    }
}

fn should_push_content_entry(entry: &SnapshotEntry, remote_entry: Option<&TreeEntry>) -> bool {
    if entry.policy.content_sync != ContentSyncDisposition::Safe
        || entry.catalog_entry.kind != TreeEntryKind::File
        || entry.catalog_entry.content_hash.is_none()
    {
        return false;
    }

    match remote_entry {
        Some(remote_entry) => {
            remote_entry.kind != TreeEntryKind::File || remote_entry.content_hash.is_none()
        }
        None => true,
    }
}

fn should_fetch_content_entry(remote_entry: &TreeEntry) -> bool {
    remote_entry.kind == TreeEntryKind::File && remote_entry.content_hash.is_some()
}

fn push_snapshot_metadata_actions(
    plan: &mut ConvergencePlan,
    entry: &SnapshotEntry,
    remote_entry: Option<&TreeEntry>,
    remote_replayed_entry: Option<&ReplayedEntry>,
    local_machine_id: &str,
) {
    if let Some(permissions) = permissions_to_propagate(entry, remote_entry, local_machine_id) {
        plan.actions.push(ConvergenceAction::PropagatePermissions {
            path: entry.path().to_owned(),
            permissions,
        });
    }

    if should_propagate_snapshot_symlink(entry, remote_entry) {
        let (target, target_hash) = match remote_entry {
            Some(remote_entry)
                if !local_metadata_wins(
                    local_machine_id,
                    entry.catalog_entry.modified_unix_millis,
                    remote_entry.modified_unix_millis,
                ) => (
                    explicit_remote_symlink_target(remote_entry, remote_replayed_entry),
                    remote_entry.content_hash.clone(),
                ),
            _ => (
                entry.symlink_target.clone(),
                entry.catalog_entry.content_hash.clone(),
            ),
        };
        push_symlink_action_or_requirement(plan, entry.path().to_owned(), target, target_hash);
    }
}

fn push_remote_only_metadata_actions(
    plan: &mut ConvergencePlan,
    path: &str,
    remote_entry: &TreeEntry,
    remote_replayed_entry: Option<&ReplayedEntry>,
) {
    plan.actions.push(ConvergenceAction::PropagatePermissions {
        path: path.to_owned(),
        permissions: remote_entry.permissions,
    });
    if remote_entry.kind == TreeEntryKind::Symlink {
        push_symlink_action_or_requirement(
            plan,
            path.to_owned(),
            explicit_remote_symlink_target(remote_entry, remote_replayed_entry),
            remote_entry.content_hash.clone(),
        );
    }
}

fn push_symlink_action_or_requirement(
    plan: &mut ConvergencePlan,
    path: String,
    target: Option<String>,
    target_hash: Option<ContentHash>,
) {
    if let Some(target) = target {
        plan.actions.push(ConvergenceAction::PropagateSymlink { path, target });
    } else {
        plan.actions.push(ConvergenceAction::FetchSymlinkTargetMetadata { path, target_hash });
    }
}

fn explicit_remote_symlink_target(
    remote_entry: &TreeEntry,
    remote_replayed_entry: Option<&ReplayedEntry>,
) -> Option<String> {
    if remote_entry.kind != TreeEntryKind::Symlink {
        return None;
    }
    let replayed_entry = remote_replayed_entry?;
    let target = replayed_entry.symlink_target.as_ref()?;
    match (&remote_entry.content_hash, &replayed_entry.content_hash) {
        (Some(remote_hash), Some(replayed_hash)) if remote_hash == replayed_hash => Some(target.clone()),
        (Some(_), _) => None,
        _ => Some(target.clone()),
    }
}

fn permissions_to_propagate(
    entry: &SnapshotEntry,
    remote_entry: Option<&TreeEntry>,
    local_machine_id: &str,
) -> Option<u32> {
    match remote_entry {
        Some(remote_entry) if remote_entry.permissions != entry.catalog_entry.permissions => {
            if local_metadata_wins(
                local_machine_id,
                entry.catalog_entry.modified_unix_millis,
                remote_entry.modified_unix_millis,
            ) {
                Some(entry.catalog_entry.permissions)
            } else {
                Some(remote_entry.permissions)
            }
        }
        Some(_) => None,
        None => Some(entry.catalog_entry.permissions),
    }
}

fn should_propagate_snapshot_symlink(
    entry: &SnapshotEntry,
    remote_entry: Option<&TreeEntry>,
) -> bool {
    if entry.catalog_entry.kind != TreeEntryKind::Symlink {
        return false;
    }

    match remote_entry {
        Some(remote_entry) => {
            remote_entry.kind != TreeEntryKind::Symlink
                || remote_entry.content_hash != entry.catalog_entry.content_hash
        }
        None => true,
    }
}

fn local_metadata_wins(
    local_machine_id: &str,
    local_modified_unix_millis: u64,
    remote_modified_unix_millis: u64,
) -> bool {
    local_modified_unix_millis
        .cmp(&remote_modified_unix_millis)
        .then_with(|| local_machine_id.cmp(SYNC_REMOTE_MANIFEST_MACHINE_ID))
        == std::cmp::Ordering::Greater
}

fn is_sync_partial_artifact(path: &str) -> bool {
    path.split('/').any(|component| component.ends_with(SYNC_PARTIAL_SUFFIX))
}

fn event_touches_sync_partial_artifact(event: &FsEvent) -> bool {
    is_sync_partial_artifact(&event.path)
        || event
            .previous_path
            .as_deref()
            .map(is_sync_partial_artifact)
            .unwrap_or(false)
}

fn push_sync_partial_artifact_ignore(plan: &mut ConvergencePlan, path: &str) {
    plan.actions.push(ConvergenceAction::Ignore {
        path: path.to_owned(),
    });
}

fn entries_by_path(manifest: &TreeManifest) -> BTreeMap<String, TreeEntry> {
    manifest
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

fn deserialize_manifest(bytes: &[u8]) -> SyncResult<TreeManifest> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        SyncStoreError::decode(format!("manifest payload is not UTF-8: {error}"))
    })?;
    let mut format_seen = false;
    let mut manifest_id = None;
    let mut project_id = None;
    let mut entries = Vec::new();

    for (line_index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        match fields.first().copied() {
            Some("format") if fields.len() == 2 => {
                if format_seen {
                    return Err(SyncStoreError::decode("manifest has duplicate format line"));
                }
                let format = decode_field(fields[1])?;
                if format != CATALOG_MANIFEST_FORMAT_VERSION {
                    return Err(SyncStoreError::decode(format!(
                        "unsupported catalog manifest format `{format}`"
                    )));
                }
                format_seen = true;
            }
            Some("manifest") if fields.len() == 3 => {
                if manifest_id.is_some() {
                    return Err(SyncStoreError::decode("manifest has duplicate header line"));
                }
                manifest_id = Some(decode_field(fields[1])?);
                project_id = Some(decode_field(fields[2])?);
            }
            Some("entry") if fields.len() == 7 => {
                let path = decode_field(fields[1])?;
                let kind = tree_entry_kind_from_wire(&decode_field(fields[2])?)?;
                let size_bytes = parse_u64(&decode_field(fields[3])?, "manifest entry size_bytes")?;
                let modified_unix_millis = parse_u64(
                    &decode_field(fields[4])?,
                    "manifest entry modified_unix_millis",
                )?;
                let permissions = parse_u32(&decode_field(fields[5])?, "manifest entry permissions")?;
                let content_hash = if fields[6] == "-" {
                    None
                } else {
                    Some(decode_field(fields[6])?)
                };
                entries.push(TreeEntry::new(
                    path,
                    kind,
                    size_bytes,
                    modified_unix_millis,
                    permissions,
                    content_hash,
                ));
            }
            Some(tag) => {
                return Err(SyncStoreError::decode(format!(
                    "invalid manifest line {} tagged `{tag}` with {} fields",
                    line_index + 1,
                    fields.len()
                )));
            }
            None => {}
        }
    }

    if !format_seen {
        return Err(SyncStoreError::decode("manifest is missing format line"));
    }
    let manifest_id = manifest_id
        .ok_or_else(|| SyncStoreError::decode("manifest is missing header line"))?;
    let project_id = project_id
        .ok_or_else(|| SyncStoreError::decode("manifest is missing project id"))?;
    Ok(TreeManifest::new(manifest_id, project_id, entries))
}

fn tree_entry_kind_from_wire(value: &str) -> SyncResult<TreeEntryKind> {
    match value {
        "directory" => Ok(TreeEntryKind::Directory),
        "file" => Ok(TreeEntryKind::File),
        "symlink" => Ok(TreeEntryKind::Symlink),
        other => Err(SyncStoreError::decode(format!(
            "unknown manifest entry kind `{other}`"
        ))),
    }
}

fn compare_operations(left: &OperationRecord, right: &OperationRecord) -> std::cmp::Ordering {
    left.sequence
        .cmp(&right.sequence)
        .then_with(|| left.id.cmp(&right.id))
}

fn version_wins(left: &FileVersion, right: &FileVersion) -> bool {
    left.modified_unix_millis
        .cmp(&right.modified_unix_millis)
        .then_with(|| left.machine_id.cmp(&right.machine_id))
        == std::cmp::Ordering::Greater
}

fn write_append_only(path: &Path, bytes: &[u8]) -> SyncResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| SyncStoreError::io("create directory", parent, error))?;
    }
    if path.exists() {
        return accept_existing_append_only_object(path, bytes);
    }
    let temp_path = append_temp_path(path);
    let write_result = (|| -> SyncResult<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|error| SyncStoreError::io("create", &temp_path, error))?;
        file.write_all(bytes)
            .map_err(|error| SyncStoreError::io("write", &temp_path, error))?;
        file.sync_all()
            .map_err(|error| SyncStoreError::io("sync", &temp_path, error))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    match fs::hard_link(&temp_path, path) {
        Ok(()) => {
            fs::remove_file(&temp_path)
                .map_err(|error| SyncStoreError::io("remove", &temp_path, error))?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let result = accept_existing_append_only_object(path, bytes);
            let _ = fs::remove_file(&temp_path);
            result
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(SyncStoreError::io("link", path, error))
        }
    }
}

fn accept_existing_append_only_object(path: &Path, bytes: &[u8]) -> SyncResult<()> {
    let existing = fs::read(path).map_err(|error| SyncStoreError::io("read", path, error))?;
    if existing == bytes {
        Ok(())
    } else {
        Err(SyncStoreError::integrity(format!(
            "append-only object `{}` already exists with different bytes",
            path.display()
        )))
    }
}

fn append_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sync-object");
    let counter = APPEND_TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        counter
    ))
}

fn sorted_files(directory: &Path) -> SyncResult<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| SyncStoreError::io("read directory", directory, error))? {
        let entry = entry.map_err(|error| SyncStoreError::io("read directory entry", directory, error))?;
        let path = entry.path();
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn copy_directory_tree(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    copied_files: &mut Vec<PathBuf>,
) -> SyncResult<()> {
    fs::create_dir_all(destination)
        .map_err(|error| SyncStoreError::io("create directory", destination, error))?;
    for entry in fs::read_dir(source).map_err(|error| SyncStoreError::io("read directory", source, error))? {
        let entry = entry.map_err(|error| SyncStoreError::io("read directory entry", source, error))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory_tree(&source_path, &destination_path, source_root, copied_files)?;
        } else if source_path.is_file() {
            fs::copy(&source_path, &destination_path)
                .map_err(|error| SyncStoreError::io("copy", &source_path, error))?;
            let relative = source_path.strip_prefix(source_root).unwrap_or(&source_path);
            copied_files.push(relative.to_path_buf());
        }
    }
    Ok(())
}

fn restore_swap_path(destination_root: &Path, suffix: &str) -> PathBuf {
    let file_name = destination_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sync-store");
    destination_root.with_file_name(format!(
        ".{file_name}.{suffix}.{}.tmp",
        std::process::id()
    ))
}

fn remove_path_if_exists(path: &Path) -> SyncResult<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|error| SyncStoreError::io("remove directory", path, error))?;
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| SyncStoreError::io("remove file", path, error))?;
    }
    Ok(())
}

fn is_git_metadata_path(path: &str) -> bool {
    let normalized = path.trim_matches('/').replace('\\', "/");
    normalized == ".git"
        || normalized == ".gitmodules"
        || normalized.starts_with(".git/")
        || normalized.ends_with("/.git")
        || normalized.contains("/.git/")
        || normalized.ends_with("/.gitmodules")
}

fn push_kv(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push('\t');
    output.push_str(&encode_field(value));
    output.push('\n');
}

fn push_optional_kv(output: &mut String, key: &str, value: Option<&str>) {
    output.push_str(key);
    output.push('\t');
    output.push_str(&encode_optional_field(value));
    output.push('\n');
}

fn parse_kv_lines(text: &str) -> SyncResult<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('\t').ok_or_else(|| {
            SyncStoreError::decode(format!("line {} is missing tab separator", index + 1))
        })?;
        if fields
            .insert(key.to_owned(), decode_field(value)?)
            .is_some()
        {
            return Err(SyncStoreError::decode(format!(
                "duplicate field `{key}`"
            )));
        }
    }
    Ok(fields)
}

fn required_field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> SyncResult<&'a str> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| SyncStoreError::decode(format!("missing field `{key}`")))
}

fn optional_field(fields: &BTreeMap<String, String>, key: &str) -> SyncResult<Option<String>> {
    let Some(value) = fields.get(key) else {
        return Ok(None);
    };
    decode_optional_field(value)
}

fn encode_optional_field(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("+{}", encode_field(value)),
        None => "~".to_owned(),
    }
}

fn decode_optional_field(value: &str) -> SyncResult<Option<String>> {
    if value == "~" {
        return Ok(None);
    }
    let Some(encoded) = value.strip_prefix('+') else {
        return Err(SyncStoreError::decode(
            "optional field must be `~` or `+<encoded>`",
        ));
    };
    Ok(Some(encoded.to_owned()))
}

fn encode_field(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b':' | b'/' => {
                output.push(*byte as char);
            }
            _ => {
                write!(&mut output, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    output
}

fn decode_field(value: &str) -> SyncResult<String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut output = Vec::new();
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(SyncStoreError::decode("truncated percent escape"));
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output)
        .map_err(|error| SyncStoreError::decode(format!("field is not UTF-8: {error}")))
}

fn safe_path_component(value: &str) -> String {
    let encoded = encode_field(value).replace('/', "%2F").replace(':', "%3A");
    match encoded.as_str() {
        "" => "_".to_owned(),
        "." => "%2E".to_owned(),
        ".." => "%2E%2E".to_owned(),
        _ => encoded,
    }
}

fn bool_wire(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

fn parse_bool(value: &str, name: &str) -> SyncResult<bool> {
    match value {
        "1" => Ok(true),
        "0" => Ok(false),
        other => Err(SyncStoreError::decode(format!(
            "field `{name}` must be 0 or 1, got `{other}`"
        ))),
    }
}

fn parse_u64(value: &str, name: &str) -> SyncResult<u64> {
    value.parse::<u64>().map_err(|error| {
        SyncStoreError::decode(format!("field `{name}` is not a u64 `{value}`: {error}"))
    })
}

fn parse_u32(value: &str, name: &str) -> SyncResult<u32> {
    value.parse::<u32>().map_err(|error| {
        SyncStoreError::decode(format!("field `{name}` is not a u32 `{value}`: {error}"))
    })
}

fn derive_nonce(
    kind: &PayloadKind,
    sender_machine_id: &str,
    recipient_machine_id: Option<&str>,
    plaintext: &[u8],
    secret: &SharedSecret,
) -> [u8; 16] {
    let mut material = Vec::new();
    push_domain(&mut material, b"sync:envelope-nonce:v1");
    push_len_prefixed(&mut material, kind.as_wire().as_bytes());
    push_len_prefixed(&mut material, sender_machine_id.as_bytes());
    push_len_prefixed(&mut material, recipient_machine_id.unwrap_or_default().as_bytes());
    push_len_prefixed(&mut material, plaintext);
    let digest = hmac_sha256(secret.key(), &material);
    let mut nonce = [0_u8; 16];
    nonce.copy_from_slice(&digest[..16]);
    nonce
}

fn envelope_mac(
    secret: &SharedSecret,
    version: &str,
    kind: &PayloadKind,
    sender_machine_id: &str,
    recipient_machine_id: Option<&str>,
    nonce: &[u8; 16],
    ciphertext: &[u8],
) -> [u8; 32] {
    let mut material = Vec::new();
    push_domain(&mut material, b"sync:envelope-mac:v1");
    push_len_prefixed(&mut material, version.as_bytes());
    push_len_prefixed(&mut material, kind.as_wire().as_bytes());
    push_len_prefixed(&mut material, sender_machine_id.as_bytes());
    push_len_prefixed(&mut material, recipient_machine_id.unwrap_or_default().as_bytes());
    push_len_prefixed(&mut material, nonce);
    push_len_prefixed(&mut material, ciphertext);
    hmac_sha256(secret.key(), &material)
}

fn xor_stream_in_place(bytes: &mut [u8], secret: &SharedSecret, nonce: &[u8; 16]) {
    let mut offset = 0;
    let mut counter = 0_u64;
    while offset < bytes.len() {
        let mut material = Vec::new();
        push_domain(&mut material, b"sync:xor-stream:v1");
        push_len_prefixed(&mut material, nonce);
        material.extend_from_slice(&counter.to_be_bytes());
        let block = hmac_sha256(secret.key(), &material);
        for byte in block {
            if offset == bytes.len() {
                break;
            }
            bytes[offset] ^= byte;
            offset += 1;
        }
        counter += 1;
    }
}

fn stable_digest_hex(domain: &[u8], bytes: &[u8]) -> String {
    let mut material = Vec::new();
    push_domain(&mut material, domain);
    push_len_prefixed(&mut material, bytes);
    hex_bytes(&sha256(&material))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut key_block = [0_u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&sha256(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut outer = [0_u8; 64];
    let mut inner = [0_u8; 64];
    for index in 0..64 {
        outer[index] = key_block[index] ^ 0x5c;
        inner[index] = key_block[index] ^ 0x36;
    }
    let mut inner_material = Vec::with_capacity(64 + data.len());
    inner_material.extend_from_slice(&inner);
    inner_material.extend_from_slice(data);
    let inner_hash = sha256(&inner_material);
    let mut outer_material = Vec::with_capacity(64 + inner_hash.len());
    outer_material.extend_from_slice(&outer);
    outer_material.extend_from_slice(&inner_hash);
    sha256(&outer_material)
}

fn push_domain(output: &mut Vec<u8>, domain: &[u8]) {
    push_len_prefixed(output, domain);
}

fn push_len_prefixed(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn decode_hex(value: &str) -> SyncResult<Vec<u8>> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(SyncStoreError::decode("hex field has odd length"));
    }
    let mut output = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_value(bytes[index])?;
        let low = hex_value(bytes[index + 1])?;
        output.push((high << 4) | low);
        index += 2;
    }
    Ok(output)
}

fn hex_value(byte: u8) -> SyncResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(SyncStoreError::decode(format!(
            "invalid hex byte `{}`",
            other as char
        ))),
    }
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (input.len() as u64) * 8;
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0_u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut output = [0_u8; 32];
    for (index, word) in h.iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::{Architecture, MachineIdProvenance, OsFamily, PlatformCapabilities};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn same_file_conflict_writes_sidecar_for_loser() {
        let local = FileVersion::new("src/lib.rs", "machine-a", 10, b"older".to_vec());
        let remote = FileVersion::new("src/lib.rs", "machine-b", 20, b"newer".to_vec());

        let resolution = resolve_same_file_conflict(local, remote).expect("conflict resolves");

        assert_eq!(resolution.policy, U5_CONFLICT_POLICY);
        assert_eq!(resolution.winner.machine_id, "machine-b");
        let sidecar = resolution.sidecar.expect("loser retained");
        assert_eq!(sidecar.original_path, "src/lib.rs");
        assert!(sidecar.path.contains(SYNC_CONFLICT_SIDECAR_SUFFIX));
        assert_eq!(sidecar.loser_machine_id, "machine-a");
        assert_eq!(sidecar.loser_bytes, b"older".to_vec());
        assert_eq!(sidecar.manual_escape_hatch, ManualConflictResolution::KeepBoth);
    }

    #[test]
    fn offline_reconnect_converges_queued_operations() {
        let mut store = AuthoritativeSyncStore::new("project");
        let mut session = SyncSession::offline();
        let operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::PutContent, "file.txt")
                .content_hash("hash-a")
                .modified_unix_millis(42)
                .permissions(0o644),
        );

        let submit = session.submit_operation(operation.clone(), &mut store);
        assert!(matches!(submit, SyncSubmitResult::Queued(_)));
        assert_eq!(store.operation_log().len(), 0);
        assert_eq!(session.offline_queue.len(), 1);

        let drained = session.reconnect(&mut store);

        assert_eq!(drained, vec![operation.clone()]);
        assert_eq!(store.operation_log(), &[operation]);
        assert!(session.offline_queue.is_empty());
    }

    #[test]
    fn operation_log_replay_is_deterministic() {
        let put_a = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::PutContent, "a.txt")
                .content_hash("hash-a1")
                .modified_unix_millis(10)
                .permissions(0o644),
        );
        let move_a = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::MovePath, "b.txt")
                .previous_path("a.txt")
                .content_hash("hash-a1")
                .modified_unix_millis(11)
                .permissions(0o644),
        );
        let delete_b = OperationRecord::from_draft(
            OperationDraft::new(3, "project", "machine-b", OperationKind::DeletePath, "b.txt")
                .modified_unix_millis(12),
        );
        let ordered = vec![put_a.clone(), move_a.clone(), delete_b.clone()];
        let shuffled = vec![delete_b, put_a, move_a];

        let ordered_state = replay_operation_log(&ordered);
        let shuffled_state = replay_operation_log(&shuffled);

        assert_eq!(ordered_state, shuffled_state);
        assert!(ordered_state.entries.is_empty());
    }

    #[test]
    fn replay_divergent_put_content_keeps_lww_and_sidecar() {
        let winner = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::PutContent, "file.txt")
                .content_hash("hash-new")
                .modified_unix_millis(100)
                .permissions(0o644),
        );
        let loser = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::PutContent, "file.txt")
                .content_hash("hash-old")
                .modified_unix_millis(10)
                .permissions(0o644),
        );

        let state = replay_operation_log(&[loser, winner]);

        let entry = state.entries.get("file.txt").expect("winner retained");
        assert_eq!(entry.content_hash.as_deref(), Some("hash-new"));
        assert_eq!(entry.source_machine_id, "machine-b");
        assert_eq!(
            state.conflict_sidecars,
            vec![conflict_sidecar_path("file.txt", "machine-a", 10)]
        );
    }

    #[test]
    fn replay_tombstone_prevents_old_put_resurrection() {
        let delete = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::DeletePath, "file.txt")
                .modified_unix_millis(50),
        );
        let old_put = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::PutContent, "file.txt")
                .content_hash("hash-old")
                .modified_unix_millis(10)
                .permissions(0o644),
        );

        let state = replay_operation_log(&[old_put, delete]);

        assert!(state.entries.is_empty());
        assert_eq!(
            state.tombstones.get("file.txt"),
            Some(&DeleteTombstone {
                path: "file.txt".to_owned(),
                modified_unix_millis: 50,
                source_machine_id: "machine-b".to_owned(),
            })
        );
    }


    #[test]
    fn replay_old_delete_does_not_remove_newer_put() {
        let newer_put = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::PutContent, "file.txt")
                .content_hash("hash-new")
                .modified_unix_millis(100)
                .permissions(0o644),
        );
        let old_delete = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::DeletePath, "file.txt")
                .modified_unix_millis(10),
        );

        let state = replay_operation_log(&[newer_put, old_delete]);

        let entry = state.entries.get("file.txt").expect("newer put retained");
        assert_eq!(entry.content_hash.as_deref(), Some("hash-new"));
        assert_eq!(entry.source_machine_id, "machine-b");
        assert!(!state.tombstones.contains_key("file.txt"));
    }

    #[test]
    fn replay_move_materializes_destination_when_source_is_absent() {
        let move_record = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::MovePath, "new.txt")
                .previous_path("old.txt")
                .content_hash("hash-moved")
                .modified_unix_millis(20)
                .permissions(0o600),
        );

        let state = replay_operation_log(&[move_record]);

        let entry = state.entries.get("new.txt").expect("move destination materialized");
        assert_eq!(entry.content_hash.as_deref(), Some("hash-moved"));
        assert_eq!(entry.permissions, Some(0o600));
        assert_eq!(
            state.tombstones.get("old.txt"),
            Some(&DeleteTombstone {
                path: "old.txt".to_owned(),
                modified_unix_millis: 20,
                source_machine_id: "machine-a".to_owned(),
            })
        );
    }

    #[test]
    fn replay_metadata_only_creates_materialize_directories_and_symlinks() {
        let directory = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::PermissionChanged, "assets")
                .modified_unix_millis(10)
                .permissions(0o755),
        );
        let symlink = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::SymlinkChanged, "current")
                .content_hash("target-hash")
                .modified_unix_millis(11)
                .permissions(0o777)
                .symlink_target("releases/v1"),
        );

        let state = replay_operation_log(&[symlink, directory]);

        let directory_entry = state.entries.get("assets").expect("directory materialized");
        assert!(directory_entry.content_hash.is_none());
        assert_eq!(directory_entry.permissions, Some(0o755));
        assert!(directory_entry.symlink_target.is_none());
        assert_eq!(directory_entry.source_machine_id, "machine-a");

        let symlink_entry = state.entries.get("current").expect("symlink materialized");
        assert_eq!(symlink_entry.content_hash.as_deref(), Some("target-hash"));
        assert_eq!(symlink_entry.permissions, Some(0o777));
        assert_eq!(symlink_entry.symlink_target.as_deref(), Some("releases/v1"));
        assert_eq!(symlink_entry.source_machine_id, "machine-a");
    }

    #[test]
    fn replay_old_permission_change_does_not_downgrade_newer_entry() {
        let newer_put = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::PutContent, "file.txt")
                .content_hash("hash-new")
                .modified_unix_millis(100)
                .permissions(0o600),
        );
        let old_permission = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::PermissionChanged, "file.txt")
                .modified_unix_millis(10)
                .permissions(0o777),
        );

        let state = replay_operation_log(&[newer_put, old_permission]);

        let entry = state.entries.get("file.txt").expect("newer entry retained");
        assert_eq!(entry.content_hash.as_deref(), Some("hash-new"));
        assert_eq!(entry.permissions, Some(0o600));
        assert_eq!(entry.modified_unix_millis, 100);
        assert_eq!(entry.source_machine_id, "machine-b");
    }

    #[test]
    fn replay_newer_permission_change_updates_existing_entry() {
        let old_put = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::PutContent, "file.txt")
                .content_hash("hash-old")
                .modified_unix_millis(10)
                .permissions(0o644),
        );
        let newer_permission = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-b", OperationKind::PermissionChanged, "file.txt")
                .modified_unix_millis(100)
                .permissions(0o755),
        );

        let state = replay_operation_log(&[old_put, newer_permission]);

        let entry = state.entries.get("file.txt").expect("permission change applied");
        assert_eq!(entry.content_hash.as_deref(), Some("hash-old"));
        assert_eq!(entry.permissions, Some(0o755));
        assert_eq!(entry.modified_unix_millis, 100);
        assert_eq!(entry.source_machine_id, "machine-b");
    }

    #[test]
    fn replay_old_symlink_change_does_not_downgrade_newer_entry() {
        let newer_symlink = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::SymlinkChanged, "current")
                .content_hash("target-new")
                .modified_unix_millis(100)
                .permissions(0o777)
                .symlink_target("releases/v2"),
        );
        let old_symlink = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::SymlinkChanged, "current")
                .content_hash("target-old")
                .modified_unix_millis(10)
                .permissions(0o700)
                .symlink_target("releases/v1"),
        );

        let state = replay_operation_log(&[newer_symlink, old_symlink]);

        let entry = state.entries.get("current").expect("newer symlink retained");
        assert_eq!(entry.content_hash.as_deref(), Some("target-new"));
        assert_eq!(entry.permissions, Some(0o777));
        assert_eq!(entry.symlink_target.as_deref(), Some("releases/v2"));
        assert_eq!(entry.modified_unix_millis, 100);
        assert_eq!(entry.source_machine_id, "machine-b");
    }

    #[test]
    fn replay_newer_symlink_change_updates_existing_entry() {
        let old_symlink = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::SymlinkChanged, "current")
                .content_hash("target-old")
                .modified_unix_millis(10)
                .permissions(0o700)
                .symlink_target("releases/v1"),
        );
        let newer_symlink = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-b", OperationKind::SymlinkChanged, "current")
                .content_hash("target-new")
                .modified_unix_millis(100)
                .permissions(0o777)
                .symlink_target("releases/v2"),
        );

        let state = replay_operation_log(&[old_symlink, newer_symlink]);

        let entry = state.entries.get("current").expect("newer symlink applied");
        assert_eq!(entry.content_hash.as_deref(), Some("target-new"));
        assert_eq!(entry.permissions, Some(0o777));
        assert_eq!(entry.symlink_target.as_deref(), Some("releases/v2"));
        assert_eq!(entry.modified_unix_millis, 100);
        assert_eq!(entry.source_machine_id, "machine-b");
    }

    #[test]
    fn replay_metadata_changes_use_machine_id_tie_breaker() {
        let permission_entry = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::PutContent, "file.txt")
                .content_hash("hash-current")
                .modified_unix_millis(50)
                .permissions(0o600),
        );
        let losing_permission = OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-a", OperationKind::PermissionChanged, "file.txt")
                .modified_unix_millis(50)
                .permissions(0o777),
        );
        let symlink_entry = OperationRecord::from_draft(
            OperationDraft::new(3, "project", "machine-a", OperationKind::SymlinkChanged, "current")
                .content_hash("target-old")
                .modified_unix_millis(50)
                .permissions(0o700)
                .symlink_target("releases/v1"),
        );
        let winning_symlink = OperationRecord::from_draft(
            OperationDraft::new(4, "project", "machine-b", OperationKind::SymlinkChanged, "current")
                .content_hash("target-new")
                .modified_unix_millis(50)
                .permissions(0o777)
                .symlink_target("releases/v2"),
        );

        let state = replay_operation_log(&[
            permission_entry,
            losing_permission,
            symlink_entry,
            winning_symlink,
        ]);

        let file_entry = state.entries.get("file.txt").expect("higher machine id retained");
        assert_eq!(file_entry.permissions, Some(0o600));
        assert_eq!(file_entry.modified_unix_millis, 50);
        assert_eq!(file_entry.source_machine_id, "machine-b");

        let symlink = state.entries.get("current").expect("higher machine id applied");
        assert_eq!(symlink.content_hash.as_deref(), Some("target-new"));
        assert_eq!(symlink.permissions, Some(0o777));
        assert_eq!(symlink.symlink_target.as_deref(), Some("releases/v2"));
        assert_eq!(symlink.modified_unix_millis, 50);
        assert_eq!(symlink.source_machine_id, "machine-b");
    }

    #[test]
    fn local_harness_exchanges_over_file_backed_production_contract() {
        let root = temp_path("file-backed-exchange");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let machine_b = app_scoped_machine_id("machine-b").expect("machine id");
        let authorized = vec![machine_a.clone(), machine_b.clone()];
        let store_a = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                authorized.clone(),
                secret.clone(),
            ),
            "project",
            machine_a.clone(),
        )
        .expect("store a");
        let store_b = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(TransportMode::LocalHarness, root.clone(), authorized, secret),
            "project",
            machine_b,
        )
        .expect("store b");
        let content = b"hello from machine a";
        let content_hash = sync_content_hash(content);
        let operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", machine_a, OperationKind::PutContent, "hello.txt")
                .content_hash(content_hash.clone())
                .modified_unix_millis(7)
                .permissions(0o644),
        );
        let manifest = TreeManifest::new(
            "manifest-a",
            "project",
            vec![TreeEntry::file(
                "hello.txt",
                content.len() as u64,
                7,
                0o644,
                Some(content_hash.clone()),
            )],
        );

        store_a
            .put_content_blob(&content_hash, content)
            .expect("content stored");
        store_b
            .put_content_blob(&content_hash, content)
            .expect("idempotent duplicate content stored");
        store_a.put_manifest(&manifest).expect("manifest stored");
        store_a.append_operation(&operation).expect("op stored");

        assert_eq!(
            store_b.fetch_content_blob(&content_hash).expect("content fetched"),
            content
        );
        assert_eq!(
            store_b.fetch_manifest("manifest-a").expect("manifest fetched"),
            manifest
        );
        assert_eq!(store_b.load_operation_log().expect("oplog"), vec![operation]);
        remove_temp(&root);
    }

    #[test]
    fn encrypted_envelope_contains_no_plaintext_payload() {
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let plaintext = b"TOP_SECRET_PAYLOAD";
        let envelope = PayloadEnvelope::seal(
            PayloadKind::Content,
            app_scoped_machine_id("machine-a").expect("machine id"),
            None,
            plaintext,
            &secret,
        )
        .expect("sealed");
        let wire = envelope.to_wire_bytes();

        assert!(!contains_subslice(&wire, plaintext));
        assert_eq!(envelope.open(&secret).expect("opened"), plaintext);
    }

    #[test]
    fn file_backed_payload_reads_reject_unauthorized_senders_and_wrong_recipients() {
        let root = temp_path("secure-payload-reads");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let machine_b = app_scoped_machine_id("machine-b").expect("machine id");
        let unauthorized = app_scoped_machine_id("machine-c").expect("machine id");
        let store_b = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                vec![machine_a.clone(), machine_b.clone()],
                secret.clone(),
            ),
            "project",
            machine_b.clone(),
        )
        .expect("store b");

        let content = b"content from unauthorized machine";
        let content_hash = sync_content_hash(content);
        let unauthorized_content = PayloadEnvelope::seal(
            PayloadKind::Content,
            unauthorized,
            None,
            content,
            &secret,
        )
        .expect("content envelope");
        fs::write(
            store_b.payload_path(&content_hash, "blob"),
            unauthorized_content.to_wire_bytes(),
        )
        .expect("write unauthorized content");
        assert!(matches!(
            store_b.fetch_content_blob(&content_hash),
            Err(SyncStoreError::AuthenticationFailed(message)) if message.contains("not authorized")
        ));

        let manifest = TreeManifest::new("manifest-wrong-recipient", "project", Vec::new());
        let manifest_bytes = manifest.serialize_deterministic();
        let wrong_recipient_manifest = PayloadEnvelope::seal(
            PayloadKind::Manifest,
            machine_a.clone(),
            Some(machine_a.clone()),
            manifest_bytes.as_bytes(),
            &secret,
        )
        .expect("manifest envelope");
        fs::write(
            store_b.payload_path("manifest-wrong-recipient", "manifest"),
            wrong_recipient_manifest.to_wire_bytes(),
        )
        .expect("write wrong recipient manifest");
        assert!(matches!(
            store_b.fetch_manifest("manifest-wrong-recipient"),
            Err(SyncStoreError::AuthenticationFailed(message)) if message.contains("recipient")
        ));

        let generic_bytes = b"generic payload";
        let generic_id = sync_generic_payload_id("settings", generic_bytes);
        let wrong_recipient_generic = PayloadEnvelope::seal(
            PayloadKind::generic("settings").expect("generic kind"),
            machine_a.clone(),
            Some(machine_a),
            generic_bytes,
            &secret,
        )
        .expect("generic envelope");
        fs::write(
            store_b.payload_path(&generic_id, "payload"),
            wrong_recipient_generic.to_wire_bytes(),
        )
        .expect("write wrong recipient generic");
        assert!(matches!(
            store_b.fetch_generic_payload("settings", &generic_id),
            Err(SyncStoreError::AuthenticationFailed(message)) if message.contains("recipient")
        ));

        remove_temp(&root);
    }

    #[test]
    fn file_backed_payload_reads_reject_copied_envelopes_under_wrong_ids() {
        let root = temp_path("payload-id-binding");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let machine_b = app_scoped_machine_id("machine-b").expect("machine id");
        let authorized = vec![machine_a.clone(), machine_b.clone()];
        let store_a = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                authorized.clone(),
                secret.clone(),
            ),
            "project",
            machine_a.clone(),
        )
        .expect("store a");
        let store_b = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(TransportMode::LocalHarness, root.clone(), authorized, secret.clone()),
            "project",
            machine_b,
        )
        .expect("store b");

        let content = b"content bound to its hash";
        let content_hash = sync_content_hash(content);
        store_a
            .put_content_blob(&content_hash, content)
            .expect("content stored");
        let wrong_content_hash = sync_content_hash(b"different content");
        fs::copy(
            store_a.payload_path(&content_hash, "blob"),
            store_b.payload_path(&wrong_content_hash, "blob"),
        )
        .expect("copy content envelope under wrong id");
        assert!(matches!(
            store_b.fetch_content_blob(&wrong_content_hash),
            Err(SyncStoreError::Integrity(message)) if message.contains("content payload id")
        ));

        let generic_bytes = b"generic payload bound to type and bytes";
        let generic_id = store_a
            .put_generic_payload("settings", generic_bytes)
            .expect("generic stored");
        let other_type_id = store_a
            .put_generic_payload("profile", generic_bytes)
            .expect("other generic stored");
        assert_ne!(generic_id, other_type_id);
        let wrong_generic_id = sync_generic_payload_id("settings", b"different generic payload");
        fs::copy(
            store_a.payload_path(&generic_id, "payload"),
            store_b.payload_path(&wrong_generic_id, "payload"),
        )
        .expect("copy generic envelope under wrong id");
        assert!(matches!(
            store_b.fetch_generic_payload("settings", &wrong_generic_id),
            Err(SyncStoreError::Integrity(message)) if message.contains("generic payload id")
        ));

        let manifest = TreeManifest::new("manifest-a", "project", Vec::new());
        store_a.put_manifest(&manifest).expect("manifest stored");
        fs::copy(
            store_a.payload_path("manifest-a", "manifest"),
            store_b.payload_path("manifest-copy", "manifest"),
        )
        .expect("copy manifest envelope under wrong id");
        assert!(matches!(
            store_b.fetch_manifest("manifest-copy"),
            Err(SyncStoreError::Integrity(message)) if message.contains("contained manifest")
        ));

        let other_project_manifest = TreeManifest::new("manifest-other-project", "other-project", Vec::new());
        let other_project_bytes = other_project_manifest.serialize_deterministic();
        let other_project_envelope = PayloadEnvelope::seal(
            PayloadKind::Manifest,
            machine_a,
            None,
            other_project_bytes.as_bytes(),
            &secret,
        )
        .expect("other project manifest envelope");
        fs::write(
            store_b.payload_path("manifest-other-project", "manifest"),
            other_project_envelope.to_wire_bytes(),
        )
        .expect("write wrong project manifest");
        assert!(matches!(
            store_b.fetch_manifest("manifest-other-project"),
            Err(SyncStoreError::Integrity(message)) if message.contains("belongs to project")
        ));

        remove_temp(&root);
    }

    #[test]
    fn operation_log_rejects_unauthorized_senders_and_non_operation_envelopes() {
        let root = temp_path("secure-oplog-reads");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let machine_b = app_scoped_machine_id("machine-b").expect("machine id");
        let unauthorized = app_scoped_machine_id("machine-c").expect("machine id");
        let store_b = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                vec![machine_a.clone(), machine_b.clone()],
                secret.clone(),
            ),
            "project",
            machine_b,
        )
        .expect("store b");
        let unauthorized_operation = OperationRecord::from_draft(
            OperationDraft::new(
                1,
                "project",
                unauthorized.clone(),
                OperationKind::PutContent,
                "file.txt",
            )
            .content_hash("hash-a")
            .modified_unix_millis(1),
        );
        let unauthorized_envelope = PayloadEnvelope::seal(
            PayloadKind::Operation,
            unauthorized,
            None,
            unauthorized_operation.serialize_deterministic().as_bytes(),
            &secret,
        )
        .expect("operation envelope");
        let unauthorized_path = store_b.operation_path(&unauthorized_operation.id);
        fs::write(&unauthorized_path, unauthorized_envelope.to_wire_bytes())
            .expect("write unauthorized operation");
        assert!(matches!(
            store_b.load_operation_log(),
            Err(SyncStoreError::AuthenticationFailed(message)) if message.contains("not authorized")
        ));
        fs::remove_file(&unauthorized_path).expect("remove unauthorized operation");

        let wrong_kind = PayloadEnvelope::seal(
            PayloadKind::Content,
            machine_a,
            None,
            b"not an operation record",
            &secret,
        )
        .expect("wrong kind envelope");
        fs::write(store_b.operation_path("not-operation"), wrong_kind.to_wire_bytes())
            .expect("write wrong kind operation");
        assert!(matches!(
            store_b.load_operation_log(),
            Err(SyncStoreError::Decode(message)) if message.contains("kind mismatch")
        ));

        remove_temp(&root);
    }

    #[test]
    fn operation_log_ignores_temp_files_and_rejects_filename_id_mismatches() {
        let root = temp_path("oplog-canonical-files");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let store = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                vec![machine_a.clone()],
                secret,
            ),
            "project",
            machine_a.clone(),
        )
        .expect("store");
        let operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", machine_a, OperationKind::PutContent, "file.txt")
                .content_hash("hash-a")
                .modified_unix_millis(1)
                .permissions(0o644),
        );
        store.append_operation(&operation).expect("append operation");
        fs::write(
            store.operation_dir().join(".orphan.syncop.123.0.tmp"),
            b"not an operation envelope",
        )
        .expect("write stale temp operation");

        assert_eq!(store.load_operation_log().expect("oplog"), vec![operation.clone()]);

        fs::copy(
            store.operation_path(&operation.id),
            store.operation_path("different-operation-id"),
        )
        .expect("copy operation under wrong filename");
        assert!(matches!(
            store.load_operation_log(),
            Err(SyncStoreError::Integrity(message))
                if message.contains("filename stem") && message.contains("contained operation id")
        ));

        remove_temp(&root);
    }

    #[test]
    fn convergence_branches_on_all_policy_actions_and_git_metadata() {
        let platform = linux_platform("machine-a");
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            platform.clone(),
        );
        let mismatch_pin = PlatformPin {
            os_family: OsFamily::Macos,
            architecture: Architecture::Aarch64,
        };
        let entries = vec![
            snapshot_entry("src/main.rs", Action::Sync, Some("hash-main")),
            snapshot_entry("ignored.log", Action::Ignore, Some("hash-ignore")),
            snapshot_entry("node_modules/pkg/index.js", Action::RebuildLocally, Some("hash-node")),
            snapshot_entry(
                "native/addon.node",
                Action::PlatformPin(mismatch_pin),
                Some("hash-native"),
            ),
            snapshot_entry(".git/config", Action::Sync, Some("hash-git")),
            snapshot_entry("submodule/.git", Action::Sync, Some("hash-submodule")),
            snapshot_entry(".gitmodules", Action::Sync, Some("hash-gitmodules")),
        ];
        let snapshot = IndexedSnapshot::new("project", entries);

        let plan = engine.plan_snapshot(&snapshot, None, true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "src/main.rs"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Ignore { path } if path == "ignored.log"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::RebuildLocally { path } if path == "node_modules/pkg/index.js"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PlatformPinRedirected { path, required_os, .. }
                if path == "native/addon.node" && required_os == "macos"
        )));
        for git_path in [".git/config", "submodule/.git", ".gitmodules"] {
            assert!(plan.actions.iter().any(|action| matches!(
                action,
                ConvergenceAction::GitMetadataLocalOnly { path } if path == git_path
            )));
        }
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. }
                if path == "ignored.log"
                    || path == "node_modules/pkg/index.js"
                    || path == "native/addon.node"
                    || path.starts_with(".git")
                    || path == "submodule/.git"
        )));
    }

    #[test]
    fn remote_only_snapshot_paths_apply_local_policy_before_fetch() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![
                SnapshotEntry::new(
                    TreeEntry::directory("private", 1, 0o755),
                    PolicyMetadata::from_action(Action::Ignore),
                ),
                SnapshotEntry::new(
                    TreeEntry::directory("node_modules", 1, 0o755),
                    PolicyMetadata::from_action(Action::RebuildLocally),
                ),
            ],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-a",
            "project",
            vec![
                TreeEntry::file("private/cache.bin", 1, 10, 0o644, Some("hash-private".to_owned())),
                TreeEntry::file(
                    "node_modules/pkg/index.js",
                    1,
                    10,
                    0o644,
                    Some("hash-node".to_owned()),
                ),
                TreeEntry::file("native/addon.node", 1, 10, 0o644, Some("hash-native".to_owned())),
                TreeEntry::file(".git/config", 1, 10, 0o600, Some("hash-git".to_owned())),
                TreeEntry::file("src/remote.rs", 1, 10, 0o644, Some("hash-remote".to_owned())),
            ],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Ignore { path } if path == "private/cache.bin"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::RebuildLocally { path } if path == "node_modules/pkg/index.js"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PlatformPinAccepted { path } if path == "native/addon.node"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::GitMetadataLocalOnly { path } if path == ".git/config"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path == "src/remote.rs" && store_blob_id.as_deref() == Some("hash-remote")
        )));
        for policy_path in [
            "private/cache.bin",
            "node_modules/pkg/index.js",
            "native/addon.node",
            ".git/config",
        ] {
            assert!(!plan.actions.iter().any(|action| matches!(
                action,
                ConvergenceAction::FetchContent { path, .. } if path == policy_path
            )));
        }
    }

    #[test]
    fn remote_only_leaf_excluded_by_configured_syncignore_is_not_fetched() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        // No local ancestor for `secrets/leak.key`; only the configured
        // project `.syncignore` rule (`secrets/`) excludes it. A fresh
        // `Policy::new()` would miss the rule and fetch the leaf as Sync.
        let snapshot = IndexedSnapshot::new("project", Vec::new());
        let policy = Policy::from_syncignore("secrets/\n", "").expect("parsed syncignore");
        let remote_manifest = TreeManifest::new(
            "remote-manifest-secrets",
            "project",
            vec![
                TreeEntry::file(
                    "secrets/leak.key",
                    1,
                    10,
                    0o600,
                    Some("hash-leak".to_owned()),
                ),
                TreeEntry::file(
                    "src/remote.rs",
                    1,
                    10,
                    0o644,
                    Some("hash-remote".to_owned()),
                ),
            ],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &policy);

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Ignore { path } if path == "secrets/leak.key"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "secrets/leak.key"
        )));
        // A sibling remote-only path outside the ignore rule still fetches.
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path == "src/remote.rs" && store_blob_id.as_deref() == Some("hash-remote")
        )));
    }

    #[test]
    fn remote_only_reincluded_descendant_under_ignored_ancestor_is_fetched() {
        // `.syncignore` ignores `dist/` but re-includes `dist/keep.js`. The
        // local snapshot has the ignored `dist/` ancestor (the watcher indexed
        // the directory but suppressed its contents), while `dist/keep.js` is
        // remote-only. The configured leaf policy re-includes the leaf, so it
        // must fetch instead of inheriting the ancestor's Ignore.
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::directory("dist", 1, 0o755),
                PolicyMetadata::from_action(Action::Ignore),
            )],
        );
        let policy = Policy::from_syncignore("dist/\n!dist/keep.js\n", "").expect("parsed syncignore");
        let remote_manifest = TreeManifest::new(
            "remote-manifest-reinclude",
            "project",
            vec![
                TreeEntry::file(
                    "dist/keep.js",
                    1,
                    10,
                    0o644,
                    Some("hash-keep".to_owned()),
                ),
                TreeEntry::file(
                    "dist/other.js",
                    1,
                    10,
                    0o644,
                    Some("hash-other".to_owned()),
                ),
            ],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &policy);

        // Re-included leaf fetches.
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path == "dist/keep.js" && store_blob_id.as_deref() == Some("hash-keep")
        )));
        // Non-re-included sibling under the ignored ancestor stays ignored
        // (ancestor suppression applies because the leaf policy is suppressed).
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Ignore { path } if path == "dist/other.js"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "dist/other.js"
        )));
    }

    #[test]
    fn remote_only_reinclusion_does_not_override_non_overridable_git_metadata() {
        // A configured `!dist/.git/config` re-inclusion cannot sync Git
        // metadata even though the ancestor `dist/` is ignored.
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::directory("dist", 1, 0o755),
                PolicyMetadata::from_action(Action::Ignore),
            )],
        );
        let policy =
            Policy::from_syncignore("dist/\n!dist/.git/config\n", "").expect("parsed syncignore");
        let remote_manifest = TreeManifest::new(
            "remote-manifest-git-reinclude",
            "project",
            vec![TreeEntry::file(
                "dist/.git/config",
                1,
                10,
                0o600,
                Some("hash-git".to_owned()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &policy);

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::GitMetadataLocalOnly { path } if path == "dist/.git/config"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "dist/.git/config"
        )));
    }

    #[test]
    fn push_content_carries_watcher_fingerprint_while_store_blob_id_is_computed_from_bytes() {
        // The convergence plan carries the watcher/catalog source fingerprint
        // (an `fnv64:...`-style snapshot hash) on PushContent. The file-backed
        // store addresses content blobs by `store_blob_id_for_content(bytes)`
        // (a `sync_content_hash` value), which is distinct from the source
        // fingerprint. A real watcher push must compute the store blob id from
        // the payload bytes and use that id for `put_content_blob` and for the
        // remote manifest, not the raw watcher fingerprint.
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let watcher_fingerprint = "fnv64:0123456789abcdef".to_owned();
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![snapshot_entry("src/lib.rs", Action::Sync, Some(watcher_fingerprint.as_str()))],
        );

        let plan = engine.plan_snapshot(&snapshot, None, true, 0, &Policy::new());

        let push = plan
            .actions
            .iter()
            .find_map(|action| match action {
                ConvergenceAction::PushContent { path, source_content_hash } if path == "src/lib.rs" => {
                    Some(source_content_hash.clone())
                }
                _ => None,
            })
            .expect("push action for src/lib.rs");
        assert_eq!(push.as_deref(), Some(watcher_fingerprint.as_str()));

        // The store blob id is computed from payload bytes and differs from the
        // watcher fingerprint; it is what the file-backed store accepts.
        let payload = b"local file contents";
        let store_blob_id = store_blob_id_for_content(payload);
        assert_ne!(store_blob_id, watcher_fingerprint);
        assert_eq!(store_blob_id, sync_content_hash(payload));

        // The file-backed store accepts the computed store blob id, not the
        // raw watcher fingerprint.
        let root = temp_path("push-store-blob-id");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let store = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                vec![machine_a.clone()],
                secret,
            ),
            "project",
            machine_a,
        )
        .expect("store");
        store
            .put_content_blob(&store_blob_id, payload)
            .expect("store accepts computed store blob id");
        assert!(matches!(
            store.put_content_blob(&watcher_fingerprint, payload),
            Err(SyncStoreError::Integrity(message)) if message.contains("content payload id")
        ));
        assert_eq!(
            store.fetch_content_blob(&store_blob_id).expect("fetch by store blob id"),
            payload
        );
        remove_temp(&root);
    }

    #[test]
    fn fetch_content_carries_store_blob_id_from_remote_manifest() {
        // FetchContent carries the store blob id recorded in the remote
        // manifest (a `sync_content_hash` value), which the file-backed store
        // reads directly. It is not the raw watcher FNV fingerprint.
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let payload = b"remote file contents";
        let store_blob_id = sync_content_hash(payload);
        let snapshot = IndexedSnapshot::new("project", Vec::new());
        let remote_manifest = TreeManifest::new(
            "remote-manifest-store-id",
            "project",
            vec![TreeEntry::file(
                "src/remote.rs",
                payload.len() as u64,
                10,
                0o644,
                Some(store_blob_id.clone()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        let fetch_blob_id = plan
            .actions
            .iter()
            .find_map(|action| match action {
                ConvergenceAction::FetchContent { path, store_blob_id: blob_id, .. } if path == "src/remote.rs" => {
                    Some(blob_id.clone())
                }
                _ => None,
            })
            .expect("fetch action for src/remote.rs");
        assert_eq!(fetch_blob_id.as_deref(), Some(store_blob_id.as_str()));

        // The store blob id is what the file-backed store reads by.
        let root = temp_path("fetch-store-blob-id");
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_a = app_scoped_machine_id("machine-a").expect("machine id");
        let machine_b = app_scoped_machine_id("machine-b").expect("machine id");
        let store_a = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                vec![machine_a.clone(), machine_b.clone()],
                secret.clone(),
            ),
            "project",
            machine_a,
        )
        .expect("store a");
        let machine_a_again = app_scoped_machine_id("machine-a").expect("machine id");
        let store_b = FileBackedSyncStore::new(
            EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                root.clone(),
                vec![machine_a_again.clone(), machine_b],
                secret,
            ),
            "project",
            machine_a_again,
        )
        .expect("store b");
        store_a
            .put_content_blob(&store_blob_id, payload)
        .expect("store blob by store id");
        assert_eq!(
            store_b.fetch_content_blob(&store_blob_id).expect("fetch by store blob id"),
            payload
        );
        remove_temp(&root);
    }

    #[test]
    fn snapshot_planning_uses_remote_source_fingerprint_metadata_for_unchanged_file() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine.clone(), linux_platform("machine-a"));
        let source_hash = "fnv64:0123456789abcdef";
        let payload = b"unchanged file contents";
        let store_blob_id = sync_content_hash(payload);
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file(
                    "src/lib.rs",
                    payload.len() as u64,
                    7,
                    0o644,
                    Some(source_hash.to_owned()),
                ),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-source-sidecar",
            "project",
            vec![TreeEntry::file(
                "src/lib.rs",
                payload.len() as u64,
                7,
                0o644,
                Some(store_blob_id.clone()),
            )],
        );
        let prior_push = OperationRecord::from_draft(
            OperationDraft::new(1, "project", local_machine, OperationKind::PutContent, "src/lib.rs")
                .content_hash(source_hash)
                .payload_id(store_blob_id.clone())
                .modified_unix_millis(7)
                .permissions(0o644),
        );
        let remote_state = replay_operation_log(&[prior_push]);

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(
            plan.actions.is_empty(),
            "unchanged source fingerprint should not plan actions: {:?}",
            plan.actions
        );
        assert!(plan.queued_operations.is_empty());
    }

    #[test]
    fn snapshot_planning_fetches_for_verification_when_source_metadata_is_missing() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let source_hash = "fnv64:fedcba9876543210";
        let payload = b"remote bytes needing verification";
        let store_blob_id = sync_content_hash(payload);
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file(
                    "src/lib.rs",
                    payload.len() as u64,
                    7,
                    0o644,
                    Some(source_hash.to_owned()),
                ),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-store-only",
            "project",
            vec![TreeEntry::file(
                "src/lib.rs",
                payload.len() as u64,
                7,
                0o644,
                Some(store_blob_id.clone()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id: blob_id, .. }
                if path == "src/lib.rs" && blob_id.as_deref() == Some(store_blob_id.as_str())
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::ConflictSidecar { path, .. } if path == "src/lib.rs"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "src/lib.rs"
        )));
    }

    #[test]
    fn snapshot_planning_uses_conflict_sidecar_for_divergent_edits() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine.clone(), linux_platform("machine-a"));
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![snapshot_entry("file.txt", Action::Sync, Some("hash-local"))],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-a",
            "project",
            vec![TreeEntry::file(
                "file.txt",
                1,
                20,
                0o644,
                Some("hash-remote".to_owned()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::ConflictSidecar {
                path,
                sidecar_path,
                winner_machine_id,
                loser_machine_id,
                loser_content_hash,
                loser_bytes,
            } if path == "file.txt"
                && sidecar_path.contains(SYNC_CONFLICT_SIDECAR_SUFFIX)
                && winner_machine_id.as_str() == SYNC_REMOTE_MANIFEST_MACHINE_ID
                && loser_machine_id == &local_machine
                && loser_content_hash.as_str() == "hash-local"
                && loser_bytes.is_empty()
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path == "file.txt" && store_blob_id.as_deref() == Some("hash-remote")
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "file.txt"
        )));
    }

    #[test]
    fn snapshot_planning_local_winner_sidecar_identifies_remote_loser_payload() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine.clone(), linux_platform("machine-a"));
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![snapshot_entry("file.txt", Action::Sync, Some("hash-local"))],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-a",
            "project",
            vec![TreeEntry::file(
                "file.txt",
                1,
                0,
                0o644,
                Some("hash-remote".to_owned()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::ConflictSidecar {
                path,
                winner_machine_id,
                loser_machine_id,
                loser_content_hash,
                loser_bytes,
                ..
            } if path == "file.txt"
                && winner_machine_id == &local_machine
                && loser_machine_id.as_str() == SYNC_REMOTE_MANIFEST_MACHINE_ID
                && loser_content_hash.as_str() == "hash-remote"
                && loser_bytes.is_empty()
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, source_content_hash }
                if path == "file.txt" && source_content_hash.as_deref() == Some("hash-local")
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path.contains(SYNC_CONFLICT_SIDECAR_SUFFIX)
                    && store_blob_id.as_deref() == Some("hash-remote")
        )));
    }

    #[test]
    fn snapshot_planning_pushes_local_only_path_when_remote_manifest_omits_it() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![snapshot_entry("local-only.txt", Action::Sync, Some("hash-local"))],
        );
        let remote_manifest = TreeManifest::new("remote-empty", "project", Vec::new());

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, source_content_hash }
                if path == "local-only.txt" && source_content_hash.as_deref() == Some("hash-local")
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "local-only.txt"
        )));
    }

    #[test]
    fn snapshot_planning_suppresses_remote_only_stale_manifest_file_with_replay_tombstone() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let remote_machine = app_scoped_machine_id("machine-b").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine, linux_platform("machine-a"));
        let snapshot = IndexedSnapshot::new("project", Vec::new());
        let remote_manifest = TreeManifest::new(
            "remote-stale-delete",
            "project",
            vec![TreeEntry::file(
                "deleted.txt",
                1,
                10,
                0o644,
                Some("hash-stale".to_owned()),
            )],
        );
        let delete_operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", remote_machine, OperationKind::DeletePath, "deleted.txt")
                .modified_unix_millis(20),
        );
        let remote_state = replay_operation_log(&[delete_operation]);

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.is_empty());
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "deleted.txt"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, .. } if path == "deleted.txt"
        )));
    }

    #[test]
    fn snapshot_planning_suppresses_remote_only_stale_manifest_child_under_directory_tombstone() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let remote_machine = app_scoped_machine_id("machine-b").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine, linux_platform("machine-a"));
        let snapshot = IndexedSnapshot::new("project", Vec::new());
        let remote_manifest = TreeManifest::new(
            "remote-stale-directory-delete",
            "project",
            vec![TreeEntry::file(
                "deleted-dir/child.txt",
                1,
                10,
                0o644,
                Some("hash-child".to_owned()),
            )],
        );
        let delete_operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", remote_machine, OperationKind::DeletePath, "deleted-dir")
                .modified_unix_millis(20),
        );
        let remote_state = replay_operation_log(&[delete_operation]);

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.is_empty());
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "deleted-dir/child.txt"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, .. } if path == "deleted-dir/child.txt"
        )));
    }

    #[test]
    fn snapshot_planning_uses_remote_delete_tombstone_instead_of_reuploading_stale_local_file() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let remote_machine = app_scoped_machine_id("machine-b").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine, linux_platform("machine-a"));
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file("deleted.txt", 1, 10, 0o644, Some("hash-stale".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new("remote-after-delete", "project", Vec::new());
        let delete_operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", remote_machine, OperationKind::DeletePath, "deleted.txt")
                .modified_unix_millis(20),
        );
        let remote_state = replay_operation_log(&[delete_operation]);

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "deleted.txt"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "deleted.txt"
        )));
        assert!(plan.queued_operations.is_empty());
    }

    #[test]
    fn snapshot_planning_keeps_equal_mtime_replayed_delete_tombstone_after_higher_machine_put() {
        let source_machine = "dropbox-dev-ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
        let deleting_machine = "dropbox-dev-0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        assert!(deleting_machine.as_str() < source_machine.as_str());
        let platform = linux_platform(&source_machine);
        let engine = ConvergenceEngine::new(source_machine.clone(), platform);
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file("deleted-equal.txt", 1, 20, 0o644, Some("hash-stale".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new("remote-after-equal-delete", "project", Vec::new());
        let original_put = OperationRecord::from_draft(
            OperationDraft::new(
                1,
                "project",
                source_machine,
                OperationKind::PutContent,
                "deleted-equal.txt",
            )
            .content_hash("hash-stale")
            .payload_id("store-hash-stale")
            .modified_unix_millis(20)
            .permissions(0o644),
        );
        let delete_operation = OperationRecord::from_draft(
            OperationDraft::new(
                2,
                "project",
                deleting_machine,
                OperationKind::DeletePath,
                "deleted-equal.txt",
            )
            .modified_unix_millis(20),
        );
        let remote_state = replay_operation_log(&[original_put, delete_operation]);
        assert!(!remote_state.entries.contains_key("deleted-equal.txt"));
        assert!(remote_state.tombstones.contains_key("deleted-equal.txt"));

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "deleted-equal.txt"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "deleted-equal.txt"
        )));
        assert!(plan.queued_operations.is_empty());
    }

    #[test]
    fn snapshot_planning_uses_remote_move_tombstone_instead_of_reuploading_stale_source() {
        let local_machine = app_scoped_machine_id("machine-a").expect("machine id");
        let remote_machine = app_scoped_machine_id("machine-b").expect("machine id");
        let engine = ConvergenceEngine::new(local_machine, linux_platform("machine-a"));
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file("old-name.txt", 1, 10, 0o644, Some("hash-stale".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new(
            "remote-after-move",
            "project",
            vec![TreeEntry::file(
                "new-name.txt",
                1,
                20,
                0o644,
                Some("store-hash-new".to_owned()),
            )],
        );
        let move_operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", remote_machine, OperationKind::MovePath, "new-name.txt")
                .previous_path("old-name.txt")
                .content_hash("source-hash-new")
                .payload_id("store-hash-new")
                .modified_unix_millis(20)
                .permissions(0o644),
        );
        let remote_state = replay_operation_log(&[move_operation]);

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "old-name.txt"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path == "new-name.txt" && store_blob_id.as_deref() == Some("store-hash-new")
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "old-name.txt"
        )));
    }

    #[test]
    fn snapshot_planning_keeps_equal_mtime_replayed_move_tombstone_after_higher_machine_put() {
        let source_machine = "dropbox-dev-ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
        let moving_machine = "dropbox-dev-0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        assert!(moving_machine.as_str() < source_machine.as_str());
        let platform = linux_platform(&source_machine);
        let engine = ConvergenceEngine::new(source_machine.clone(), platform);
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file("old-equal-name.txt", 1, 20, 0o644, Some("hash-stale".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new(
            "remote-after-equal-move",
            "project",
            vec![TreeEntry::file(
                "new-equal-name.txt",
                1,
                20,
                0o644,
                Some("store-hash-new".to_owned()),
            )],
        );
        let original_put = OperationRecord::from_draft(
            OperationDraft::new(
                1,
                "project",
                source_machine,
                OperationKind::PutContent,
                "old-equal-name.txt",
            )
            .content_hash("source-hash-stale")
            .payload_id("store-hash-stale")
            .modified_unix_millis(20)
            .permissions(0o644),
        );
        let move_operation = OperationRecord::from_draft(
            OperationDraft::new(
                2,
                "project",
                moving_machine,
                OperationKind::MovePath,
                "new-equal-name.txt",
            )
            .previous_path("old-equal-name.txt")
            .content_hash("source-hash-new")
            .payload_id("store-hash-new")
            .modified_unix_millis(20)
            .permissions(0o644),
        );
        let remote_state = replay_operation_log(&[original_put, move_operation]);
        assert!(!remote_state.entries.contains_key("old-equal-name.txt"));
        assert!(remote_state.tombstones.contains_key("old-equal-name.txt"));
        assert_eq!(
            remote_state
                .entries
                .get("new-equal-name.txt")
                .and_then(|entry| entry.content_hash.as_deref()),
            Some("source-hash-new")
        );

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteLocal { path } if path == "old-equal-name.txt"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, store_blob_id, .. }
                if path == "new-equal-name.txt" && store_blob_id.as_deref() == Some("store-hash-new")
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "old-equal-name.txt"
        )));
    }

    #[test]
    fn snapshot_planning_ignores_sync_partial_artifacts_instead_of_uploading_them() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let partial_path = format!("dir/.file.txt{SYNC_PARTIAL_SUFFIX}");
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file(partial_path.clone(), 1, 10, 0o644, Some("hash-partial".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new("remote-empty", "project", Vec::new());

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Ignore { path } if path == &partial_path
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == &partial_path
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, .. } if path == &partial_path
        )));
    }

    #[test]
    fn event_planning_ignores_sync_partial_artifact_moves() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let before = snapshot_entry(
            &format!("dir/.file.txt{SYNC_PARTIAL_SUFFIX}"),
            Action::Sync,
            Some("hash-partial"),
        );
        let after = snapshot_entry("dir/file.txt", Action::Sync, Some("hash-final"));
        let event = FsEvent::moved(before, after);

        let plan = engine.plan_event("project", &event, true, 1);

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Noop { reason } if reason == "internal sync partial artifact ignored"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::MovePath { from, to }
                if from.ends_with(SYNC_PARTIAL_SUFFIX) || to == "dir/file.txt"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "dir/file.txt"
        )));
    }


    #[test]
    fn snapshot_planning_uses_metadata_actions_for_local_only_directories_and_symlinks() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![
                SnapshotEntry::new(
                    TreeEntry::directory("assets", 10, 0o755),
                    PolicyMetadata::from_action(Action::Sync),
                ),
                SnapshotEntry {
                    catalog_entry: TreeEntry::symlink(
                        "current",
                        11,
                        0o777,
                        Some("hash-target".to_owned()),
                    ),
                    policy: PolicyMetadata::from_action(Action::Sync),
                    symlink_target: Some("releases/v1".to_owned()),
                },
            ],
        );
        let remote_manifest = TreeManifest::new("remote-empty", "project", Vec::new());

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        for metadata_path in ["assets", "current"] {
            assert!(!plan.actions.iter().any(|action| matches!(
                action,
                ConvergenceAction::PushContent { path, .. } if path == metadata_path
            )));
        }
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "assets" && *permissions == 0o755
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "current" && *permissions == 0o777
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagateSymlink { path, target }
                if path == "current" && target.as_str() == "releases/v1"
        )));
    }

    #[test]
    fn snapshot_planning_uses_metadata_actions_for_remote_only_directories_and_symlinks() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new("project", Vec::new());
        let remote_manifest = TreeManifest::new(
            "remote-manifest-metadata",
            "project",
            vec![
                TreeEntry::directory("remote-assets", 20, 0o755),
                TreeEntry::symlink(
                    "remote-current",
                    21,
                    0o777,
                    Some("remote-target-hash".to_owned()),
                ),
            ],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        for metadata_path in ["remote-assets", "remote-current"] {
            assert!(!plan.actions.iter().any(|action| matches!(
                action,
                ConvergenceAction::FetchContent { path, .. } if path == metadata_path
            )));
        }
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "remote-assets" && *permissions == 0o755
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "remote-current" && *permissions == 0o777
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagateSymlink { path, .. } if path == "remote-current"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchSymlinkTargetMetadata { path, target_hash }
                if path == "remote-current" && target_hash.as_deref() == Some("remote-target-hash")
        )));
    }

    #[test]
    fn snapshot_planning_carries_remote_symlink_target_from_replay_state() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new("project", Vec::new());
        let remote_manifest = TreeManifest::new(
            "remote-manifest-metadata",
            "project",
            vec![TreeEntry::symlink(
                "remote-current",
                21,
                0o777,
                Some("remote-target-hash".to_owned()),
            )],
        );
        let remote_operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-b", OperationKind::SymlinkChanged, "remote-current")
                .content_hash("remote-target-hash")
                .modified_unix_millis(21)
                .permissions(0o777)
                .symlink_target("releases/v2"),
        );
        let remote_state = replay_operation_log(&[remote_operation]);

        let plan = engine.plan_snapshot_with_remote_state(
            &snapshot,
            Some(&remote_manifest),
            Some(&remote_state),
            true,
            0,
            &Policy::new(),
        );

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagateSymlink { path, target }
                if path == "remote-current" && target.as_str() == "releases/v2"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchSymlinkTargetMetadata { path, .. } if path == "remote-current"
        )));
    }

    #[test]
    fn snapshot_planning_applies_newer_remote_permissions_without_pushing_content() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file("script.sh", 1, 10, 0o644, Some("hash-same".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-permissions",
            "project",
            vec![TreeEntry::file(
                "script.sh",
                1,
                20,
                0o755,
                Some("hash-same".to_owned()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "script.sh"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "script.sh"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "script.sh" && *permissions == 0o755
        )));
    }

    #[test]
    fn snapshot_planning_pushes_newer_local_permissions_without_pushing_content() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let snapshot = IndexedSnapshot::new(
            "project",
            vec![SnapshotEntry::new(
                TreeEntry::file("script.sh", 1, 30, 0o700, Some("hash-same".to_owned())),
                PolicyMetadata::from_action(Action::Sync),
            )],
        );
        let remote_manifest = TreeManifest::new(
            "remote-manifest-permissions",
            "project",
            vec![TreeEntry::file(
                "script.sh",
                1,
                20,
                0o755,
                Some("hash-same".to_owned()),
            )],
        );

        let plan = engine.plan_snapshot(&snapshot, Some(&remote_manifest), true, 0, &Policy::new());

        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "script.sh"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::FetchContent { path, .. } if path == "script.sh"
        )));
        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "script.sh" && *permissions == 0o700
        )));
    }

    #[test]
    fn event_planning_uses_metadata_actions_for_created_directory_online_and_offline() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let directory = SnapshotEntry::new(
            TreeEntry::directory("assets", 10, 0o755),
            PolicyMetadata::from_action(Action::Sync),
        );
        let event = FsEvent::created(directory);

        let online_plan = engine.plan_event("project", &event, true, 1);

        assert!(online_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "assets" && *permissions == 0o755
        )));
        assert!(!online_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "assets"
        )));
        assert!(online_plan.queued_operations.is_empty());

        let offline_plan = engine.plan_event("project", &event, false, 2);

        assert!(offline_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::QueueOffline { path, .. } if path == "assets"
        )));
        assert!(!offline_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "assets"
        )));
        assert_eq!(offline_plan.queued_operations.len(), 1);
        let operation = &offline_plan.queued_operations[0];
        assert_eq!(operation.kind, OperationKind::PermissionChanged);
        assert_eq!(operation.path, "assets");
        assert_eq!(operation.permissions, Some(0o755));
        assert!(operation.content_hash.is_none());
    }

    #[test]
    fn event_planning_uses_symlink_actions_for_created_symlink_online_and_offline() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let symlink = SnapshotEntry {
            catalog_entry: TreeEntry::symlink(
                "current",
                11,
                0o777,
                Some("target-hash".to_owned()),
            ),
            policy: PolicyMetadata::from_action(Action::Sync),
            symlink_target: Some("releases/v1".to_owned()),
        };
        let event = FsEvent::created(symlink);

        let online_plan = engine.plan_event("project", &event, true, 1);

        assert!(online_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagatePermissions { path, permissions }
                if path == "current" && *permissions == 0o777
        )));
        assert!(online_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PropagateSymlink { path, target }
                if path == "current" && target.as_str() == "releases/v1"
        )));
        assert!(!online_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "current"
        )));
        assert!(online_plan.queued_operations.is_empty());

        let offline_plan = engine.plan_event("project", &event, false, 2);

        assert!(offline_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::QueueOffline { path, .. } if path == "current"
        )));
        assert!(!offline_plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::PushContent { path, .. } if path == "current"
        )));
        assert_eq!(offline_plan.queued_operations.len(), 1);
        let operation = &offline_plan.queued_operations[0];
        assert_eq!(operation.kind, OperationKind::SymlinkChanged);
        assert_eq!(operation.path, "current");
        assert_eq!(operation.permissions, Some(0o777));
        assert_eq!(operation.content_hash.as_deref(), Some("target-hash"));
        assert_eq!(operation.symlink_target.as_deref(), Some("releases/v1"));
    }

    #[test]
    fn suppressed_move_to_ignored_path_does_not_mutate_remote() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let before = snapshot_entry("src/app.js", Action::Sync, Some("hash-app"));
        let after = snapshot_entry("dist/app.js", Action::Ignore, Some("hash-app"));
        let event = FsEvent::moved(before, after);

        let plan = engine.plan_event("project", &event, true, 1);

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::Ignore { path } if path == "dist/app.js"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteRemote { path } if path == "src/app.js"
        )));
        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::MovePath { from, to } if from == "src/app.js" && to == "dist/app.js"
        )));
    }

    #[test]
    fn suppressed_delete_with_previous_sync_policy_does_not_mutate_remote() {
        let engine = ConvergenceEngine::new(
            app_scoped_machine_id("machine-a").expect("machine id"),
            linux_platform("machine-a"),
        );
        let before = snapshot_entry("src/app.js", Action::Sync, Some("hash-app"));
        let mut event = FsEvent::deleted(before);
        event.content_sync = ContentSyncDisposition::Suppressed;

        let plan = engine.plan_event("project", &event, true, 1);

        assert!(!plan.actions.iter().any(|action| matches!(
            action,
            ConvergenceAction::DeleteRemote { path } if path == "src/app.js"
        )));
        assert!(plan.queued_operations.is_empty());
    }

    #[test]
    fn sync_migration_applies_and_rolls_back() {
        let runner = sync_migration_runner().expect("runner");
        let mut store = crate::foundation::InMemoryMigrationStore::new();

        let applied = runner.apply(&mut store).expect("apply");
        assert_eq!(applied.schema_version, SYNC_MIGRATION_VERSION);
        let expected_tables = SYNC_MIGRATION_TABLES
            .iter()
            .map(|table| (*table).to_owned())
            .collect::<Vec<_>>();
        assert_eq!(applied.product_tables, expected_tables);

        let rolled_back = runner.rollback(&mut store).expect("rollback");
        assert_eq!(rolled_back.schema_version, crate::foundation::BASELINE_SCHEMA_VERSION);
        assert!(rolled_back.product_tables.is_empty());
    }

    #[test]
    fn backup_and_restore_recover_sync_store_state() {
        let mut store = AuthoritativeSyncStore::new("project");
        let operation = OperationRecord::from_draft(
            OperationDraft::new(1, "project", "machine-a", OperationKind::PutContent, "file.txt")
                .content_hash("hash-a")
                .modified_unix_millis(1)
                .permissions(0o644),
        );
        store.append_operation(operation.clone());
        store.put_content("hash-a", b"before".to_vec());
        let backup = store.backup("merge-boundary");

        store.append_operation(OperationRecord::from_draft(
            OperationDraft::new(2, "project", "machine-b", OperationKind::DeletePath, "file.txt")
                .modified_unix_millis(2),
        ));
        store.content_payloads.clear();
        store.restore(backup);

        assert_eq!(store.operation_log(), &[operation]);
        assert_eq!(store.content_payloads.get("hash-a"), Some(&b"before".to_vec()));
    }

    #[test]
    fn file_backed_backup_and_restore_recover_wire_state() {
        let root = temp_path("file-backup-source");
        let backup = temp_path("file-backup-copy");
        let restored = temp_path("file-backup-restored");
        let nested = root.join("sync_store_v1/projects/project/oplog");
        fs::create_dir_all(&nested).expect("create source");
        fs::write(nested.join("one.syncop"), b"encrypted bytes").expect("write source");

        let report = backup_file_backed_sync_store(&root, &backup).expect("backup");
        assert_eq!(report.copied_files, vec![PathBuf::from("sync_store_v1/projects/project/oplog/one.syncop")]);

        fs::remove_file(nested.join("one.syncop")).expect("remove source file");
        restore_file_backed_sync_store(&backup, &restored).expect("restore");
        let restored_bytes = fs::read(restored.join("sync_store_v1/projects/project/oplog/one.syncop"))
            .expect("read restored");
        assert_eq!(restored_bytes, b"encrypted bytes".to_vec());
        remove_temp(&root);
        remove_temp(&backup);
        remove_temp(&restored);
    }

    #[test]
    fn restore_keeps_live_store_when_backup_cannot_be_validated() {
        let missing_backup = temp_path("missing-backup");
        let destination = temp_path("restore-live-destination");
        let live_file = destination.join("sync_store_v1/projects/project/oplog/live.syncop");
        fs::create_dir_all(live_file.parent().expect("live parent")).expect("create live store");
        fs::write(&live_file, b"live bytes").expect("write live store");

        assert!(matches!(
            restore_file_backed_sync_store(&missing_backup, &destination),
            Err(SyncStoreError::Missing(_))
        ));

        assert_eq!(fs::read(&live_file).expect("live still present"), b"live bytes".to_vec());
        remove_temp(&destination);
    }

    #[test]
    fn atomic_content_write_uses_final_rename() {
        let root = temp_path("atomic-write");
        let final_path = root.join("dir/file.txt");

        write_content_atomically(&final_path, b"complete").expect("atomic write");

        assert_eq!(fs::read(&final_path).expect("final read"), b"complete".to_vec());
        assert!(!final_path.with_file_name(".file.txt.sync-partial").exists());
        remove_temp(&root);
    }

    #[test]
    fn append_only_write_is_idempotent_and_never_overwrites_existing_object() {
        let root = temp_path("append-only-existing");
        let final_path = root.join("objects/payload.bin");

        write_append_only(&final_path, b"first bytes").expect("initial write");
        write_append_only(&final_path, b"first bytes").expect("idempotent write");
        assert!(matches!(
            write_append_only(&final_path, b"second bytes"),
            Err(SyncStoreError::Integrity(message)) if message.contains("different bytes")
        ));

        assert_eq!(fs::read(&final_path).expect("final read"), b"first bytes".to_vec());
        remove_temp(&root);
    }

    #[test]
    fn append_only_racing_creates_keep_one_complete_object() {
        let root = temp_path("append-only-race");
        let final_path = root.join("objects/payload.bin");
        let first_path = final_path.clone();
        let second_path = final_path.clone();
        let first_thread = std::thread::spawn(move || {
            let bytes = b"first writer".to_vec();
            let result = write_append_only(&first_path, &bytes);
            result.map(|()| bytes)
        });
        let second_thread = std::thread::spawn(move || {
            let bytes = b"second writer".to_vec();
            let result = write_append_only(&second_path, &bytes);
            result.map(|()| bytes)
        });

        let first_result = first_thread.join().expect("first writer joined");
        let second_result = second_thread.join().expect("second writer joined");
        let final_bytes = fs::read(&final_path).expect("final read");
        let mut winners = Vec::new();
        for result in [first_result, second_result] {
            match result {
                Ok(bytes) => winners.push(bytes),
                Err(SyncStoreError::Integrity(message)) => assert!(message.contains("different bytes")),
                Err(error) => panic!("unexpected append-only race error: {error}"),
            }
        }

        assert_eq!(winners.len(), 1);
        assert_eq!(final_bytes, winners[0]);
        assert!(final_bytes == b"first writer".to_vec() || final_bytes == b"second writer".to_vec());
        remove_temp(&root);
    }

    #[test]
    fn endpoint_security_rejects_plaintext_and_unauthorized_settings() {
        let secret = SharedSecret::from_pairing_token("shared pairing token").expect("secret");
        let machine_id = app_scoped_machine_id("machine-a").expect("machine id");
        let mut config = EndpointSecurityConfig::file_backed(
            TransportMode::Production,
            temp_path("secure-endpoint"),
            vec![machine_id],
            secret,
        );
        config.security = TransportSecurity::plaintext_forbidden();

        assert!(config.validate().is_err());
    }

    #[test]
    fn safe_path_component_encodes_dot_segments() {
        assert_eq!(safe_path_component(""), "_");
        assert_eq!(safe_path_component("."), "%2E");
        assert_eq!(safe_path_component(".."), "%2E%2E");
        assert_eq!(safe_path_component("project/../escape"), "project%2F..%2Fescape");
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            hex_bytes(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn snapshot_entry(path: &str, action: Action, content_hash: Option<&str>) -> SnapshotEntry {
        let policy = PolicyMetadata::from_action(action);
        SnapshotEntry {
            catalog_entry: TreeEntry::file(path, 1, 1, 0o644, content_hash.map(|value| value.to_owned())),
            policy,
            symlink_target: None,
        }
    }

    fn linux_platform(source: &str) -> Platform {
        Platform {
            os_family: OsFamily::Linux,
            os_version: Some("test".to_owned()),
            architecture: Architecture::X86_64,
            capabilities: PlatformCapabilities::for_os(&OsFamily::Linux),
            machine_id: MachineId {
                value: app_scoped_machine_id(source).expect("machine id"),
                provenance: MachineIdProvenance::Fallback,
            },
        }
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|candidate| candidate == needle)
    }

    fn temp_path(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "dropbox-dev-sync-{label}-{}-{now}-{counter}",
            std::process::id()
        ))
    }

    fn remove_temp(path: &Path) {
        if path.exists() {
            fs::remove_dir_all(path).expect("remove temp");
        }
    }
}
