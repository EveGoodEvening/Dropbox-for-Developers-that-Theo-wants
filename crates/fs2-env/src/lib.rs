#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Environment variable data model and precedence rules.

use fs2_core::{
    DeviceId, EnvScope as CoreEnvScope, EnvVarId, SecretKind, WorkspaceId, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, fmt, str::FromStr};

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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use fs2_core::{DeviceId, EnvVarId, WorkspaceId};
    use std::str::FromStr;
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
}
