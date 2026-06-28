//! Environment-variable sync over the CHUNK-05 authenticated/encrypted payload path.
//!
//! U7 resolution: environment secrets are protected by a per-machine keyring
//! abstraction. The local master key bytes are supplied out-of-band to an
//! [`EnvKeyProvider`]; only [`EnvKeyVersionRecord`] metadata (version, digest,
//! creation time, active marker) is stored or audited. The stdlib-only
//! [`StdlibTestKeyProvider`] is intentionally a baseline test provider, not a
//! production key store. Rotation is modeled by adding a new active key version
//! while keeping previous version records available for old payloads. Debug,
//! audit, migration, and transport metadata never contain plaintext key bytes.
//!
//! Env payloads are first sealed with the env key version and then submitted as
//! `generic:env-record-v1` payloads to CHUNK-05 [`FileBackedSyncStore`]. Machine
//! override payloads use target-bound encryption material derived from target
//! machine secret material, project id, target machine id, and key version before entering the generic transport.
//! Env code does not write transport files directly: the sync store remains responsible
//! for the outer authenticated/encrypted envelope, append-only operation log,
//! authorized-machine checks, and at-rest wire format.
//!
//! Conflict handling reuses the CHUNK-05 U5 last-writer-wins plus sidecar policy
//! for same-variable divergent edits. The sidecar records redacted env metadata
//! and the losing encrypted artifact payload id plus digest rather than the losing secret.

use crate::catalog::{ContentHash, ProjectId};
use crate::foundation::{MachineId, Migration, MigrationError, MigrationRunner};
use crate::sync::{
    resolve_same_file_conflict, FileBackedSyncStore, FileVersion, ManualConflictResolution,
    OperationDraft, OperationKind, OperationRecord, PayloadId, SyncStoreError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

pub const MODULE_NAME: &str = "env";
pub const ENV_SYNC_PAYLOAD_TYPE: &str = "env-record-v1";
pub const ENV_PAYLOAD_FORMAT_VERSION: &str = "env-payload-lines-v1";
pub const ENV_RECORD_FORMAT_VERSION: &str = "env-record-lines-v1";
pub const ENV_AUDIT_FORMAT_VERSION: &str = "env-audit-lines-v1";
pub const ENV_SESSION_EXPORT_FORMAT_VERSION: &str = "env-session-export-lines-v1";
pub const ENV_OPERATION_PATH_PREFIX: &str = "env/";
const ENV_OVERRIDE_OPERATION_PATH_PREFIX: &str = "env/override/";
pub const ENV_REDACTED_VALUE: &str = "<redacted>";
pub const ENV_U7_KEYRING_RATIONALE: &str = "local env master key material and machine override secret material are supplied out-of-band through a per-machine key provider; only key version records and digests are persisted or audited; stdlib test provider is a baseline harness and rotation keeps old versions for decrypting old payloads";
pub const ENV_TRANSPORT_CONTRACT: &str = "env payloads are sealed by env key version and carried only as CHUNK-05 generic authenticated/encrypted payload envelopes; machine override payloads use target-bound material derived from target machine secret material, project id, target machine id, and key version; env sync does not bypass transport";

pub const ENV_MIGRATION_VERSION: &str = "env_v1";
pub const ENV_MIGRATION_DESCRIPTION: &str =
    "environment variable sync schema; key version records, encrypted payload references, overrides, conflict sidecars, audit log";
pub const ENV_KEY_VERSIONS_TABLE: &str = "env_key_versions";
pub const ENV_VARIABLES_TABLE: &str = "env_variables";
pub const ENV_MACHINE_OVERRIDES_TABLE: &str = "env_machine_overrides";
pub const ENV_CONFLICT_SIDECARS_TABLE: &str = "env_conflict_sidecars";
pub const ENV_AUDIT_LOG_TABLE: &str = "env_audit_log";
pub const ENV_MIGRATION_TABLES: &[&str] = &[
    ENV_KEY_VERSIONS_TABLE,
    ENV_VARIABLES_TABLE,
    ENV_MACHINE_OVERRIDES_TABLE,
    ENV_CONFLICT_SIDECARS_TABLE,
    ENV_AUDIT_LOG_TABLE,
];
pub const ENV_MIGRATION_UP_SQL: &[&str] = &[
    concat!(
        "CREATE TABLE env_key_versions (",
        "key_version TEXT PRIMARY KEY, ",
        "keyring_machine_id TEXT NOT NULL, ",
        "key_digest TEXT NOT NULL, ",
        "created_logical_millis INTEGER NOT NULL, ",
        "active INTEGER NOT NULL);"
    ),
    concat!(
        "CREATE TABLE env_variables (",
        "env_name TEXT PRIMARY KEY, ",
        "value_payload_id TEXT NOT NULL, ",
        "key_version TEXT NOT NULL, ",
        "author_machine_id TEXT NOT NULL, ",
        "modified_unix_millis INTEGER NOT NULL, ",
        "operation_id TEXT NOT NULL);"
    ),
    concat!(
        "CREATE TABLE env_machine_overrides (",
        "target_machine_id TEXT NOT NULL, ",
        "env_name TEXT NOT NULL, ",
        "value_payload_id TEXT NOT NULL, ",
        "key_version TEXT NOT NULL, ",
        "author_machine_id TEXT NOT NULL, ",
        "modified_unix_millis INTEGER NOT NULL, ",
        "operation_id TEXT NOT NULL, ",
        "PRIMARY KEY (target_machine_id, env_name));"
    ),
    concat!(
        "CREATE TABLE env_conflict_sidecars (",
        "sidecar_path TEXT PRIMARY KEY, ",
        "original_env_name TEXT NOT NULL, ",
        "scope TEXT NOT NULL, ",
        "loser_machine_id TEXT NOT NULL, ",
        "winner_machine_id TEXT NOT NULL, ",
        "loser_payload_id TEXT NOT NULL, ",
        "loser_payload_digest TEXT NOT NULL, ",
        "manual_escape_hatch TEXT NOT NULL);"
    ),
    concat!(
        "CREATE TABLE env_audit_log (",
        "audit_ordinal INTEGER PRIMARY KEY, ",
        "event_unix_millis INTEGER NOT NULL, ",
        "operation TEXT NOT NULL, ",
        "actor_machine_id TEXT NOT NULL, ",
        "env_name TEXT, ",
        "scope TEXT, ",
        "payload_id TEXT, ",
        "key_version TEXT, ",
        "status TEXT NOT NULL, ",
        "redacted_value TEXT NOT NULL);"
    ),
];
pub const ENV_MIGRATION_DOWN_SQL: &[&str] = &[
    "DROP TABLE env_audit_log;",
    "DROP TABLE env_conflict_sidecars;",
    "DROP TABLE env_machine_overrides;",
    "DROP TABLE env_variables;",
    "DROP TABLE env_key_versions;",
];

type EnvResult<T> = Result<T, EnvSyncError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSyncError {
    InvalidConfig(String),
    MissingKey(String),
    KeyMismatch(String),
    Transport(String),
    Decode(String),
    Integrity(String),
    Conflict(String),
}

impl EnvSyncError {
    fn invalid_config(message: impl Into<String>) -> Self {
        Self::InvalidConfig(message.into())
    }

    fn missing_key(message: impl Into<String>) -> Self {
        Self::MissingKey(message.into())
    }

    fn key_mismatch(message: impl Into<String>) -> Self {
        Self::KeyMismatch(message.into())
    }

    fn decode(message: impl Into<String>) -> Self {
        Self::Decode(message.into())
    }

    fn integrity(message: impl Into<String>) -> Self {
        Self::Integrity(message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }
}

impl fmt::Display for EnvSyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "ENV_CONFIG_INVALID: {message}"),
            Self::MissingKey(message) => write!(formatter, "ENV_KEY_MISSING: {message}"),
            Self::KeyMismatch(message) => write!(formatter, "ENV_KEY_MISMATCH: {message}"),
            Self::Transport(message) => write!(formatter, "ENV_TRANSPORT: {message}"),
            Self::Decode(message) => write!(formatter, "ENV_DECODE: {message}"),
            Self::Integrity(message) => write!(formatter, "ENV_INTEGRITY: {message}"),
            Self::Conflict(message) => write!(formatter, "ENV_CONFLICT: {message}"),
        }
    }
}

impl Error for EnvSyncError {}

impl From<SyncStoreError> for EnvSyncError {
    fn from(error: SyncStoreError) -> Self {
        Self::Transport(error.to_string())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvMasterKey {
    key_version: String,
    material: Vec<u8>,
}

impl EnvMasterKey {
    pub fn new(key_version: impl Into<String>, material: impl AsRef<[u8]>) -> EnvResult<Self> {
        let key_version = key_version.into();
        validate_key_version(&key_version)?;
        let material = material.as_ref();
        if material.is_empty() {
            return Err(EnvSyncError::invalid_config(
                "env master key material must not be empty",
            ));
        }
        Ok(Self {
            key_version,
            material: material.to_vec(),
        })
    }

    pub fn key_version(&self) -> &str {
        &self.key_version
    }

    pub fn digest_hex(&self) -> String {
        env_digest_hex(b"env:key-digest:v1", &[&self.material])
    }

    fn material(&self) -> &[u8] {
        &self.material
    }
}

impl fmt::Debug for EnvMasterKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvMasterKey")
            .field("key_version", &self.key_version)
            .field("digest", &self.digest_hex())
            .field("material", &ENV_REDACTED_VALUE)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvKeyVersionRecord {
    pub keyring_machine_id: String,
    pub key_version: String,
    pub key_digest: String,
    pub created_logical_millis: u64,
    pub active: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvMachineOverrideSecret {
    key_version: String,
    machine_id: String,
    material: Vec<u8>,
}

impl EnvMachineOverrideSecret {
    pub fn new(
        key_version: impl Into<String>,
        machine_id: impl Into<String>,
        material: impl AsRef<[u8]>,
    ) -> EnvResult<Self> {
        let key_version = key_version.into();
        validate_key_version(&key_version)?;
        let machine_id = machine_id.into();
        validate_machine_id(&machine_id, "env machine override secret machine id")?;
        let material = material.as_ref();
        if material.is_empty() {
            return Err(EnvSyncError::invalid_config(
                "env machine override secret material must not be empty",
            ));
        }
        Ok(Self {
            key_version,
            machine_id,
            material: material.to_vec(),
        })
    }

    pub fn key_version(&self) -> &str {
        &self.key_version
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn digest_hex(&self) -> String {
        env_digest_hex(b"env:machine-override-secret-digest:v1", &[&self.material])
    }

    fn material(&self) -> &[u8] {
        &self.material
    }
}

impl fmt::Debug for EnvMachineOverrideSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvMachineOverrideSecret")
            .field("key_version", &self.key_version)
            .field("machine_id", &self.machine_id)
            .field("digest", &self.digest_hex())
            .field("material", &ENV_REDACTED_VALUE)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct StoredEnvKey {
    key: EnvMasterKey,
    created_logical_millis: u64,
}

pub trait EnvKeyProvider {
    fn keyring_machine_id(&self) -> &str;
    fn latest_key_version(&self) -> EnvResult<EnvKeyVersionRecord>;
    fn key_for_version(&self, key_version: &str) -> EnvResult<EnvMasterKey>;
    fn override_secret_for_target(
        &self,
        key_version: &str,
        target_machine_id: &str,
    ) -> EnvResult<EnvMachineOverrideSecret> {
        validate_key_version(key_version)?;
        validate_machine_id(target_machine_id, "env override target machine id")?;
        Err(EnvSyncError::missing_key(format!(
            "env override secret for target machine `{target_machine_id}` and key version `{key_version}` is not available in local keyring"
        )))
    }
    fn key_version_records(&self) -> Vec<EnvKeyVersionRecord>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct StdlibTestKeyProvider {
    keyring_machine_id: String,
    active_version: Option<String>,
    versions: BTreeMap<String, StoredEnvKey>,
    override_secrets: BTreeMap<String, BTreeMap<String, EnvMachineOverrideSecret>>,
}

impl StdlibTestKeyProvider {
    pub fn new(keyring_machine_id: impl Into<String>) -> EnvResult<Self> {
        let keyring_machine_id = keyring_machine_id.into();
        validate_machine_id(&keyring_machine_id, "env keyring machine id")?;
        Ok(Self {
            keyring_machine_id,
            active_version: None,
            versions: BTreeMap::new(),
            override_secrets: BTreeMap::new(),
        })
    }

    pub fn with_key(
        keyring_machine_id: impl Into<String>,
        key_version: impl Into<String>,
        material: impl AsRef<[u8]>,
        created_logical_millis: u64,
    ) -> EnvResult<Self> {
        let mut provider = Self::new(keyring_machine_id)?;
        provider.provision_key_version(key_version, material, created_logical_millis)?;
        Ok(provider)
    }

    pub fn provision_key_version(
        &mut self,
        key_version: impl Into<String>,
        material: impl AsRef<[u8]>,
        created_logical_millis: u64,
    ) -> EnvResult<EnvKeyVersionRecord> {
        let key = EnvMasterKey::new(key_version, material)?;
        let key_version = key.key_version.clone();
        if self.versions.contains_key(&key_version) {
            return Err(EnvSyncError::invalid_config(format!(
                "env key version `{key_version}` is already provisioned"
            )));
        }
        self.versions.insert(
            key_version.clone(),
            StoredEnvKey {
                key,
                created_logical_millis,
            },
        );
        self.active_version = Some(key_version.clone());
        self.record_for_version(&key_version)
    }

    pub fn rotate_key(
        &mut self,
        key_version: impl Into<String>,
        material: impl AsRef<[u8]>,
        created_logical_millis: u64,
    ) -> EnvResult<EnvKeyVersionRecord> {
        self.provision_key_version(key_version, material, created_logical_millis)
    }

    pub fn provision_machine_override_secret(
        &mut self,
        key_version: impl Into<String>,
        machine_id: impl Into<String>,
        material: impl AsRef<[u8]>,
    ) -> EnvResult<EnvMachineOverrideSecret> {
        let key_version = key_version.into();
        validate_key_version(&key_version)?;
        if !self.versions.contains_key(&key_version) {
            return Err(EnvSyncError::missing_key(format!(
                "env key version `{key_version}` must be provisioned before its override secret"
            )));
        }
        let machine_id = machine_id.into();
        validate_machine_id(&machine_id, "env machine override secret machine id")?;
        if machine_id != self.keyring_machine_id {
            return Err(EnvSyncError::key_mismatch(format!(
                "env override secret target `{machine_id}` does not match keyring machine `{}`",
                self.keyring_machine_id
            )));
        }
        let secrets_for_version = self
            .override_secrets
            .entry(key_version.clone())
            .or_default();
        if secrets_for_version.contains_key(&machine_id) {
            return Err(EnvSyncError::invalid_config(format!(
                "env override secret for machine `{machine_id}` and key version `{key_version}` is already provisioned"
            )));
        }
        let secret = EnvMachineOverrideSecret::new(key_version, machine_id.clone(), material)?;
        secrets_for_version.insert(machine_id, secret.clone());
        Ok(secret)
    }

    fn record_for_version(&self, key_version: &str) -> EnvResult<EnvKeyVersionRecord> {
        let stored = self.versions.get(key_version).ok_or_else(|| {
            EnvSyncError::missing_key(format!("env key version `{key_version}` is not available"))
        })?;
        Ok(EnvKeyVersionRecord {
            keyring_machine_id: self.keyring_machine_id.clone(),
            key_version: key_version.to_owned(),
            key_digest: stored.key.digest_hex(),
            created_logical_millis: stored.created_logical_millis,
            active: self.active_version.as_deref() == Some(key_version),
        })
    }
}

impl fmt::Debug for StdlibTestKeyProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StdlibTestKeyProvider")
            .field("keyring_machine_id", &self.keyring_machine_id)
            .field("active_version", &self.active_version)
            .field("key_version_records", &self.key_version_records())
            .field(
                "override_secret_count",
                &self.override_secrets.values().map(|secrets| secrets.len()).sum::<usize>(),
            )
            .field("material", &ENV_REDACTED_VALUE)
            .finish()
    }
}

impl EnvKeyProvider for StdlibTestKeyProvider {
    fn keyring_machine_id(&self) -> &str {
        &self.keyring_machine_id
    }

    fn latest_key_version(&self) -> EnvResult<EnvKeyVersionRecord> {
        let active_version = self.active_version.as_deref().ok_or_else(|| {
            EnvSyncError::missing_key("env keyring has no active key version provisioned")
        })?;
        self.record_for_version(active_version)
    }

    fn key_for_version(&self, key_version: &str) -> EnvResult<EnvMasterKey> {
        validate_key_version(key_version)?;
        self.versions
            .get(key_version)
            .map(|stored| stored.key.clone())
            .ok_or_else(|| {
                EnvSyncError::missing_key(format!(
                    "env key version `{key_version}` is not available in local keyring"
                ))
            })
    }

    fn override_secret_for_target(
        &self,
        key_version: &str,
        target_machine_id: &str,
    ) -> EnvResult<EnvMachineOverrideSecret> {
        validate_key_version(key_version)?;
        validate_machine_id(target_machine_id, "env override target machine id")?;
        self.override_secrets
            .get(key_version)
            .and_then(|secrets| secrets.get(target_machine_id))
            .cloned()
            .ok_or_else(|| {
                EnvSyncError::missing_key(format!(
                    "env override secret for target machine `{target_machine_id}` and key version `{key_version}` is not available in local keyring"
                ))
            })
    }

    fn key_version_records(&self) -> Vec<EnvKeyVersionRecord> {
        self.versions
            .keys()
            .filter_map(|key_version| self.record_for_version(key_version).ok())
            .collect()
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvSecretValue(String);

impl EnvSecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_for_materialization(&self) -> &str {
        &self.0
    }

    pub fn redacted(&self) -> RedactedEnvValue {
        RedactedEnvValue
    }
}

impl fmt::Debug for EnvSecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvSecretValue")
            .field("value", &ENV_REDACTED_VALUE)
            .finish()
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct RedactedEnvValue;

impl RedactedEnvValue {
    pub const fn as_str(self) -> &'static str {
        ENV_REDACTED_VALUE
    }
}

impl fmt::Debug for RedactedEnvValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(ENV_REDACTED_VALUE)
    }
}

impl fmt::Display for RedactedEnvValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(ENV_REDACTED_VALUE)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvNeverSyncPolicy {
    exact_names: BTreeSet<String>,
    prefixes: BTreeSet<String>,
    suffixes: BTreeSet<String>,
}

impl EnvNeverSyncPolicy {
    pub fn allow_all() -> Self {
        Self::default()
    }

    pub fn deny_names<I, S>(names: I) -> EnvResult<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut policy = Self::default();
        for name in names {
            policy.deny_name(name)?;
        }
        Ok(policy)
    }

    pub fn deny_name(&mut self, name: impl Into<String>) -> EnvResult<&mut Self> {
        let name = name.into();
        validate_env_name(&name)?;
        self.exact_names.insert(name);
        Ok(self)
    }

    pub fn deny_prefix(&mut self, prefix: impl Into<String>) -> EnvResult<&mut Self> {
        let prefix = prefix.into();
        validate_env_name_fragment(&prefix, "env never-sync prefix")?;
        self.prefixes.insert(prefix);
        Ok(self)
    }

    pub fn deny_suffix(&mut self, suffix: impl Into<String>) -> EnvResult<&mut Self> {
        let suffix = suffix.into();
        validate_env_name_fragment(&suffix, "env never-sync suffix")?;
        self.suffixes.insert(suffix);
        Ok(self)
    }

    pub fn rejects(&self, name: &str) -> bool {
        self.exact_names.contains(name)
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
            || self.suffixes.iter().any(|suffix| name.ends_with(suffix))
    }

    fn validate_publish(&self, scope: &EnvScope, name: &str) -> EnvResult<()> {
        if self.rejects(name) {
            return Err(EnvSyncError::invalid_config(format!(
                "env variable `{name}` in scope `{}` is marked never-sync and was not published",
                scope.as_wire()
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnvScope {
    Shared,
    MachineOverride { machine_id: String },
}

impl EnvScope {
    pub fn shared() -> Self {
        Self::Shared
    }

    pub fn machine_override(machine_id: impl Into<String>) -> EnvResult<Self> {
        let machine_id = machine_id.into();
        validate_machine_id(&machine_id, "env override target machine id")?;
        Ok(Self::MachineOverride { machine_id })
    }

    pub fn applies_to_machine(&self, machine_id: &str) -> bool {
        match self {
            Self::Shared => true,
            Self::MachineOverride {
                machine_id: target,
            } => target == machine_id,
        }
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Shared => "shared".to_owned(),
            Self::MachineOverride { machine_id } => format!("machine:{machine_id}"),
        }
    }

    fn from_wire(value: &str) -> EnvResult<Self> {
        if value == "shared" {
            return Ok(Self::Shared);
        }
        if let Some(machine_id) = value.strip_prefix("machine:") {
            return Self::machine_override(machine_id.to_owned());
        }
        Err(EnvSyncError::decode(format!(
            "unknown env scope `{value}`"
        )))
    }

    fn path_component(&self) -> String {
        match self {
            Self::Shared => "shared".to_owned(),
            Self::MachineOverride { machine_id } => {
                format!("override/{}", safe_path_component(machine_id))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvRecord {
    pub name: String,
    pub scope: EnvScope,
    pub value: EnvSecretValue,
    pub author_machine_id: String,
    pub modified_unix_millis: u64,
    pub sequence: u64,
    pub key_version: String,
}

impl EnvRecord {
    pub fn shared(
        name: impl Into<String>,
        value: impl Into<String>,
        author_machine_id: impl Into<String>,
        modified_unix_millis: u64,
        sequence: u64,
        key_version: impl Into<String>,
    ) -> EnvResult<Self> {
        Self::new(
            name,
            EnvScope::Shared,
            value,
            author_machine_id,
            modified_unix_millis,
            sequence,
            key_version,
        )
    }

    pub fn override_for_machine(
        target_machine_id: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
        author_machine_id: impl Into<String>,
        modified_unix_millis: u64,
        sequence: u64,
        key_version: impl Into<String>,
    ) -> EnvResult<Self> {
        Self::new(
            name,
            EnvScope::machine_override(target_machine_id)?,
            value,
            author_machine_id,
            modified_unix_millis,
            sequence,
            key_version,
        )
    }

    fn new(
        name: impl Into<String>,
        scope: EnvScope,
        value: impl Into<String>,
        author_machine_id: impl Into<String>,
        modified_unix_millis: u64,
        sequence: u64,
        key_version: impl Into<String>,
    ) -> EnvResult<Self> {
        let name = name.into();
        validate_env_name(&name)?;
        let author_machine_id = author_machine_id.into();
        validate_machine_id(&author_machine_id, "env author machine id")?;
        let key_version = key_version.into();
        validate_key_version(&key_version)?;
        Ok(Self {
            name,
            scope,
            value: EnvSecretValue::new(value),
            author_machine_id,
            modified_unix_millis,
            sequence,
            key_version,
        })
    }

    pub fn operation_path(&self) -> String {
        env_operation_path(&self.scope, &self.name)
    }

    fn to_wire_bytes(&self, project_id: &str) -> EnvResult<Vec<u8>> {
        validate_project_id(project_id)?;
        let mut output = String::new();
        push_kv(&mut output, "format", ENV_RECORD_FORMAT_VERSION);
        push_kv(&mut output, "project_id", project_id);
        push_kv(&mut output, "name", &self.name);
        push_kv(&mut output, "scope", &self.scope.as_wire());
        push_kv(&mut output, "value", self.value.expose_for_materialization());
        push_kv(&mut output, "author_machine_id", &self.author_machine_id);
        push_kv(
            &mut output,
            "modified_unix_millis",
            &self.modified_unix_millis.to_string(),
        );
        push_kv(&mut output, "sequence", &self.sequence.to_string());
        push_kv(&mut output, "key_version", &self.key_version);
        Ok(output.into_bytes())
    }

    fn from_wire_bytes(bytes: &[u8], expected_project_id: &str) -> EnvResult<Self> {
        let fields = parse_kv_lines(bytes)?;
        let format = required_field(&fields, "format")?;
        if format != ENV_RECORD_FORMAT_VERSION {
            return Err(EnvSyncError::decode(format!(
                "unsupported env record format `{format}`"
            )));
        }
        let project_id = required_field(&fields, "project_id")?;
        if project_id != expected_project_id {
            return Err(EnvSyncError::integrity(format!(
                "env record belongs to project `{project_id}`, not `{expected_project_id}`"
            )));
        }
        Self::new(
            required_field(&fields, "name")?.to_owned(),
            EnvScope::from_wire(required_field(&fields, "scope")?)?,
            required_field(&fields, "value")?.to_owned(),
            required_field(&fields, "author_machine_id")?.to_owned(),
            parse_u64(required_field(&fields, "modified_unix_millis")?, "modified_unix_millis")?,
            parse_u64(required_field(&fields, "sequence")?, "sequence")?,
            required_field(&fields, "key_version")?.to_owned(),
        )
    }

}

impl fmt::Debug for EnvRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvRecord")
            .field("name", &self.name)
            .field("scope", &self.scope)
            .field("value", &ENV_REDACTED_VALUE)
            .field("author_machine_id", &self.author_machine_id)
            .field("modified_unix_millis", &self.modified_unix_millis)
            .field("sequence", &self.sequence)
            .field("key_version", &self.key_version)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvConflictSidecar {
    pub path: String,
    pub original_env_name: String,
    pub scope: EnvScope,
    pub loser_machine_id: String,
    pub winner_machine_id: String,
    pub loser_value: RedactedEnvValue,
    pub loser_payload_id: PayloadId,
    pub loser_payload_digest: ContentHash,
    pub manual_escape_hatch: ManualConflictResolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvRecordArtifact {
    payload_id: PayloadId,
    payload_digest: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvStoredRecord {
    record: EnvRecord,
    artifact: Option<EnvRecordArtifact>,
}

impl EnvStoredRecord {
    fn new(record: EnvRecord, artifact: Option<EnvRecordArtifact>) -> Self {
        Self { record, artifact }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EnvRecordKey {
    scope: EnvScope,
    name: String,
}

impl EnvRecordKey {
    fn from_record(record: &EnvRecord) -> Self {
        Self {
            scope: record.scope.clone(),
            name: record.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvState {
    records: BTreeMap<EnvRecordKey, EnvStoredRecord>,
    conflict_sidecars: Vec<EnvConflictSidecar>,
}

impl EnvState {
    pub fn records(&self) -> Vec<&EnvRecord> {
        self.records.values().map(|stored| &stored.record).collect()
    }

    pub fn record(&self, scope: &EnvScope, name: &str) -> Option<&EnvRecord> {
        self.records
            .get(&EnvRecordKey {
                scope: scope.clone(),
                name: name.to_owned(),
            })
            .map(|stored| &stored.record)
    }

    pub fn conflict_sidecars(&self) -> &[EnvConflictSidecar] {
        &self.conflict_sidecars
    }

    pub fn apply_record(&mut self, incoming: EnvRecord) -> EnvResult<EnvApplyOutcome> {
        self.apply_record_with_artifact(incoming, None)
    }

    fn apply_record_with_artifact(
        &mut self,
        incoming: EnvRecord,
        artifact: Option<EnvRecordArtifact>,
    ) -> EnvResult<EnvApplyOutcome> {
        let incoming = EnvStoredRecord::new(incoming, artifact);
        let key = EnvRecordKey::from_record(&incoming.record);
        let Some(existing) = self.records.get(&key).cloned() else {
            self.records.insert(key, incoming);
            return Ok(EnvApplyOutcome {
                applied: true,
                conflict_sidecar: None,
            });
        };

        if existing.record.value == incoming.record.value {
            if record_wins(&incoming.record, &existing.record) {
                self.records.insert(key, incoming);
                return Ok(EnvApplyOutcome {
                    applied: true,
                    conflict_sidecar: None,
                });
            }
            return Ok(EnvApplyOutcome {
                applied: false,
                conflict_sidecar: None,
            });
        }

        let existing_version = file_version_for_env_conflict(&existing)?;
        let incoming_version = file_version_for_env_conflict(&incoming)?;
        let resolution = resolve_same_file_conflict(existing_version.clone(), incoming_version.clone())
            .map_err(|error| EnvSyncError::conflict(error.to_string()))?;
        let incoming_wins = resolution.winner == incoming_version;
        let (winner, loser) = if incoming_wins {
            (incoming, existing)
        } else {
            (existing, incoming)
        };

        let loser_artifact = loser.artifact.as_ref().ok_or_else(|| {
            EnvSyncError::conflict("env conflict sidecar requires losing encrypted payload artifact")
        })?;
        let sidecar = resolution.sidecar.map(|sync_sidecar| EnvConflictSidecar {
            path: sync_sidecar.path,
            original_env_name: winner.record.name.clone(),
            scope: winner.record.scope.clone(),
            loser_machine_id: loser.record.author_machine_id.clone(),
            winner_machine_id: winner.record.author_machine_id.clone(),
            loser_value: loser.record.value.redacted(),
            loser_payload_id: loser_artifact.payload_id.clone(),
            loser_payload_digest: loser_artifact.payload_digest.clone(),
            manual_escape_hatch: sync_sidecar.manual_escape_hatch,
        });
        self.records.insert(key, winner.clone());
        if let Some(sidecar) = sidecar.clone() {
            self.conflict_sidecars.push(sidecar);
        }
        Ok(EnvApplyOutcome {
            applied: incoming_wins,
            conflict_sidecar: sidecar,
        })
    }

    pub fn materialize_for_machine(&self, machine_id: &str) -> EnvResult<EnvMaterialization> {
        validate_machine_id(machine_id, "env materialization machine id")?;
        let mut launcher_environment = BTreeMap::new();
        for stored in self.records.values() {
            let record = &stored.record;
            if record.scope == EnvScope::Shared {
                launcher_environment.insert(
                    record.name.clone(),
                    record.value.expose_for_materialization().to_owned(),
                );
            }
        }
        for stored in self.records.values() {
            let record = &stored.record;
            if record.scope == (EnvScope::MachineOverride {
                machine_id: machine_id.to_owned(),
            }) {
                launcher_environment.insert(
                    record.name.clone(),
                    record.value.expose_for_materialization().to_owned(),
                );
            }
        }
        let session_export = render_session_export(&launcher_environment, false);
        let redacted_session_export = render_session_export(&launcher_environment, true);
        let redacted_log_fields = launcher_environment
            .keys()
            .map(|name| (name.clone(), ENV_REDACTED_VALUE.to_owned()))
            .collect();
        Ok(EnvMaterialization {
            machine_id: machine_id.to_owned(),
            launcher_environment,
            session_export,
            redacted_session_export,
            redacted_log_fields,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvApplyOutcome {
    pub applied: bool,
    pub conflict_sidecar: Option<EnvConflictSidecar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvMaterialization {
    pub machine_id: String,
    pub launcher_environment: BTreeMap<String, String>,
    pub session_export: String,
    pub redacted_session_export: String,
    pub redacted_log_fields: BTreeMap<String, String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvSealedPayload {
    pub format: String,
    pub project_id: ProjectId,
    pub sender_machine_id: String,
    pub target_machine_id: Option<String>,
    pub key_version: String,
    pub key_digest: String,
    pub nonce: [u8; 16],
    pub ciphertext: Vec<u8>,
    pub tag_hex: String,
}

impl EnvSealedPayload {
    pub fn seal<P: EnvKeyProvider>(
        project_id: &str,
        sender_machine_id: &str,
        record: &EnvRecord,
        key_provider: &P,
    ) -> EnvResult<Self> {
        validate_project_id(project_id)?;
        validate_machine_id(sender_machine_id, "env payload sender machine id")?;
        let keyring_machine_id = key_provider.keyring_machine_id();
        validate_machine_id(keyring_machine_id, "env keyring machine id")?;
        if keyring_machine_id != sender_machine_id {
            return Err(EnvSyncError::key_mismatch(format!(
                "env payload sender `{sender_machine_id}` does not match keyring machine `{keyring_machine_id}`"
            )));
        }
        if record.author_machine_id != sender_machine_id {
            return Err(EnvSyncError::integrity(format!(
                "env payload sender `{sender_machine_id}` does not match record author `{}`",
                record.author_machine_id
            )));
        }
        let target_machine_id = match &record.scope {
            EnvScope::Shared => None,
            EnvScope::MachineOverride { machine_id } => Some(machine_id.clone()),
        };
        let payload_key = match target_machine_id.as_deref() {
            Some(target_machine_id) => {
                let secret = key_provider
                    .override_secret_for_target(&record.key_version, target_machine_id)?;
                derive_override_payload_key(
                    &secret,
                    project_id,
                    target_machine_id,
                    &record.key_version,
                )?
            }
            None => {
                let key = key_provider.key_for_version(&record.key_version)?;
                if record.key_version != key.key_version() {
                    return Err(EnvSyncError::key_mismatch(format!(
                        "env record key version `{}` does not match sealing key `{}`",
                        record.key_version,
                        key.key_version()
                    )));
                }
                key
            }
        };
        let plaintext = record.to_wire_bytes(project_id)?;
        let nonce = derive_env_nonce(&payload_key, &plaintext);
        let mut ciphertext = plaintext.clone();
        xor_env_stream(&mut ciphertext, &payload_key, &nonce);
        let key_digest = payload_key.digest_hex();
        let tag_hex = env_payload_tag(
            &payload_key,
            EnvPayloadTagContext {
                project_id,
                sender_machine_id,
                key_version: &record.key_version,
                key_digest: &key_digest,
                target_machine_id: target_machine_id.as_deref(),
                nonce: &nonce,
                ciphertext: &ciphertext,
            },
        );
        Ok(Self {
            format: ENV_PAYLOAD_FORMAT_VERSION.to_owned(),
            project_id: project_id.to_owned(),
            sender_machine_id: sender_machine_id.to_owned(),
            target_machine_id,
            key_version: record.key_version.clone(),
            key_digest,
            nonce,
            ciphertext,
            tag_hex,
        })
    }

    pub fn open<P: EnvKeyProvider>(
        &self,
        key_provider: &P,
        expected_project_id: &str,
        local_machine_id: &str,
    ) -> EnvResult<EnvRecord> {
        if self.format != ENV_PAYLOAD_FORMAT_VERSION {
            return Err(EnvSyncError::decode(format!(
                "unsupported env payload format `{}`",
                self.format
            )));
        }
        validate_project_id(expected_project_id)?;
        if self.project_id != expected_project_id {
            return Err(EnvSyncError::integrity(format!(
                "env payload belongs to project `{}`, not `{expected_project_id}`",
                self.project_id
            )));
        }
        validate_machine_id(local_machine_id, "env payload local machine id")?;
        let keyring_machine_id = key_provider.keyring_machine_id();
        validate_machine_id(keyring_machine_id, "env keyring machine id")?;
        if keyring_machine_id != local_machine_id {
            return Err(EnvSyncError::key_mismatch(format!(
                "env payload local machine `{local_machine_id}` does not match keyring machine `{keyring_machine_id}`"
            )));
        }
        if let Some(target_machine_id) = self.target_machine_id.as_deref() {
            if target_machine_id != local_machine_id {
                return Err(EnvSyncError::integrity(format!(
                    "env override payload target `{target_machine_id}` does not match local machine `{local_machine_id}`"
                )));
            }
        }
        let payload_key = match self.target_machine_id.as_deref() {
            Some(target_machine_id) => {
                let secret = key_provider
                    .override_secret_for_target(&self.key_version, target_machine_id)?;
                derive_override_payload_key(
                    &secret,
                    &self.project_id,
                    target_machine_id,
                    &self.key_version,
                )?
            }
            None => key_provider.key_for_version(&self.key_version)?,
        };
        let actual_digest = payload_key.digest_hex();
        if actual_digest != self.key_digest {
            return Err(EnvSyncError::key_mismatch(format!(
                "env key version `{}` digest mismatch",
                self.key_version
            )));
        }
        let expected_tag = env_payload_tag(
            &payload_key,
            EnvPayloadTagContext {
                project_id: &self.project_id,
                sender_machine_id: &self.sender_machine_id,
                key_version: &self.key_version,
                key_digest: &self.key_digest,
                target_machine_id: self.target_machine_id.as_deref(),
                nonce: &self.nonce,
                ciphertext: &self.ciphertext,
            },
        );
        if !constant_time_eq(expected_tag.as_bytes(), self.tag_hex.as_bytes()) {
            return Err(EnvSyncError::integrity(
                "env payload authentication tag did not verify",
            ));
        }
        let mut plaintext = self.ciphertext.clone();
        xor_env_stream(&mut plaintext, &payload_key, &self.nonce);
        let record = EnvRecord::from_wire_bytes(&plaintext, &self.project_id)?;
        if record.author_machine_id != self.sender_machine_id {
            return Err(EnvSyncError::integrity(format!(
                "env record author `{}` does not match payload sender `{}`",
                record.author_machine_id, self.sender_machine_id
            )));
        }
        if record.key_version != self.key_version {
            return Err(EnvSyncError::integrity(format!(
                "env record key version `{}` does not match payload key version `{}`",
                record.key_version, self.key_version
            )));
        }
        match (&self.target_machine_id, &record.scope) {
            (Some(payload_target), EnvScope::MachineOverride { machine_id })
                if payload_target == machine_id => {}
            (Some(payload_target), EnvScope::MachineOverride { machine_id }) => {
                return Err(EnvSyncError::integrity(format!(
                    "env payload target `{payload_target}` does not match override record target `{machine_id}`"
                )));
            }
            (Some(payload_target), EnvScope::Shared) => {
                return Err(EnvSyncError::integrity(format!(
                    "env payload declares override target `{payload_target}` but record scope is shared"
                )));
            }
            (None, EnvScope::MachineOverride { machine_id }) => {
                return Err(EnvSyncError::integrity(format!(
                    "env override record for `{machine_id}` is missing target payload metadata"
                )));
            }
            (None, EnvScope::Shared) => {}
        }
        Ok(record)
    }

    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let mut output = String::new();
        push_kv(&mut output, "format", &self.format);
        push_kv(&mut output, "project_id", &self.project_id);
        push_kv(&mut output, "sender_machine_id", &self.sender_machine_id);
        if let Some(target_machine_id) = &self.target_machine_id {
            push_kv(&mut output, "target_machine_id", target_machine_id);
        }
        push_kv(&mut output, "key_version", &self.key_version);
        push_kv(&mut output, "key_digest", &self.key_digest);
        push_kv(&mut output, "nonce", &hex_bytes(&self.nonce));
        push_kv(&mut output, "ciphertext", &hex_bytes(&self.ciphertext));
        push_kv(&mut output, "tag", &self.tag_hex);
        output.into_bytes()
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> EnvResult<Self> {
        let fields = parse_kv_lines(bytes)?;
        let format = required_field(&fields, "format")?.to_owned();
        let project_id = required_field(&fields, "project_id")?.to_owned();
        validate_project_id(&project_id)?;
        let sender_machine_id = required_field(&fields, "sender_machine_id")?.to_owned();
        validate_machine_id(&sender_machine_id, "env payload sender machine id")?;
        let target_machine_id = fields
            .get("target_machine_id")
            .map(|machine_id| {
                validate_machine_id(machine_id, "env payload target machine id")?;
                Ok::<String, EnvSyncError>(machine_id.clone())
            })
            .transpose()?;
        let key_version = required_field(&fields, "key_version")?.to_owned();
        validate_key_version(&key_version)?;
        let key_digest = required_field(&fields, "key_digest")?.to_owned();
        let nonce_bytes = decode_hex(required_field(&fields, "nonce")?)?;
        if nonce_bytes.len() != 16 {
            return Err(EnvSyncError::decode("env payload nonce must be 16 bytes"));
        }
        let mut nonce = [0_u8; 16];
        nonce.copy_from_slice(&nonce_bytes);
        let ciphertext = decode_hex(required_field(&fields, "ciphertext")?)?;
        let tag_hex = required_field(&fields, "tag")?.to_owned();
        Ok(Self {
            format,
            project_id,
            sender_machine_id,
            target_machine_id,
            key_version,
            key_digest,
            nonce,
            ciphertext,
            tag_hex,
        })
    }
}

impl fmt::Debug for EnvSealedPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvSealedPayload")
            .field("format", &self.format)
            .field("project_id", &self.project_id)
            .field("sender_machine_id", &self.sender_machine_id)
            .field("target_machine_id", &self.target_machine_id)
            .field("key_version", &self.key_version)
            .field("key_digest", &self.key_digest)
            .field("nonce", &hex_bytes(&self.nonce))
            .field("ciphertext", &"<encrypted>")
            .field("tag_hex", &self.tag_hex)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvAuditOperation {
    KeyVersionSelected,
    PayloadPublished,
    PayloadFetched,
    PayloadDecrypted,
    ValueApplied,
    ConflictSidecarRecorded,
    Materialized,
    DecryptFailed,
}

impl EnvAuditOperation {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::KeyVersionSelected => "key-version-selected",
            Self::PayloadPublished => "payload-published",
            Self::PayloadFetched => "payload-fetched",
            Self::PayloadDecrypted => "payload-decrypted",
            Self::ValueApplied => "value-applied",
            Self::ConflictSidecarRecorded => "conflict-sidecar-recorded",
            Self::Materialized => "materialized",
            Self::DecryptFailed => "decrypt-failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvAuditStatus {
    Succeeded,
    Failed,
}

impl EnvAuditStatus {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvAuditEntry {
    pub ordinal: u64,
    pub event_unix_millis: u64,
    pub operation: EnvAuditOperation,
    pub actor_machine_id: String,
    pub env_name: Option<String>,
    pub scope: Option<EnvScope>,
    pub payload_id: Option<PayloadId>,
    pub key_version: Option<String>,
    pub status: EnvAuditStatus,
    pub redacted_value: RedactedEnvValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvAuditLog {
    entries: Vec<EnvAuditEntry>,
    next_ordinal: u64,
}

impl EnvAuditLog {
    pub fn entries(&self) -> &[EnvAuditEntry] {
        &self.entries
    }

    fn record(&mut self, draft: EnvAuditDraft) {
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        self.entries.push(EnvAuditEntry {
            ordinal,
            event_unix_millis: draft.event_unix_millis,
            operation: draft.operation,
            actor_machine_id: draft.actor_machine_id,
            env_name: draft.env_name,
            scope: draft.scope,
            payload_id: draft.payload_id,
            key_version: draft.key_version,
            status: draft.status,
            redacted_value: RedactedEnvValue,
        });
    }

    pub fn to_redacted_lines(&self) -> String {
        let mut output = String::new();
        push_kv(&mut output, "format", ENV_AUDIT_FORMAT_VERSION);
        for entry in &self.entries {
            let mut line = String::new();
            push_inline_kv(&mut line, "ordinal", &entry.ordinal.to_string());
            push_inline_kv(
                &mut line,
                "event_unix_millis",
                &entry.event_unix_millis.to_string(),
            );
            push_inline_kv(&mut line, "operation", entry.operation.as_wire());
            push_inline_kv(&mut line, "actor_machine_id", &entry.actor_machine_id);
            if let Some(env_name) = &entry.env_name {
                push_inline_kv(&mut line, "env_name", env_name);
            }
            if let Some(scope) = &entry.scope {
                push_inline_kv(&mut line, "scope", &scope.as_wire());
            }
            if let Some(payload_id) = &entry.payload_id {
                push_inline_kv(&mut line, "payload_id", payload_id);
            }
            if let Some(key_version) = &entry.key_version {
                push_inline_kv(&mut line, "key_version", key_version);
            }
            push_inline_kv(&mut line, "status", entry.status.as_wire());
            push_inline_kv(&mut line, "value", entry.redacted_value.as_str());
            output.push_str("event=");
            output.push_str(&encode_field(&line));
            output.push('\n');
        }
        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvAuditDraft {
    operation: EnvAuditOperation,
    event_unix_millis: u64,
    actor_machine_id: String,
    env_name: Option<String>,
    scope: Option<EnvScope>,
    payload_id: Option<PayloadId>,
    key_version: Option<String>,
    status: EnvAuditStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPublishReport {
    pub payload_id: PayloadId,
    pub operation_id: String,
    pub key_version: String,
    pub env_name: String,
    pub scope: EnvScope,
    pub redacted_value: RedactedEnvValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvIngestReport {
    pub fetched_payloads: usize,
    pub applied_records: usize,
    pub conflict_sidecars: usize,
}

pub struct EnvReplica<P: EnvKeyProvider> {
    project_id: ProjectId,
    machine_id: String,
    key_provider: P,
    state: EnvState,
    audit_log: EnvAuditLog,
    applied_payload_ids: BTreeSet<PayloadId>,
    never_sync_policy: EnvNeverSyncPolicy,
    next_sequence: u64,
}


impl<P: EnvKeyProvider> EnvReplica<P> {
    pub fn new(
        project_id: impl Into<ProjectId>,
        machine_id: impl Into<String>,
        key_provider: P,
    ) -> EnvResult<Self> {
        Self::new_with_never_sync_policy(
            project_id,
            machine_id,
            key_provider,
            EnvNeverSyncPolicy::allow_all(),
        )
    }

    pub fn new_with_never_sync_policy(
        project_id: impl Into<ProjectId>,
        machine_id: impl Into<String>,
        key_provider: P,
        never_sync_policy: EnvNeverSyncPolicy,
    ) -> EnvResult<Self> {
        let project_id = project_id.into();
        validate_project_id(&project_id)?;
        let machine_id = machine_id.into();
        validate_machine_id(&machine_id, "env replica machine id")?;
        let keyring_machine_id = key_provider.keyring_machine_id();
        validate_machine_id(keyring_machine_id, "env keyring machine id")?;
        if keyring_machine_id != machine_id {
            return Err(EnvSyncError::key_mismatch(format!(
                "env replica machine `{machine_id}` does not match keyring machine `{keyring_machine_id}`"
            )));
        }
        Ok(Self {
            project_id,
            machine_id,
            key_provider,
            state: EnvState::default(),
            audit_log: EnvAuditLog::default(),
            applied_payload_ids: BTreeSet::new(),
            never_sync_policy,
            next_sequence: 1,
        })
    }


    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn state(&self) -> &EnvState {
        &self.state
    }

    pub fn audit_log(&self) -> &EnvAuditLog {
        &self.audit_log
    }

    pub fn key_provider(&self) -> &P {
        &self.key_provider
    }

    pub fn set_shared(
        &mut self,
        transport: &FileBackedSyncStore,
        name: impl Into<String>,
        value: impl Into<String>,
        modified_unix_millis: u64,
    ) -> EnvResult<EnvPublishReport> {
        let name = name.into();
        validate_env_name(&name)?;
        let scope = EnvScope::Shared;
        self.validate_never_sync_publish(&scope, &name)?;
        let key_version = self.select_active_key_version(modified_unix_millis)?;
        let author_machine_id = self.machine_id.clone();
        let sequence = self.take_next_sequence();
        let record = EnvRecord::new(
            name,
            scope,
            value,
            author_machine_id,
            modified_unix_millis,
            sequence,
            key_version,
        )?;
        self.publish_record(transport, record)
    }


    pub fn set_override_for_machine(
        &mut self,
        transport: &FileBackedSyncStore,
        target_machine_id: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
        modified_unix_millis: u64,
    ) -> EnvResult<EnvPublishReport> {
        let scope = EnvScope::machine_override(target_machine_id)?;
        let name = name.into();
        validate_env_name(&name)?;
        self.validate_never_sync_publish(&scope, &name)?;
        let key_version = self.select_active_key_version(modified_unix_millis)?;
        let author_machine_id = self.machine_id.clone();
        let sequence = self.take_next_sequence();
        let record = EnvRecord::new(
            name,
            scope,
            value,
            author_machine_id,
            modified_unix_millis,
            sequence,
            key_version,
        )?;
        self.publish_record(transport, record)
    }


    pub fn ingest_from_transport(
        &mut self,
        transport: &FileBackedSyncStore,
    ) -> EnvResult<EnvIngestReport> {
        self.validate_transport(transport)?;
        let operations = transport.load_operation_log()?;
        let mut report = EnvIngestReport::default();
        for operation in operations {
            if !is_env_operation(&operation) {
                continue;
            }
            let payload_id = operation.payload_id.clone().ok_or_else(|| {
                EnvSyncError::integrity(format!(
                    "env operation `{}` has no payload id",
                    operation.id
                ))
            })?;
            if self.applied_payload_ids.contains(&payload_id) {
                continue;
            }
            if operation_targets_other_machine_override(&operation.path, &self.machine_id) {
                self.applied_payload_ids.insert(payload_id);
                continue;
            }
            let payload_bytes = transport.fetch_generic_payload(ENV_SYNC_PAYLOAD_TYPE, &payload_id)?;
            report.fetched_payloads += 1;
            self.audit(EnvAuditDraft {
                operation: EnvAuditOperation::PayloadFetched,
                event_unix_millis: operation.modified_unix_millis,
                actor_machine_id: self.machine_id.clone(),
                env_name: None,
                scope: None,
                payload_id: Some(payload_id.clone()),
                key_version: None,
                status: EnvAuditStatus::Succeeded,
            });
            let sealed = EnvSealedPayload::from_wire_bytes(&payload_bytes)?;
            let payload_digest = env_payload_digest(&payload_bytes);
            let record = match sealed.open(
                &self.key_provider,
                &self.project_id,
                &self.machine_id,
            ) {
                Ok(record) => record,
                Err(error) => {
                    self.audit(EnvAuditDraft {
                        operation: EnvAuditOperation::DecryptFailed,
                        event_unix_millis: operation.modified_unix_millis,
                        actor_machine_id: self.machine_id.clone(),
                        env_name: None,
                        scope: None,
                        payload_id: Some(payload_id),
                        key_version: Some(sealed.key_version),
                        status: EnvAuditStatus::Failed,
                    });
                    return Err(error);
                }
            };
            self.audit(EnvAuditDraft {
                operation: EnvAuditOperation::PayloadDecrypted,
                event_unix_millis: operation.modified_unix_millis,
                actor_machine_id: self.machine_id.clone(),
                env_name: Some(record.name.clone()),
                scope: Some(record.scope.clone()),
                payload_id: Some(payload_id.clone()),
                key_version: Some(record.key_version.clone()),
                status: EnvAuditStatus::Succeeded,
            });
            self.validate_operation_matches_record(&operation, &payload_id, &record)?;
            if !record.scope.applies_to_machine(&self.machine_id) {
                self.applied_payload_ids.insert(payload_id);
                continue;
            }
            let outcome = self.apply_record_with_audit(
                record,
                Some(EnvRecordArtifact {
                    payload_id: payload_id.clone(),
                    payload_digest,
                }),
            )?;
            if outcome.applied {
                report.applied_records += 1;
            }
            if outcome.conflict_sidecar.is_some() {
                report.conflict_sidecars += 1;
            }
            self.applied_payload_ids.insert(payload_id);
        }
        Ok(report)
    }

    pub fn materialize(&mut self) -> EnvResult<EnvMaterialization> {
        let machine_id = self.machine_id.clone();
        self.materialize_for_machine(&machine_id)
    }

    pub fn materialize_for_machine(&mut self, machine_id: &str) -> EnvResult<EnvMaterialization> {
        let materialization = self.state.materialize_for_machine(machine_id)?;
        self.audit(EnvAuditDraft {
            operation: EnvAuditOperation::Materialized,
            event_unix_millis: current_unix_millis(),
            actor_machine_id: self.machine_id.clone(),
            env_name: None,
            scope: None,
            payload_id: None,
            key_version: None,
            status: EnvAuditStatus::Succeeded,
        });
        Ok(materialization)
    }

    fn select_active_key_version(&mut self, event_unix_millis: u64) -> EnvResult<String> {
        let record = self.key_provider.latest_key_version()?;
        self.audit(EnvAuditDraft {
            operation: EnvAuditOperation::KeyVersionSelected,
            event_unix_millis,
            actor_machine_id: self.machine_id.clone(),
            env_name: None,
            scope: None,
            payload_id: None,
            key_version: Some(record.key_version.clone()),
            status: EnvAuditStatus::Succeeded,
        });
        Ok(record.key_version)
    }

    fn publish_record(
        &mut self,
        transport: &FileBackedSyncStore,
        record: EnvRecord,
    ) -> EnvResult<EnvPublishReport> {
        self.validate_never_sync_publish(&record.scope, &record.name)?;

        self.validate_transport(transport)?;
        let sealed =
            EnvSealedPayload::seal(&self.project_id, &self.machine_id, &record, &self.key_provider)?;
        let payload_bytes = sealed.to_wire_bytes();
        let payload_digest = env_payload_digest(&payload_bytes);
        assert_no_plaintext_in_env_artifact(&payload_bytes, record.value.expose_for_materialization())?;
        let payload_id = transport.put_generic_payload(ENV_SYNC_PAYLOAD_TYPE, &payload_bytes)?;
        let operation = OperationRecord::from_draft(
            OperationDraft::new(
                record.sequence,
                self.project_id.clone(),
                self.machine_id.clone(),
                OperationKind::GenericPayload,
                record.operation_path(),
            )
            .payload_id(payload_id.clone())
            .content_hash(payload_digest.clone())
            .modified_unix_millis(record.modified_unix_millis),
        );
        let operation_id = transport.append_operation(&operation)?;
        self.audit(EnvAuditDraft {
            operation: EnvAuditOperation::PayloadPublished,
            event_unix_millis: record.modified_unix_millis,
            actor_machine_id: self.machine_id.clone(),
            env_name: Some(record.name.clone()),
            scope: Some(record.scope.clone()),
            payload_id: Some(payload_id.clone()),
            key_version: Some(record.key_version.clone()),
            status: EnvAuditStatus::Succeeded,
        });
        if record.scope.applies_to_machine(&self.machine_id) {
            self.apply_record_with_audit(
                record.clone(),
                Some(EnvRecordArtifact {
                    payload_id: payload_id.clone(),
                    payload_digest,
                }),
            )?;
        }
        self.applied_payload_ids.insert(payload_id.clone());
        Ok(EnvPublishReport {
            payload_id,
            operation_id,
            key_version: record.key_version,
            env_name: record.name,
            scope: record.scope,
            redacted_value: RedactedEnvValue,
        })
    }

    fn apply_record_with_audit(
        &mut self,
        record: EnvRecord,
        artifact: Option<EnvRecordArtifact>,
    ) -> EnvResult<EnvApplyOutcome> {
        let env_name = record.name.clone();
        let scope = record.scope.clone();
        let key_version = record.key_version.clone();
        let event_unix_millis = record.modified_unix_millis;
        let payload_id = artifact.as_ref().map(|artifact| artifact.payload_id.clone());
        let outcome = self.state.apply_record_with_artifact(record, artifact)?;
        self.audit(EnvAuditDraft {
            operation: EnvAuditOperation::ValueApplied,
            event_unix_millis,
            actor_machine_id: self.machine_id.clone(),
            env_name: Some(env_name.clone()),
            scope: Some(scope.clone()),
            payload_id: payload_id.clone(),
            key_version: Some(key_version.clone()),
            status: EnvAuditStatus::Succeeded,
        });
        if outcome.conflict_sidecar.is_some() {
            self.audit(EnvAuditDraft {
                operation: EnvAuditOperation::ConflictSidecarRecorded,
                event_unix_millis,
                actor_machine_id: self.machine_id.clone(),
                env_name: Some(env_name),
                scope: Some(scope),
                payload_id,
                key_version: Some(key_version),
                status: EnvAuditStatus::Succeeded,
            });
        }
        Ok(outcome)
    }

    fn never_sync_policy(&self) -> &EnvNeverSyncPolicy {
        &self.never_sync_policy
    }

    fn validate_never_sync_publish(&self, scope: &EnvScope, name: &str) -> EnvResult<()> {
        self.never_sync_policy().validate_publish(scope, name)
    }

    fn validate_transport(&self, transport: &FileBackedSyncStore) -> EnvResult<()> {
        if transport.project_id() != self.project_id.as_str() {
            return Err(EnvSyncError::integrity(format!(
                "env replica project `{}` does not match sync transport project `{}`",
                self.project_id,
                transport.project_id()
            )));
        }
        if transport.local_machine_id() != self.machine_id.as_str() {
            return Err(EnvSyncError::integrity(format!(
                "env replica machine `{}` does not match sync transport machine `{}`",
                self.machine_id,
                transport.local_machine_id()
            )));
        }
        Ok(())
    }

    fn validate_operation_matches_record(
        &self,
        operation: &OperationRecord,
        payload_id: &str,
        record: &EnvRecord,
    ) -> EnvResult<()> {
        if operation.project_id != self.project_id {
            return Err(EnvSyncError::integrity(format!(
                "env operation `{}` belongs to project `{}`, not `{}`",
                operation.id, operation.project_id, self.project_id
            )));
        }
        if operation.machine_id != record.author_machine_id {
            return Err(EnvSyncError::integrity(format!(
                "env operation `{}` machine `{}` does not match env author `{}`",
                operation.id, operation.machine_id, record.author_machine_id
            )));
        }
        if operation.path != record.operation_path() {
            return Err(EnvSyncError::integrity(format!(
                "env operation `{}` path `{}` does not match env record path `{}`",
                operation.id,
                operation.path,
                record.operation_path()
            )));
        }
        if operation.payload_id.as_deref() != Some(payload_id) {
            return Err(EnvSyncError::integrity(format!(
                "env operation `{}` payload id mismatch",
                operation.id
            )));
        }
        Ok(())
    }

    fn take_next_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        sequence
    }

    fn audit(&mut self, draft: EnvAuditDraft) {
        self.audit_log.record(draft);
    }
}

impl<P: EnvKeyProvider> fmt::Debug for EnvReplica<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvReplica")
            .field("project_id", &self.project_id)
            .field("machine_id", &self.machine_id)
            .field("key_version_records", &self.key_provider.key_version_records())
            .field("state", &self.state)
            .field("audit_log", &self.audit_log)
            .field("applied_payload_ids", &self.applied_payload_ids)
            .field("never_sync_policy", &self.never_sync_policy)

            .field("next_sequence", &self.next_sequence)
            .finish()
    }
}

pub fn env_initial_migration() -> Migration {
    Migration::new(
        ENV_MIGRATION_VERSION,
        ENV_MIGRATION_DESCRIPTION,
        ENV_MIGRATION_UP_SQL,
        ENV_MIGRATION_DOWN_SQL,
        ENV_MIGRATION_TABLES,
    )
}

pub fn env_migration_runner() -> Result<MigrationRunner, MigrationError> {
    MigrationRunner::with_migrations([env_initial_migration()])
}

pub fn assert_no_plaintext_in_env_artifact(artifact: &[u8], plaintext: &str) -> EnvResult<()> {
    if plaintext.is_empty() {
        return Ok(());
    }
    if let Ok(fields) = parse_kv_lines(artifact) {
        if let Some(ciphertext_hex) = fields.get("ciphertext") {
            let ciphertext = decode_hex(ciphertext_hex)?;
            return assert_plaintext_absent_from_bytes(&ciphertext, plaintext);
        }
        return Ok(());
    }
    assert_plaintext_absent_from_bytes(artifact, plaintext)
}

fn assert_plaintext_absent_from_bytes(bytes: &[u8], plaintext: &str) -> EnvResult<()> {
    if bytes
        .windows(plaintext.len())
        .any(|window| window == plaintext.as_bytes())
    {
        return Err(EnvSyncError::integrity(
            "env plaintext appeared in encrypted artifact body",
        ));
    }
    Ok(())
}

fn is_env_operation(operation: &OperationRecord) -> bool {
    operation.kind == OperationKind::GenericPayload
        && operation.path.starts_with(ENV_OPERATION_PATH_PREFIX)
}

fn env_operation_path(scope: &EnvScope, name: &str) -> String {
    format!(
        "{ENV_OPERATION_PATH_PREFIX}{}/{}",
        scope.path_component(),
        safe_path_component(name)
    )
}

fn file_version_for_env_conflict(stored: &EnvStoredRecord) -> EnvResult<FileVersion> {
    let artifact = stored.artifact.as_ref().ok_or_else(|| {
        EnvSyncError::conflict("env conflict sidecar requires encrypted payload artifact")
    })?;
    Ok(FileVersion::from_known_content_hash(
        stored.record.operation_path(),
        stored.record.author_machine_id.clone(),
        stored.record.modified_unix_millis,
        artifact.payload_digest.clone(),
        Vec::new(),
    ))
}

fn operation_targets_other_machine_override(operation_path: &str, local_machine_id: &str) -> bool {
    let Some(rest) = operation_path.strip_prefix(ENV_OVERRIDE_OPERATION_PATH_PREFIX) else {
        return false;
    };
    let Some((target_machine_component, _env_name_component)) = rest.split_once('/') else {
        return false;
    };
    target_machine_component != safe_path_component(local_machine_id)
}

fn env_payload_digest(payload_bytes: &[u8]) -> ContentHash {
    env_digest_hex(b"env:sealed-payload:v1", &[payload_bytes])
}

fn current_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn record_wins(left: &EnvRecord, right: &EnvRecord) -> bool {
    (left.modified_unix_millis, left.sequence, left.author_machine_id.as_str())
        > (
            right.modified_unix_millis,
            right.sequence,
            right.author_machine_id.as_str(),
        )
}

fn render_session_export(values: &BTreeMap<String, String>, redacted: bool) -> String {
    let mut output = String::new();
    output.push_str("# ");
    output.push_str(ENV_SESSION_EXPORT_FORMAT_VERSION);
    output.push('\n');
    for (name, value) in values {
        output.push_str("export ");
        output.push_str(name);
        output.push('=');
        if redacted {
            output.push_str(ENV_REDACTED_VALUE);
        } else {
            output.push_str(&shell_single_quote(value));
        }
        output.push('\n');
    }
    output
}

fn shell_single_quote(value: &str) -> String {
    let mut output = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            output.push_str("'\\''");
        } else {
            output.push(ch);
        }
    }
    output.push('\'');
    output
}

fn validate_project_id(project_id: &str) -> EnvResult<()> {
    if project_id.trim().is_empty() {
        return Err(EnvSyncError::invalid_config(
            "env project id must not be empty",
        ));
    }
    Ok(())
}

fn validate_machine_id(machine_id: &str, label: &str) -> EnvResult<()> {
    if MachineId::is_app_scoped_value(machine_id) {
        Ok(())
    } else {
        Err(EnvSyncError::invalid_config(format!(
            "{label} `{machine_id}` is not app-scoped"
        )))
    }
}

fn validate_env_name(name: &str) -> EnvResult<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Err(EnvSyncError::invalid_config(
            "env variable name must not be empty",
        ));
    }
    let first = bytes[0];
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return Err(EnvSyncError::invalid_config(format!(
            "env variable name `{name}` must start with a letter or underscore"
        )));
    }
    if !bytes
        .iter()
        .all(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return Err(EnvSyncError::invalid_config(format!(
            "env variable name `{name}` contains unsupported characters"
        )));
    }
    Ok(())
}

fn validate_env_name_fragment(fragment: &str, label: &str) -> EnvResult<()> {
    if fragment.is_empty() {
        return Err(EnvSyncError::invalid_config(format!(
            "{label} must not be empty"
        )));
    }
    if !fragment
        .bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return Err(EnvSyncError::invalid_config(format!(
            "{label} `{fragment}` contains unsupported characters"
        )));
    }
    Ok(())
}

fn validate_key_version(key_version: &str) -> EnvResult<()> {
    if key_version.trim().is_empty() {
        return Err(EnvSyncError::invalid_config(
            "env key version must not be empty",
        ));
    }
    if !key_version.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return Err(EnvSyncError::invalid_config(format!(
            "env key version `{key_version}` contains unsupported characters"
        )));
    }
    Ok(())
}

fn push_kv(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push('=');
    output.push_str(&encode_field(value));
    output.push('\n');
}

fn push_inline_kv(output: &mut String, key: &str, value: &str) {
    if !output.is_empty() {
        output.push(' ');
    }
    output.push_str(key);
    output.push('=');
    output.push_str(&encode_field(value));
}

fn parse_kv_lines(bytes: &[u8]) -> EnvResult<BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| EnvSyncError::decode(format!("env payload is not UTF-8: {error}")))?;
    let mut fields = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            EnvSyncError::decode(format!("env payload line {} is missing `=`", index + 1))
        })?;
        if key.is_empty() {
            return Err(EnvSyncError::decode(format!(
                "env payload line {} has empty key",
                index + 1
            )));
        }
        if fields
            .insert(key.to_owned(), decode_field(value)?)
            .is_some()
        {
            return Err(EnvSyncError::decode(format!(
                "env payload contains duplicate key `{key}`"
            )));
        }
    }
    Ok(fields)
}

fn required_field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> EnvResult<&'a str> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| EnvSyncError::decode(format!("env payload missing `{key}`")))
}

fn parse_u64(value: &str, label: &str) -> EnvResult<u64> {
    value
        .parse::<u64>()
        .map_err(|error| EnvSyncError::decode(format!("invalid {label} `{value}`: {error}")))
}

fn encode_field(value: &str) -> String {
    let mut output = String::new();
    for &byte in value.as_bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' | b'/' | b'<' | b'>') {
            output.push(byte as char);
        } else {
            push_percent_encoded_byte(&mut output, byte);
        }
    }
    output
}

fn decode_field(value: &str) -> EnvResult<String> {
    let mut bytes = Vec::new();
    let raw = value.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            if index + 2 >= raw.len() {
                return Err(EnvSyncError::decode(
                    "env percent escape ended before two hex digits",
                ));
            }
            let high = hex_value(raw[index + 1])?;
            let low = hex_value(raw[index + 2])?;
            bytes.push((high << 4) | low);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes)
        .map_err(|error| EnvSyncError::decode(format!("env field is not UTF-8: {error}")))
}

fn push_percent_encoded_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0F) as usize] as char);
}

fn safe_path_component(value: &str) -> String {
    if value.is_empty() {
        return "_".to_owned();
    }
    let mut output = String::new();
    for &byte in value.as_bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.') {
            output.push(byte as char);
        } else {
            output.push('_');
            let _ = write!(output, "{byte:02x}");
        }
    }
    output
}

fn derive_env_nonce(key: &EnvMasterKey, plaintext: &[u8]) -> [u8; 16] {
    let digest = env_digest_bytes(b"env:nonce:v1", &[key.material(), plaintext]);
    let mut nonce = [0_u8; 16];
    nonce.copy_from_slice(&digest[..16]);
    nonce
}

fn derive_override_payload_key(
    secret: &EnvMachineOverrideSecret,
    project_id: &str,
    target_machine_id: &str,
    key_version: &str,
) -> EnvResult<EnvMasterKey> {
    validate_project_id(project_id)?;
    validate_machine_id(target_machine_id, "env override target machine id")?;
    validate_key_version(key_version)?;
    if secret.machine_id() != target_machine_id {
        return Err(EnvSyncError::key_mismatch(format!(
            "env override secret machine `{}` does not match payload target `{target_machine_id}`",
            secret.machine_id()
        )));
    }
    if secret.key_version() != key_version {
        return Err(EnvSyncError::key_mismatch(format!(
            "env override secret key version `{}` does not match payload key version `{key_version}`",
            secret.key_version()
        )));
    }
    let material = env_digest_bytes(
        b"env:override-target-key:v2",
        &[
            secret.material(),
            project_id.as_bytes(),
            target_machine_id.as_bytes(),
            key_version.as_bytes(),
        ],
    );
    EnvMasterKey::new(key_version.to_owned(), material)
}

struct EnvPayloadTagContext<'a> {
    project_id: &'a str,
    sender_machine_id: &'a str,
    key_version: &'a str,
    key_digest: &'a str,
    target_machine_id: Option<&'a str>,
    nonce: &'a [u8; 16],
    ciphertext: &'a [u8],
}

fn env_payload_tag(key: &EnvMasterKey, context: EnvPayloadTagContext<'_>) -> String {
    let target_scope: &[u8] = if context.target_machine_id.is_some() {
        b"machine-override"
    } else {
        b"shared"
    };
    let target_machine_id = context.target_machine_id.unwrap_or("");
    env_digest_hex(
        b"env:payload-tag:v1",
        &[
            key.material(),
            context.project_id.as_bytes(),
            context.sender_machine_id.as_bytes(),
            context.key_version.as_bytes(),
            context.key_digest.as_bytes(),
            target_scope,
            target_machine_id.as_bytes(),
            context.nonce,
            context.ciphertext,
        ],
    )
}

fn xor_env_stream(bytes: &mut [u8], key: &EnvMasterKey, nonce: &[u8; 16]) {
    let mut offset = 0;
    let mut counter = 0_u64;
    while offset < bytes.len() {
        let counter_bytes = counter.to_le_bytes();
        let block = env_digest_bytes(
            b"env:stream:v1",
            &[key.material(), nonce, &counter_bytes],
        );
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

fn env_digest_hex(domain: &[u8], parts: &[&[u8]]) -> String {
    hex_bytes(&env_digest_bytes(domain, parts))
}

fn env_digest_bytes(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut output = [0_u8; 32];
    for block in 0..4_u64 {
        let mut hash = FNV_OFFSET ^ block.wrapping_mul(0x9e3779b97f4a7c15);
        for byte in domain {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
        for part in parts {
            for byte in (part.len() as u64).to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            for byte in *part {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash ^= 0xfe;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        output[(block as usize) * 8..(block as usize + 1) * 8]
            .copy_from_slice(&hash.to_be_bytes());
    }
    output
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
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn decode_hex(value: &str) -> EnvResult<Vec<u8>> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(EnvSyncError::decode("hex value has odd length"));
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

fn hex_value(byte: u8) -> EnvResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(EnvSyncError::decode(format!(
            "invalid hex byte `{}`",
            byte as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::{InMemoryMigrationStore, BASELINE_SCHEMA_VERSION};
    use crate::sync::{
        app_scoped_machine_id, EndpointSecurityConfig, FileBackedSyncStore, SharedSecret,
        TransportMode,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn syncs_env_from_machine_a_to_machine_b() {
        let fixture = Fixture::new(&["machine-a", "machine-b"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let store_a = fixture.store(&machine_a);
        let store_b = fixture.store(&machine_b);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let provider_b = provider(&machine_b, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a.clone(), provider_a).unwrap();
        let mut replica_b = EnvReplica::new("project", machine_b.clone(), provider_b).unwrap();

        replica_a
            .set_shared(&store_a, "API_TOKEN", "secret-from-a", 1_000)
            .unwrap();
        let report = replica_b.ingest_from_transport(&store_b).unwrap();
        let materialized = replica_b.materialize().unwrap();

        assert_eq!(report.fetched_payloads, 1);
        assert_eq!(report.applied_records, 1);
        assert_eq!(
            materialized.launcher_environment.get("API_TOKEN"),
            Some(&"secret-from-a".to_owned())
        );
        assert!(materialized.session_export.contains("export API_TOKEN='secret-from-a'"));
        assert!(materialized.redacted_session_export.contains(ENV_REDACTED_VALUE));
    }

    #[test]
    fn cross_project_copied_env_payload_is_rejected_before_apply() {
        let fixture = Fixture::new(&["machine-a"]);
        let machine_a = fixture.machine("machine-a");
        let store_a = fixture.store(&machine_a);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let project_b_record = EnvRecord::shared(
            "CROSS_PROJECT_SECRET",
            "project-b-secret",
            machine_a.clone(),
            1_000,
            1,
            "v1",
        )
        .unwrap();
        let project_b_payload =
            EnvSealedPayload::seal("project-b", &machine_a, &project_b_record, &provider_a)
                .unwrap();
        let project_b_payload_bytes = project_b_payload.to_wire_bytes();
        let payload_id = store_a
            .put_generic_payload(ENV_SYNC_PAYLOAD_TYPE, &project_b_payload_bytes)
            .unwrap();
        let project_a_operation = OperationRecord::from_draft(
            OperationDraft::new(
                1,
                "project",
                machine_a.clone(),
                OperationKind::GenericPayload,
                project_b_record.operation_path(),
            )
            .payload_id(payload_id.clone())
            .content_hash(env_payload_digest(&project_b_payload_bytes))
            .modified_unix_millis(project_b_record.modified_unix_millis),
        );
        store_a.append_operation(&project_a_operation).unwrap();
        let mut replica_a = EnvReplica::new("project", machine_a, provider_a).unwrap();

        let error = replica_a.ingest_from_transport(&store_a).unwrap_err();
        let materialized = replica_a
            .state()
            .materialize_for_machine(replica_a.machine_id())
            .unwrap();
        let audit_entries = replica_a.audit_log().entries();

        assert!(matches!(
            &error,
            EnvSyncError::Integrity(message)
                if message.contains("project `project-b`, not `project`")
        ));
        assert!(replica_a
            .state()
            .record(&EnvScope::shared(), "CROSS_PROJECT_SECRET")
            .is_none());
        assert!(!materialized
            .launcher_environment
            .contains_key("CROSS_PROJECT_SECRET"));
        assert!(audit_entries.iter().any(|entry| {
            entry.operation == EnvAuditOperation::DecryptFailed
                && entry.payload_id.as_deref() == Some(payload_id.as_str())
                && entry.status == EnvAuditStatus::Failed
        }));
        assert!(!audit_entries.iter().any(|entry| {
            matches!(
                entry.operation,
                EnvAuditOperation::PayloadDecrypted | EnvAuditOperation::ValueApplied
            ) && entry.payload_id.as_deref() == Some(payload_id.as_str())
                && entry.status == EnvAuditStatus::Succeeded
        }));
        assert!(!audit_entries.iter().any(|entry| {
            entry.operation == EnvAuditOperation::Materialized
                && entry.status == EnvAuditStatus::Succeeded
        }));
    }

    #[test]
    fn replica_rejects_key_provider_for_different_machine() {
        let fixture = Fixture::new(&["machine-a", "machine-b"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let wrong_provider = provider(&machine_b, b"env-key-v1");

        let error = EnvReplica::new("project", machine_a, wrong_provider).unwrap_err();

        assert!(matches!(&error, EnvSyncError::KeyMismatch(_)));
        assert!(format!("{error}").contains("does not match keyring machine"));
    }

    #[test]
    fn never_sync_policy_rejects_shared_and_overrides_before_transport_or_audit() {
        let fixture = Fixture::new(&["machine-a"]);
        let machine_a = fixture.machine("machine-a");
        let store_a = fixture.store(&machine_a);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let mut policy = EnvNeverSyncPolicy::deny_names(["LOCAL_ONLY_TOKEN"]).unwrap();
        policy.deny_suffix("_LOCAL").unwrap();
        let mut replica =
            EnvReplica::new_with_never_sync_policy("project", machine_a.clone(), provider_a, policy)
                .unwrap();

        let shared_error = replica
            .set_shared(
                &store_a,
                "LOCAL_ONLY_TOKEN",
                "shared-never-sync-secret",
                1_000,
            )
            .unwrap_err();
        let override_error = replica
            .set_override_for_machine(
                &store_a,
                machine_a,
                "DATABASE_URL_LOCAL",
                "override-never-sync-secret",
                2_000,
            )
            .unwrap_err();

        assert!(matches!(
            &shared_error,
            EnvSyncError::InvalidConfig(message) if message.contains("never-sync")
        ));
        assert!(matches!(
            &override_error,
            EnvSyncError::InvalidConfig(message) if message.contains("never-sync")
        ));
        assert!(store_a.load_operation_log().unwrap().is_empty());
        assert!(replica.audit_log().entries().is_empty());
        let diagnostics = format!("{shared_error:?}{override_error:?}{replica:?}");
        assert!(!diagnostics.contains("shared-never-sync-secret"));
        assert!(!diagnostics.contains("override-never-sync-secret"));
    }

    #[test]
    fn machine_overrides_are_honored_during_materialization() {
        let fixture = Fixture::new(&["machine-a", "machine-b"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let store_a = fixture.store(&machine_a);
        let store_b = fixture.store(&machine_b);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let provider_b = provider(&machine_b, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a.clone(), provider_a).unwrap();
        let mut replica_b = EnvReplica::new("project", machine_b.clone(), provider_b).unwrap();

        replica_a
            .set_shared(&store_a, "DATABASE_URL", "postgres://shared", 1_000)
            .unwrap();
        let override_report = replica_b
            .set_override_for_machine(
                &store_b,
                machine_b.clone(),
                "DATABASE_URL",
                "postgres://machine-b",
                2_000,
            )
            .unwrap();
        let override_payload_bytes = store_b
            .fetch_generic_payload(ENV_SYNC_PAYLOAD_TYPE, &override_report.payload_id)
            .unwrap();
        let override_payload = EnvSealedPayload::from_wire_bytes(&override_payload_bytes).unwrap();
        assert_eq!(
            override_payload.target_machine_id.as_deref(),
            Some(machine_b.as_str())
        );
        let opened_override = override_payload
            .open(replica_b.key_provider(), replica_b.project_id(), &machine_b)
            .unwrap();
        assert_eq!(
            opened_override.value.expose_for_materialization(),
            "postgres://machine-b"
        );
        replica_b.ingest_from_transport(&store_b).unwrap();

        let materialized_for_b = replica_b.materialize().unwrap();
        let materialized_for_a = replica_b.materialize_for_machine(&machine_a).unwrap();
        assert_eq!(
            materialized_for_b.launcher_environment.get("DATABASE_URL"),
            Some(&"postgres://machine-b".to_owned())
        );
        assert_eq!(
            materialized_for_a.launcher_environment.get("DATABASE_URL"),
            Some(&"postgres://shared".to_owned())
        );
        assert_eq!(
            materialized_for_b.redacted_log_fields.get("DATABASE_URL"),
            Some(&ENV_REDACTED_VALUE.to_owned())
        );
    }

    #[test]
    fn non_target_machine_overrides_are_not_fetched_or_stored() {
        let fixture = Fixture::new(&["machine-a", "machine-b", "machine-c"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let machine_c = fixture.machine("machine-c");
        let store_a = fixture.store(&machine_a);
        let store_b = fixture.store(&machine_b);
        let store_c = fixture.store(&machine_c);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let provider_b = provider(&machine_b, b"env-key-v1");
        let provider_c = provider(&machine_c, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a, provider_a).unwrap();
        let mut replica_b = EnvReplica::new("project", machine_b.clone(), provider_b).unwrap();
        let mut replica_c = EnvReplica::new("project", machine_c.clone(), provider_c).unwrap();

        replica_a
            .set_shared(&store_a, "DATABASE_URL", "postgres://shared", 1_000)
            .unwrap();
        let override_report = replica_b
            .set_override_for_machine(
                &store_b,
                machine_b.clone(),
                "DATABASE_URL",
                "postgres://machine-b-private",
                2_000,
            )
            .unwrap();
        let override_payload_bytes = store_c
            .fetch_generic_payload(ENV_SYNC_PAYLOAD_TYPE, &override_report.payload_id)
            .unwrap();
        let override_payload = EnvSealedPayload::from_wire_bytes(&override_payload_bytes).unwrap();
        let non_target_error = override_payload
            .open(replica_c.key_provider(), replica_c.project_id(), &machine_c)
            .unwrap_err();
        assert!(matches!(non_target_error, EnvSyncError::Integrity(_)));
        let spoofing_provider =
            StdlibTestKeyProvider::with_key(machine_b.clone(), "v1", b"env-key-v1", 1_000)
                .unwrap();
        let spoofed_target_error = override_payload
            .open(&spoofing_provider, replica_b.project_id(), &machine_b)
            .unwrap_err();
        assert!(matches!(spoofed_target_error, EnvSyncError::MissingKey(_)));
        assert!(!format!("{non_target_error:?}{spoofed_target_error:?}")
            .contains("postgres://machine-b-private"));

        let report = replica_c.ingest_from_transport(&store_c).unwrap();
        let materialized = replica_c.materialize().unwrap();
        let override_scope = EnvScope::machine_override(machine_b).unwrap();
        assert!(replica_a
            .state()
            .record(&override_scope, "DATABASE_URL")
            .is_none());

        assert_eq!(report.fetched_payloads, 1);
        assert_eq!(report.applied_records, 1);
        assert!(replica_c
            .state()
            .record(&override_scope, "DATABASE_URL")
            .is_none());
        assert_eq!(
            materialized.launcher_environment.get("DATABASE_URL"),
            Some(&"postgres://shared".to_owned())
        );
        assert!(!format!("{:?}", replica_c).contains("postgres://machine-b-private"));
        assert_eq!(
            replica_c
                .ingest_from_transport(&store_c)
                .unwrap()
                .fetched_payloads,
            0
        );
    }

    #[test]
    fn plaintext_is_absent_from_transport_audit_and_internal_artifacts() {
        let fixture = Fixture::new(&["machine-a", "machine-b"]);
        let machine_a = fixture.machine("machine-a");
        let store_a = fixture.store(&machine_a);
        let secret_value = "SUPER_SECRET_VALUE_123";
        let provider_a = provider(&machine_a, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a, provider_a).unwrap();

        replica_a
            .set_shared(&store_a, "SECRET_VALUE", secret_value, 1_000)
            .unwrap();

        for bytes in read_all_files(fixture.root()) {
            assert_no_plaintext_in_env_artifact(&bytes, secret_value).unwrap();
        }
        assert!(!replica_a
            .audit_log()
            .to_redacted_lines()
            .contains(secret_value));
        assert!(!format!("{:?}", replica_a).contains(secret_value));
    }

    #[test]
    fn plaintext_guard_checks_ciphertext_without_flagging_metadata() {
        let metadata_secret = "v1";
        let metadata_only = format!(
            "format=env-payload-lines-v1\nkey_version={metadata_secret}\nciphertext={}\n",
            hex_bytes(b"encrypted-body")
        );
        assert_no_plaintext_in_env_artifact(metadata_only.as_bytes(), metadata_secret).unwrap();

        let poisoned_ciphertext = format!(
            "format=env-payload-lines-v1\nkey_version=metadata-only\nciphertext={}\n",
            hex_bytes(metadata_secret.as_bytes())
        );
        assert!(matches!(
            assert_no_plaintext_in_env_artifact(poisoned_ciphertext.as_bytes(), metadata_secret),
            Err(EnvSyncError::Integrity(_))
        ));
    }

    #[test]
    fn key_rotation_keeps_old_versions_and_missing_key_cannot_decrypt() {
        let fixture = Fixture::new(&["machine-a", "machine-b", "machine-c"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let machine_c = fixture.machine("machine-c");
        let store_a = fixture.store(&machine_a);
        let store_b = fixture.store(&machine_b);
        let store_c = fixture.store(&machine_c);
        let mut provider_a = provider(&machine_a, b"env-key-v1");
        let mut provider_b = provider(&machine_b, b"env-key-v1");
        let provider_c = provider(&machine_c, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a, provider_a.clone()).unwrap();

        replica_a
            .set_shared(&store_a, "TOKEN_ONE", "old-secret", 1_000)
            .unwrap();
        provider_a.rotate_key("v2", b"env-key-v2", 2_000).unwrap();
        provider_b.rotate_key("v2", b"env-key-v2", 2_000).unwrap();
        replica_a = EnvReplica::new("project", replica_a.machine_id().to_owned(), provider_a).unwrap();
        replica_a
            .set_shared(&store_a, "TOKEN_TWO", "new-secret", 3_000)
            .unwrap();

        let mut replica_b = EnvReplica::new("project", machine_b, provider_b).unwrap();
        replica_b.ingest_from_transport(&store_b).unwrap();
        let materialized = replica_b.materialize().unwrap();
        assert_eq!(
            materialized.launcher_environment.get("TOKEN_ONE"),
            Some(&"old-secret".to_owned())
        );
        assert_eq!(
            materialized.launcher_environment.get("TOKEN_TWO"),
            Some(&"new-secret".to_owned())
        );
        assert_eq!(
            replica_b
                .key_provider()
                .key_version_records()
                .iter()
                .filter(|record| record.active)
                .map(|record| record.key_version.as_str())
                .collect::<Vec<_>>(),
            vec!["v2"]
        );

        let mut replica_c = EnvReplica::new("project", machine_c, provider_c).unwrap();
        let error = replica_c.ingest_from_transport(&store_c).unwrap_err();
        assert!(matches!(error, EnvSyncError::MissingKey(_)));
        assert!(replica_c
            .audit_log()
            .entries()
            .iter()
            .any(|entry| entry.operation == EnvAuditOperation::DecryptFailed));
    }

    #[test]
    fn audit_log_records_env_operations_without_values() {
        let fixture = Fixture::new(&["machine-a", "machine-b"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let store_a = fixture.store(&machine_a);
        let store_b = fixture.store(&machine_b);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let provider_b = provider(&machine_b, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a, provider_a).unwrap();
        let mut replica_b = EnvReplica::new("project", machine_b, provider_b).unwrap();

        replica_a
            .set_shared(&store_a, "AUDITED_SECRET", "audit-secret", 1_000)
            .unwrap();
        replica_b.ingest_from_transport(&store_b).unwrap();
        replica_b.materialize().unwrap();
        let entries = replica_b.audit_log().entries();
        let operations = entries
            .iter()
            .map(|entry| entry.operation.clone())
            .collect::<Vec<_>>();

        assert!(operations.contains(&EnvAuditOperation::PayloadFetched));
        assert!(operations.contains(&EnvAuditOperation::PayloadDecrypted));
        assert!(operations.contains(&EnvAuditOperation::ValueApplied));
        assert!(operations.contains(&EnvAuditOperation::Materialized));
        assert!(entries.iter().all(|entry| entry.event_unix_millis > 0));
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.operation == EnvAuditOperation::ValueApplied)
                .unwrap()
                .event_unix_millis,
            1_000
        );
        let redacted_lines = replica_b.audit_log().to_redacted_lines();
        assert!(!redacted_lines.contains("audit-secret"));
        assert!(redacted_lines.contains(ENV_REDACTED_VALUE));
        assert!(redacted_lines.contains("event_unix_millis%3D1000"));
    }

    #[test]
    fn divergent_shared_env_values_create_lww_conflict_sidecar() {
        let fixture = Fixture::new(&["machine-a", "machine-b", "machine-c"]);
        let machine_a = fixture.machine("machine-a");
        let machine_b = fixture.machine("machine-b");
        let machine_c = fixture.machine("machine-c");
        let store_a = fixture.store(&machine_a);
        let store_b = fixture.store(&machine_b);
        let store_c = fixture.store(&machine_c);
        let provider_a = provider(&machine_a, b"env-key-v1");
        let provider_b = provider(&machine_b, b"env-key-v1");
        let provider_c = provider(&machine_c, b"env-key-v1");
        let mut replica_a = EnvReplica::new("project", machine_a, provider_a).unwrap();
        let mut replica_b = EnvReplica::new("project", machine_b, provider_b).unwrap();
        let mut replica_c = EnvReplica::new("project", machine_c, provider_c).unwrap();

        let loser_report = replica_a
            .set_shared(&store_a, "CONFLICTING", "loser-secret", 1_000)
            .unwrap();
        replica_b
            .set_shared(&store_b, "CONFLICTING", "winner-secret", 2_000)
            .unwrap();
        let loser_payload_bytes = store_c
            .fetch_generic_payload(ENV_SYNC_PAYLOAD_TYPE, &loser_report.payload_id)
            .unwrap();
        let expected_loser_payload_digest = env_payload_digest(&loser_payload_bytes);
        let restored_loser = EnvSealedPayload::from_wire_bytes(&loser_payload_bytes)
            .unwrap()
            .open(replica_c.key_provider(), replica_c.project_id(), replica_c.machine_id())
            .unwrap();
        let report = replica_c.ingest_from_transport(&store_c).unwrap();
        let materialized = replica_c.materialize().unwrap();
        let sidecars = replica_c.state().conflict_sidecars();

        assert_eq!(report.conflict_sidecars, 1);
        assert_eq!(sidecars.len(), 1);
        assert!(sidecars[0].path.contains(".sync-conflict"));
        assert_eq!(sidecars[0].loser_value, RedactedEnvValue);
        assert_eq!(sidecars[0].loser_payload_id, loser_report.payload_id);
        assert_eq!(
            sidecars[0].loser_payload_digest,
            expected_loser_payload_digest
        );
        assert_eq!(restored_loser.value.expose_for_materialization(), "loser-secret");
        assert_eq!(
            sidecars[0].manual_escape_hatch,
            ManualConflictResolution::KeepBoth
        );
        assert_eq!(
            materialized.launcher_environment.get("CONFLICTING"),
            Some(&"winner-secret".to_owned())
        );
        assert!(!format!("{:?}", sidecars).contains("loser-secret"));
    }

    #[test]
    fn env_migration_apply_and_rollback_registers_only_env_tables() {
        let runner = env_migration_runner().unwrap();
        let mut store = InMemoryMigrationStore::new();

        let report = runner.apply(&mut store).unwrap();
        assert_eq!(report.schema_version, ENV_MIGRATION_VERSION);
        assert_eq!(
            report.product_tables,
            ENV_MIGRATION_TABLES
                .iter()
                .map(|table| (*table).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(store.applied_sql(), ENV_MIGRATION_UP_SQL);
        assert!(!store.applied_sql().join("\n").contains("plaintext"));
        assert!(store
            .applied_sql()
            .join("\n")
            .contains("event_unix_millis INTEGER NOT NULL"));
        assert!(store
            .applied_sql()
            .join("\n")
            .contains("loser_payload_id TEXT NOT NULL"));

        let rollback = runner.rollback(&mut store).unwrap();
        assert_eq!(rollback.schema_version, BASELINE_SCHEMA_VERSION);
        assert!(rollback.product_tables.is_empty());
        assert_eq!(store.rolled_back_sql(), ENV_MIGRATION_DOWN_SQL);
    }

    struct Fixture {
        root: PathBuf,
        machines: BTreeMap<String, String>,
        authorized: Vec<String>,
    }

    impl Fixture {
        fn new(machine_labels: &[&str]) -> Self {
            let root = test_root();
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let machines = machine_labels
                .iter()
                .map(|label| ((*label).to_owned(), app_scoped_machine_id(label).unwrap()))
                .collect::<BTreeMap<_, _>>();
            let authorized = machines.values().cloned().collect::<Vec<_>>();
            Self {
                root,
                machines,
                authorized,
            }
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn machine(&self, label: &str) -> String {
            self.machines.get(label).unwrap().clone()
        }

        fn store(&self, machine_id: &str) -> FileBackedSyncStore {
            let secret = SharedSecret::from_pairing_token("sync-pairing-token").unwrap();
            let config = EndpointSecurityConfig::file_backed(
                TransportMode::LocalHarness,
                self.root.clone(),
                self.authorized.clone(),
                secret,
            );
            FileBackedSyncStore::new(config, "project", machine_id.to_owned()).unwrap()
        }
    }

    fn provider(machine_id: &str, material: &[u8]) -> StdlibTestKeyProvider {
        let mut provider =
            StdlibTestKeyProvider::with_key(machine_id.to_owned(), "v1", material, 1_000).unwrap();
        provider
            .provision_machine_override_secret(
                "v1",
                machine_id.to_owned(),
                test_machine_override_secret(machine_id),
            )
            .unwrap();
        provider
    }

    fn test_machine_override_secret(machine_id: &str) -> Vec<u8> {
        let mut material = b"env-test-machine-override-secret-v1:".to_vec();
        material.extend_from_slice(machine_id.as_bytes());
        material
    }

    fn test_root() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "dropbox-dev-env-sync-test-{}-{id}",
            std::process::id()
        ))
    }

    fn read_all_files(root: &Path) -> Vec<Vec<u8>> {
        let mut bytes = Vec::new();
        read_all_files_into(root, &mut bytes);
        bytes
    }

    fn read_all_files_into(path: &Path, output: &mut Vec<Vec<u8>>) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                read_all_files_into(&entry.unwrap().path(), output);
            }
        } else if path.is_file() {
            output.push(fs::read(path).unwrap());
        }
    }
}
