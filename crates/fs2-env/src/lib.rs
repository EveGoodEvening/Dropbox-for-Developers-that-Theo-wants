#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Environment variable data model and precedence rules.

use fs2_core::{
    DeviceId, EnvScope as CoreEnvScope, EnvVarId, SecretKind, WorkspaceId, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-env"
}

/// Validation error for environment records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvError {
    field: &'static str,
    message: &'static str,
}

impl EnvError {
    const fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }

    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }

    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.field, self.message)
    }
}

impl std::error::Error for EnvError {}

/// Shell-portable environment variable name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EnvVarName(String);

impl EnvVarName {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, EnvError> {
        let value = value.as_ref();
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            return Err(EnvError::new("env_name", "must not be empty"));
        };
        if !(first == '_' || first.is_ascii_alphabetic()) {
            return Err(EnvError::new(
                "env_name",
                "must start with ASCII letter or underscore",
            ));
        }
        if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
            return Err(EnvError::new(
                "env_name",
                "must contain only ASCII letters, digits, or underscore",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for EnvVarName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for EnvVarName {
    type Err = EnvError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for EnvVarName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Canonical environment name, for example `dev`, `test`, `staging`, or `prod`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EnvironmentName(String);

impl EnvironmentName {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, EnvError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(EnvError::new("environment", "must not be empty"));
        }
        if value.len() > 64 {
            return Err(EnvError::new("environment", "must be at most 64 bytes"));
        }
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            return Err(EnvError::new("environment", "must not be empty"));
        };
        if !first.is_ascii_lowercase() {
            return Err(EnvError::new(
                "environment",
                "must start with lowercase ASCII letter",
            ));
        }
        if !chars.all(|ch| ch == '-' || ch == '_' || ch.is_ascii_lowercase() || ch.is_ascii_digit())
        {
            return Err(EnvError::new(
                "environment",
                "must contain lowercase ASCII letters, digits, hyphen, or underscore",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for EnvironmentName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for EnvironmentName {
    type Err = EnvError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for EnvironmentName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Scope for an environment variable record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvVarScope {
    Workspace,
    Project {
        project_path: WorkspacePath,
    },
    Machine {
        device_id: DeviceId,
    },
    ProjectMachine {
        project_path: WorkspacePath,
        device_id: DeviceId,
    },
}

impl EnvVarScope {
    #[must_use]
    pub const fn precedence_rank(&self) -> u8 {
        match self {
            Self::Workspace => 0,
            Self::Machine { .. } => 10,
            Self::Project { .. } => 20,
            Self::ProjectMachine { .. } => 30,
        }
    }

    #[must_use]
    pub const fn project_path(&self) -> Option<&WorkspacePath> {
        match self {
            Self::Workspace | Self::Machine { .. } => None,
            Self::Project { project_path } | Self::ProjectMachine { project_path, .. } => {
                Some(project_path)
            }
        }
    }

    #[must_use]
    pub const fn device_id(&self) -> Option<DeviceId> {
        match self {
            Self::Workspace | Self::Project { .. } => None,
            Self::Machine { device_id } | Self::ProjectMachine { device_id, .. } => {
                Some(*device_id)
            }
        }
    }

    #[must_use]
    pub fn matches(&self, context: &EnvResolutionContext) -> bool {
        match self {
            Self::Workspace => true,
            Self::Project { project_path } => context.project_path.as_ref() == Some(project_path),
            Self::Machine { device_id } => context.device_id == Some(*device_id),
            Self::ProjectMachine {
                project_path,
                device_id,
            } => {
                context.project_path.as_ref() == Some(project_path)
                    && context.device_id == Some(*device_id)
            }
        }
    }
}

impl From<&EnvVarScope> for CoreEnvScope {
    fn from(scope: &EnvVarScope) -> Self {
        match scope {
            EnvVarScope::Workspace => Self::Workspace,
            EnvVarScope::Project { project_path } => Self::Project {
                project_path: project_path.as_str().to_owned(),
            },
            EnvVarScope::Machine { device_id } => Self::Machine {
                device_id: *device_id,
            },
            EnvVarScope::ProjectMachine {
                project_path,
                device_id,
            } => Self::ProjectMachine {
                project_path: project_path.as_str().to_owned(),
                device_id: *device_id,
            },
        }
    }
}

impl TryFrom<CoreEnvScope> for EnvVarScope {
    type Error = EnvError;

    fn try_from(scope: CoreEnvScope) -> Result<Self, Self::Error> {
        match scope {
            CoreEnvScope::Workspace => Ok(Self::Workspace),
            CoreEnvScope::Project { project_path } => Ok(Self::Project {
                project_path: WorkspacePath::parse(project_path)
                    .map_err(|_| EnvError::new("scope.project_path", "must be valid"))?,
            }),
            CoreEnvScope::Machine { device_id } => Ok(Self::Machine { device_id }),
            CoreEnvScope::ProjectMachine {
                project_path,
                device_id,
            } => Ok(Self::ProjectMachine {
                project_path: WorkspacePath::parse(project_path)
                    .map_err(|_| EnvError::new("scope.project_path", "must be valid"))?,
                device_id,
            }),
        }
    }
}

/// Encrypted environment variable record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub env_var_id: EnvVarId,
    pub workspace_id: WorkspaceId,
    pub project_path: Option<WorkspacePath>,
    pub env_name: EnvVarName,
    pub environment: EnvironmentName,
    pub scope: EnvVarScope,
    pub secret_kind: SecretKind,
    pub encrypted_value: String,
}

impl EnvVar {
    pub fn new(
        env_var_id: EnvVarId,
        workspace_id: WorkspaceId,
        env_name: EnvVarName,
        environment: EnvironmentName,
        scope: EnvVarScope,
        secret_kind: SecretKind,
        encrypted_value: String,
    ) -> Result<Self, EnvError> {
        if encrypted_value.is_empty() {
            return Err(EnvError::new("encrypted_value", "must not be empty"));
        }
        Ok(Self {
            env_var_id,
            workspace_id,
            project_path: scope.project_path().cloned(),
            env_name,
            environment,
            scope,
            secret_kind,
            encrypted_value,
        })
    }

    pub fn validate(&self) -> Result<(), EnvError> {
        let expected_project_path = self.scope.project_path().cloned();
        if self.project_path != expected_project_path {
            return Err(EnvError::new(
                "project_path",
                "must mirror scope project path",
            ));
        }
        if self.encrypted_value.is_empty() {
            return Err(EnvError::new("encrypted_value", "must not be empty"));
        }
        Ok(())
    }

    #[must_use]
    pub fn identity(&self) -> EnvVarIdentity {
        EnvVarIdentity {
            workspace_id: self.workspace_id,
            environment: self.environment.clone(),
            env_name: self.env_name.clone(),
            scope: self.scope.clone(),
        }
    }

    #[must_use]
    pub fn matches(&self, context: &EnvResolutionContext) -> bool {
        self.workspace_id == context.workspace_id
            && self.environment == context.environment
            && self.env_name == context.env_name
            && self.scope.matches(context)
    }
}

/// Unique live-record identity. Duplicate identities are ambiguous data corruption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVarIdentity {
    pub workspace_id: WorkspaceId,
    pub environment: EnvironmentName,
    pub env_name: EnvVarName,
    pub scope: EnvVarScope,
}

/// Target context for selecting one env var value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvResolutionContext {
    pub workspace_id: WorkspaceId,
    pub project_path: Option<WorkspacePath>,
    pub device_id: Option<DeviceId>,
    pub env_name: EnvVarName,
    pub environment: EnvironmentName,
}

/// Selects the single highest-precedence matching record.
pub fn resolve_env_var<'a>(
    records: impl IntoIterator<Item = &'a EnvVar>,
    context: &EnvResolutionContext,
) -> Result<Option<&'a EnvVar>, EnvError> {
    let mut selected: Option<&'a EnvVar> = None;
    let mut matched_identities = Vec::new();
    for record in records {
        record.validate()?;
        if !record.matches(context) {
            continue;
        }
        let identity = record.identity();
        if matched_identities
            .iter()
            .any(|existing| existing == &identity)
        {
            return Err(EnvError::new("env_var", "duplicate live identity"));
        }
        matched_identities.push(identity);
        if let Some(current) = selected {
            match record
                .scope
                .precedence_rank()
                .cmp(&current.scope.precedence_rank())
            {
                Ordering::Greater => selected = Some(record),
                Ordering::Equal => return Err(EnvError::new("env_var", "ambiguous precedence")),
                Ordering::Less => {}
            }
        } else {
            selected = Some(record);
        }
    }
    Ok(selected)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotenvError {
    line: usize,
    message: String,
}

impl DotenvError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DotenvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "dotenv line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for DotenvError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DotenvEntry {
    pub name: EnvVarName,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvFileRuleAction {
    LocalOnly,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileRule {
    pub path: PathBuf,
    pub action: EnvFileRuleAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileSafetyPlan {
    pub rules: Vec<EnvFileRule>,
    pub warnings: Vec<String>,
}

pub fn parse_dotenv(input: &str) -> Result<Vec<DotenvEntry>, DotenvError> {
    let mut entries = Vec::new();
    let mut lines = input.lines().enumerate().peekable();
    while let Some((index, raw_line)) = lines.next() {
        let line_number = index + 1;
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (name, raw_value) = line
            .split_once('=')
            .ok_or_else(|| DotenvError::new(line_number, "expected NAME=value"))?;
        let name = EnvVarName::parse(name.trim()).map_err(|error| {
            DotenvError::new(line_number, format!("invalid variable name: {error}"))
        })?;
        let value = parse_dotenv_value(raw_value.trim_start(), line_number, &mut lines)?;
        entries.push(DotenvEntry { name, value });
    }
    Ok(entries)
}

pub fn materialize_dotenv(
    path: impl AsRef<Path>,
    values: &[(EnvVarName, String)],
) -> Result<EnvFileSafetyPlan, io::Error> {
    let path = path.as_ref();
    let mut sorted = BTreeMap::new();
    for (name, value) in values {
        sorted.insert(name.as_str().to_owned(), value.clone());
    }
    let mut out = String::new();
    for (name, value) in sorted {
        out.push_str(&name);
        out.push('=');
        out.push_str(&quote_dotenv_value(&value));
        out.push('\n');
    }
    write_secret_file(path, out.as_bytes())?;
    Ok(EnvFileSafetyPlan {
        rules: vec![EnvFileRule {
            path: path.to_path_buf(),
            action: EnvFileRuleAction::Secret,
        }],
        warnings: git_tracked_warning(path)?.into_iter().collect(),
    })
}

pub fn import_dotenv_safety_plan(path: impl AsRef<Path>) -> Result<EnvFileSafetyPlan, io::Error> {
    let path = path.as_ref();
    Ok(EnvFileSafetyPlan {
        rules: vec![EnvFileRule {
            path: path.to_path_buf(),
            action: EnvFileRuleAction::Secret,
        }],
        warnings: git_tracked_warning(path)?.into_iter().collect(),
    })
}

fn parse_dotenv_value<'a>(
    raw_value: &'a str,
    line_number: usize,
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>,
) -> Result<String, DotenvError> {
    match raw_value.chars().next() {
        Some('\'') => parse_quoted_value(raw_value, '\'', line_number, lines),
        Some('"') => parse_quoted_value(raw_value, '"', line_number, lines),
        _ => Ok(parse_unquoted_value(raw_value)),
    }
}

fn parse_quoted_value<'a>(
    first: &'a str,
    quote: char,
    line_number: usize,
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>,
) -> Result<String, DotenvError> {
    let mut value = String::new();
    let mut current = first[quote.len_utf8()..].to_owned();
    loop {
        let mut escaped = false;
        for (offset, ch) in current.char_indices() {
            if escaped {
                match (quote, ch) {
                    ('"', 'n') => value.push('\n'),
                    ('"', 'r') => value.push('\r'),
                    ('"', 't') => value.push('\t'),
                    ('"', '"' | '\\') => value.push(ch),
                    ('"', other) => {
                        value.push('\\');
                        value.push(other);
                    }
                    (_, other) => value.push(other),
                }
                escaped = false;
                continue;
            }
            if quote == '"' && ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                let trailing = current[offset + quote.len_utf8()..].trim();
                if !trailing.is_empty() && !trailing.starts_with('#') {
                    return Err(DotenvError::new(
                        line_number,
                        "unexpected content after quoted value",
                    ));
                }
                return Ok(value);
            }
            value.push(ch);
        }
        if escaped {
            value.push('\\');
        }
        value.push('\n');
        let Some((_, next_line)) = lines.next() else {
            return Err(DotenvError::new(line_number, "unterminated quoted value"));
        };
        next_line.clone_into(&mut current);
    }
}

fn parse_unquoted_value(raw_value: &str) -> String {
    let mut value = String::new();
    for ch in raw_value.chars() {
        if ch == '#' && (value.is_empty() || value.ends_with(char::is_whitespace)) {
            break;
        }
        value.push(ch);
    }
    value.trim_end().to_owned()
}

fn quote_dotenv_value(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch == '_' || ch == '-' || ch == '.' || ch.is_ascii_alphanumeric())
    {
        return value.to_owned();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    use std::os::unix::fs::OpenOptionsExt;
    match fs::metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => set_secret_file_permissions(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "secret materialization target must be a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    set_secret_file_permissions(path)
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    fs::write(path, bytes)?;
    set_secret_file_permissions(path)
}

#[cfg(unix)]
fn set_secret_file_permissions(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_secret_file_permissions(_path: &Path) -> Result<(), io::Error> {
    Ok(())
}

fn git_tracked_warning(path: &Path) -> Result<Option<String>, io::Error> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let Some(file_name) = path.file_name() else {
        return Ok(None);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let output = Command::new("git")
        .arg("ls-files")
        .arg("--error-unmatch")
        .arg(file_name)
        .current_dir(parent)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(Some(format!(
            "{} is tracked by Git; remove it from Git before importing secrets",
            path.display()
        ))),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use fs2_core::{DeviceId, EnvVarId, WorkspaceId};
    use std::str::FromStr;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn uuid_from(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid_from(1))
    }

    fn device_id(byte: u8) -> DeviceId {
        DeviceId::from_uuid(uuid_from(byte))
    }

    fn env_id(byte: u8) -> EnvVarId {
        EnvVarId::from_uuid(uuid_from(byte))
    }

    fn env_name() -> EnvVarName {
        EnvVarName::from_str("STRIPE_SECRET_KEY").unwrap_or_else(|error| panic!("{error}"))
    }

    fn environment() -> EnvironmentName {
        EnvironmentName::from_str("dev").unwrap_or_else(|error| panic!("{error}"))
    }

    fn project_path() -> WorkspacePath {
        WorkspacePath::parse("apps/web").unwrap_or_else(|error| panic!("{error}"))
    }

    fn record(id: u8, scope: EnvVarScope, encrypted_value: &str) -> EnvVar {
        EnvVar::new(
            env_id(id),
            workspace_id(),
            env_name(),
            environment(),
            scope,
            SecretKind::Secret,
            encrypted_value.to_owned(),
        )
        .unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-env");
    }

    #[test]
    fn validates_env_names() {
        assert!(EnvVarName::parse("STRIPE_SECRET_KEY").is_ok());
        assert!(EnvVarName::parse("_TOKEN2").is_ok());
        assert!(EnvVarName::parse("2BAD").is_err());
        assert!(EnvVarName::parse("BAD-NAME").is_err());
        assert!(EnvVarName::parse("BAD=NAME").is_err());
    }

    #[test]
    fn validates_environment_names() {
        assert!(EnvironmentName::parse("dev").is_ok());
        assert!(EnvironmentName::parse("preview_1").is_ok());
        assert!(EnvironmentName::parse("Dev").is_err());
        assert!(EnvironmentName::parse("1dev").is_err());
        assert!(EnvironmentName::parse("dev.prod!").is_err());
    }

    #[test]
    fn scope_model_converts_to_core_contract() -> Result<(), Box<dyn std::error::Error>> {
        let scope = EnvVarScope::ProjectMachine {
            project_path: project_path(),
            device_id: device_id(2),
        };

        let core = CoreEnvScope::from(&scope);
        assert_eq!(EnvVarScope::try_from(core)?, scope);
        assert_eq!(
            scope.project_path().map(WorkspacePath::as_str),
            Some("apps/web")
        );
        assert_eq!(scope.device_id(), Some(device_id(2)));
        Ok(())
    }

    #[test]
    fn record_project_path_must_mirror_scope() {
        let mut value = record(
            10,
            EnvVarScope::Project {
                project_path: project_path(),
            },
            "encrypted-project",
        );
        value.project_path = None;

        assert!(value.validate().is_err());
    }

    #[test]
    fn resolution_precedence_is_unambiguous() -> Result<(), Box<dyn std::error::Error>> {
        let machine_device = device_id(2);
        let records = [
            record(10, EnvVarScope::Workspace, "workspace"),
            record(
                11,
                EnvVarScope::Machine {
                    device_id: machine_device,
                },
                "machine",
            ),
            record(
                12,
                EnvVarScope::Project {
                    project_path: project_path(),
                },
                "project",
            ),
            record(
                13,
                EnvVarScope::ProjectMachine {
                    project_path: project_path(),
                    device_id: machine_device,
                },
                "project-machine",
            ),
        ];
        let context = EnvResolutionContext {
            workspace_id: workspace_id(),
            project_path: Some(project_path()),
            device_id: Some(machine_device),
            env_name: env_name(),
            environment: environment(),
        };

        let selected = resolve_env_var(&records, &context)?.expect("matching env var");
        assert_eq!(selected.encrypted_value, "project-machine");

        let records = &records[..3];
        let selected = resolve_env_var(records, &context)?.expect("matching env var");
        assert_eq!(selected.encrypted_value, "project");
        Ok(())
    }

    #[test]
    fn no_device_context_ignores_machine_scopes() -> Result<(), Box<dyn std::error::Error>> {
        let records = [
            record(14, EnvVarScope::Workspace, "workspace"),
            record(
                15,
                EnvVarScope::Machine {
                    device_id: device_id(2),
                },
                "machine",
            ),
        ];
        let context = EnvResolutionContext {
            workspace_id: workspace_id(),
            project_path: None,
            device_id: None,
            env_name: env_name(),
            environment: environment(),
        };

        let selected = resolve_env_var(&records, &context)?.expect("matching env var");
        assert_eq!(selected.encrypted_value, "workspace");
        Ok(())
    }

    #[test]
    fn lower_precedence_duplicate_identity_is_ambiguous() {
        let records = [
            record(
                16,
                EnvVarScope::ProjectMachine {
                    project_path: project_path(),
                    device_id: device_id(2),
                },
                "project-machine",
            ),
            record(17, EnvVarScope::Workspace, "workspace-one"),
            record(18, EnvVarScope::Workspace, "workspace-two"),
        ];
        let context = EnvResolutionContext {
            workspace_id: workspace_id(),
            project_path: Some(project_path()),
            device_id: Some(device_id(2)),
            env_name: env_name(),
            environment: environment(),
        };

        assert!(resolve_env_var(&records, &context).is_err());
    }

    #[test]
    fn duplicate_live_identity_is_ambiguous() {
        let records = [
            record(20, EnvVarScope::Workspace, "one"),
            record(21, EnvVarScope::Workspace, "two"),
        ];
        let context = EnvResolutionContext {
            workspace_id: workspace_id(),
            project_path: Some(project_path()),
            device_id: Some(device_id(2)),
            env_name: env_name(),
            environment: environment(),
        };

        assert!(resolve_env_var(&records, &context).is_err());
    }

    #[test]
    fn parses_common_dotenv_syntax_and_multiline_values() -> Result<(), Box<dyn std::error::Error>>
    {
        let entries = parse_dotenv(
            "# comment\nexport API_URL=https://example.invalid # public\nEMPTY= # intentionally blank\nSECRET='literal # hash'\nMULTI=\"line one\nline two\"\nSPACE_MULTI=\"abc  \ndef\"\nTRAILING_BACKSLASH=\"abc\\\ndef\"\nESCAPED=\"a\\nb\"\nREGEX=^\\d+$\nDQ_REGEX=\"^\\d+$\"\nUNC=\\\\server\\share\n",
        )?;

        assert_eq!(entries[0].value, "https://example.invalid");
        assert_eq!(entries[1].name.as_str(), "EMPTY");
        assert_eq!(entries[1].value, "");
        assert_eq!(entries[2].value, "literal # hash");
        assert_eq!(entries[3].value, "line one\nline two");
        assert_eq!(entries[4].value, "abc  \ndef");
        assert_eq!(entries[5].value, "abc\\\ndef");
        assert_eq!(entries[6].value, "a\nb");
        assert_eq!(entries[7].value, "^\\d+$");
        assert_eq!(entries[8].value, "^\\d+$");
        assert_eq!(entries[9].value, "\\\\server\\share");
        Ok(())
    }

    #[test]
    fn materializes_dotenv_with_secret_permissions_and_rule(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let path = dir.path().join(".env.fs2");
        let plan = materialize_dotenv(
            &path,
            &[
                (
                    EnvVarName::parse("SECRET_KEY")?,
                    "line one\nline two".to_owned(),
                ),
                (
                    EnvVarName::parse("API_URL")?,
                    "https://example.invalid".to_owned(),
                ),
            ],
        )?;

        let content = fs::read_to_string(&path)?;
        assert!(content.contains("API_URL=\"https://example.invalid\""));
        assert!(content.contains("SECRET_KEY=\"line one\\nline two\""));
        assert_eq!(plan.rules.len(), 1);
        assert_eq!(plan.rules[0].path, path);
        assert_eq!(plan.rules[0].action, EnvFileRuleAction::Secret);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&plan.rules[0].path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        Ok(())
    }

    #[test]
    fn materialize_rejects_directory_target() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let target_dir = dir.path().join("target-dir");
        fs::create_dir(&target_dir)?;

        let result = materialize_dotenv(
            &target_dir,
            &[(EnvVarName::parse("SECRET_KEY")?, "value".to_owned())],
        );

        assert!(result.is_err());
        assert!(fs::metadata(&target_dir)?.file_type().is_dir());
        Ok(())
    }

    #[test]
    fn import_plan_warns_for_git_tracked_dotenv() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let git_available = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !git_available {
            return Ok(());
        }
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()?;
        let env_path = dir.path().join(".env");
        fs::write(&env_path, "SECRET=value\n")?;
        std::process::Command::new("git")
            .arg("add")
            .arg(".env")
            .current_dir(dir.path())
            .output()?;

        let plan = import_dotenv_safety_plan(&env_path)?;

        assert_eq!(plan.rules[0].action, EnvFileRuleAction::Secret);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("tracked by Git")));
        Ok(())
    }
}
