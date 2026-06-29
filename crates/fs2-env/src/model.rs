//! Environment variable data model.
//!
//! Env vars are scoped to workspace/project/machine with precedence:
//! `ProjectMachine` > Machine > Project > Workspace.

use chrono::{DateTime, Utc};
use fs2_core::op::{EnvScope, EnvVarMetadata, SecretKind};
use fs2_core::WorkspaceId;
use uuid::Uuid;

/// A stored env var record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvVar {
    /// Unique env var id.
    pub env_var_id: Uuid,
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Project path within the workspace (None for workspace scope).
    pub project_path: Option<String>,
    /// Environment name (dev, test, prod, etc.).
    pub environment: String,
    /// Variable name (e.g. `STRIPE_SECRET_KEY`).
    pub name: String,
    /// Scope of the variable.
    pub scope: EnvScope,
    /// Whether the value is a secret or plain config.
    pub secret_kind: SecretKind,
    /// Encrypted value (base64 ciphertext).
    pub encrypted_value: String,
    /// Public metadata.
    pub metadata: EnvVarMetadata,
    /// Whether deleted (tombstoned).
    pub deleted: bool,
}

/// A record for display/listing (value redacted).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EnvVarRecord {
    /// Env var id.
    pub env_var_id: Uuid,
    /// Variable name.
    pub name: String,
    /// Environment.
    pub environment: String,
    /// Project path.
    pub project_path: Option<String>,
    /// Scope.
    pub scope: EnvScope,
    /// Secret kind.
    pub secret_kind: SecretKind,
    /// Redacted value indicator.
    pub value_display: String,
    /// Last updated.
    pub updated_at: DateTime<Utc>,
}

impl EnvVar {
    /// Create a display record with the value redacted.
    #[must_use]
    pub fn to_record(&self) -> EnvVarRecord {
        EnvVarRecord {
            env_var_id: self.env_var_id,
            name: self.name.clone(),
            environment: self.environment.clone(),
            project_path: self.project_path.clone(),
            scope: self.scope.clone(),
            secret_kind: self.secret_kind,
            value_display: "********".to_owned(),
            updated_at: self.metadata.updated_at,
        }
    }
}

/// Validate an environment variable name.
///
/// Must be non-empty, contain only uppercase letters, digits, and underscores,
/// and not start with a digit.
///
/// # Errors
/// Returns an error if the name is invalid.
pub fn validate_env_name(name: &str) -> Result<(), EnvValidationError> {
    if name.is_empty() {
        return Err(EnvValidationError::EmptyName);
    }
    if name
        .chars()
        .any(|c| !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '_')
    {
        return Err(EnvValidationError::InvalidChars);
    }
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(EnvValidationError::StartsWithDigit);
    }
    Ok(())
}

/// Validate an environment name (dev, test, prod, etc.).
///
/// Must be non-empty and contain only lowercase letters, digits, and hyphens.
///
/// # Errors
/// Returns an error if the environment name is invalid.
pub fn validate_environment_name(env: &str) -> Result<(), EnvValidationError> {
    if env.is_empty() {
        return Err(EnvValidationError::EmptyEnvironment);
    }
    if env
        .chars()
        .any(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-')
    {
        return Err(EnvValidationError::InvalidEnvironment);
    }
    Ok(())
}

/// Error returned by env validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvValidationError {
    /// Name is empty.
    #[error("env var name must not be empty")]
    EmptyName,
    /// Name contains invalid characters.
    #[error("env var name must contain only uppercase letters, digits, and underscores")]
    InvalidChars,
    /// Name starts with a digit.
    #[error("env var name must not start with a digit")]
    StartsWithDigit,
    /// Environment is empty.
    #[error("environment name must not be empty")]
    EmptyEnvironment,
    /// Environment contains invalid characters.
    #[error("environment name must contain only lowercase letters, digits, and hyphens")]
    InvalidEnvironment,
}

/// Compute the precedence rank of a scope (higher = more specific).
#[must_use]
pub fn scope_precedence(scope: &EnvScope) -> u8 {
    match scope {
        EnvScope::Workspace => 0,
        EnvScope::Project => 1,
        EnvScope::Machine { .. } => 2,
        EnvScope::ProjectMachine { .. } => 3,
    }
}

/// Resolve which env var wins when multiple scopes match.
///
/// Returns the env var with the highest precedence. Ties are broken by
/// insertion order (last wins).
#[must_use]
pub fn resolve_precedence(vars: &[EnvVar]) -> Option<&EnvVar> {
    vars.iter()
        .filter(|v| !v.deleted)
        .max_by_key(|v| scope_precedence(&v.scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2_core::DeviceId;

    #[test]
    fn validate_valid_names() {
        assert!(validate_env_name("STRIPE_SECRET_KEY").is_ok());
        assert!(validate_env_name("API_URL").is_ok());
        assert!(validate_env_name("_PRIVATE").is_ok());
        assert!(validate_env_name("A").is_ok());
    }

    #[test]
    fn validate_invalid_names() {
        assert!(validate_env_name("").is_err());
        assert!(validate_env_name("lowercase").is_err());
        assert!(validate_env_name("WITH-SPACE").is_err());
        assert!(validate_env_name("1STARTS_WITH_DIGIT").is_err());
    }

    #[test]
    fn validate_valid_environments() {
        assert!(validate_environment_name("dev").is_ok());
        assert!(validate_environment_name("test").is_ok());
        assert!(validate_environment_name("staging").is_ok());
        assert!(validate_environment_name("prod-1").is_ok());
    }

    #[test]
    fn validate_invalid_environments() {
        assert!(validate_environment_name("").is_err());
        assert!(validate_environment_name("DEV").is_err());
        assert!(validate_environment_name("dev test").is_err());
    }

    #[test]
    fn precedence_ordering() {
        let workspace = EnvScope::Workspace;
        let project = EnvScope::Project;
        let machine = EnvScope::Machine {
            device_id: DeviceId::new(),
        };
        let project_machine = EnvScope::ProjectMachine {
            project_path: "apps/web".to_owned(),
            device_id: DeviceId::new(),
        };
        assert!(scope_precedence(&project_machine) > scope_precedence(&machine));
        assert!(scope_precedence(&machine) > scope_precedence(&project));
        assert!(scope_precedence(&project) > scope_precedence(&workspace));
    }

    #[test]
    fn to_record_redacts_value() {
        let var = EnvVar {
            env_var_id: Uuid::new_v4(),
            workspace_id: WorkspaceId::new(),
            project_path: Some("apps/web".to_owned()),
            environment: "dev".to_owned(),
            name: "STRIPE_SECRET_KEY".to_owned(),
            scope: EnvScope::Project,
            secret_kind: SecretKind::Secret,
            encrypted_value: "base64ciphertext".to_owned(),
            metadata: EnvVarMetadata {
                name: "STRIPE_SECRET_KEY".to_owned(),
                environment: "dev".to_owned(),
                project_path: Some("apps/web".to_owned()),
                scope: EnvScope::Project,
                secret_kind: SecretKind::Secret,
                updated_at: Utc::now(),
            },
            deleted: false,
        };
        let record = var.to_record();
        assert_eq!(record.value_display, "********");
        assert_eq!(record.name, "STRIPE_SECRET_KEY");
    }
}
