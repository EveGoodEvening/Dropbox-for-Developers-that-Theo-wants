//! `.fs2/config.toml` parser.
//!
//! Defines the structured config schema and validates action names and cache
//! size strings.

use serde::{Deserialize, Serialize};

use crate::action::Action;

/// Top-level workspace config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Workspace name.
    pub workspace_name: String,
    /// Default file policy for existing files.
    #[serde(default = "default_file_policy")]
    pub default_file_policy: Action,
    /// Default policy for newly created files.
    #[serde(default = "default_new_file_policy")]
    pub default_new_file_policy: Action,
    /// Case policy: `portable` or `case-sensitive-only`.
    #[serde(default)]
    pub case_policy: CasePolicyConfig,
    /// Cache configuration.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Git integration configuration.
    #[serde(default)]
    pub git: GitConfig,
    /// Env sync configuration.
    #[serde(default)]
    pub env: EnvConfig,
    /// Structured rules.
    #[serde(default)]
    pub rules: Vec<RuleEntry>,
}

/// Case policy config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CasePolicyConfig {
    /// Portable (default).
    #[default]
    Portable,
    /// Case-sensitive only.
    CaseSensitiveOnly,
}

/// Cache configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Maximum cache size (e.g. `50GiB`).
    pub max_bytes: String,
    /// Minimum free disk space.
    pub min_free_bytes: String,
    /// Eviction policy.
    #[serde(default = "default_eviction")]
    pub eviction: String,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: "50GiB".to_owned(),
            min_free_bytes: "20GiB".to_owned(),
            eviction: "lru".to_owned(),
        }
    }
}

/// Git integration configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitConfig {
    /// Git mode: `ignore`, `aware`, or `experimental-sync-git-dir`.
    #[serde(default = "default_git_mode")]
    pub mode: String,
    /// Whether to auto-fetch.
    #[serde(default = "default_true")]
    pub auto_fetch: bool,
    /// Whether to auto-merge (must be false in MVP).
    #[serde(default)]
    pub auto_merge: bool,
    /// Whether to sync `.git` internals (must be false in MVP).
    #[serde(default)]
    pub sync_git_dir: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            mode: default_git_mode(),
            auto_fetch: true,
            auto_merge: false,
            sync_git_dir: false,
        }
    }
}

/// Env sync configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvConfig {
    /// Encryption mode.
    #[serde(default = "default_env_mode")]
    pub mode: String,
    /// Materialization mode: `never`, `on-command`, `on-mount`.
    #[serde(default = "default_materialize")]
    pub materialize: String,
    /// Materialized filename.
    #[serde(default = "default_materialize_filename")]
    pub materialize_filename: String,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            mode: default_env_mode(),
            materialize: default_materialize(),
            materialize_filename: default_materialize_filename(),
        }
    }
}

/// A structured rule entry from config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEntry {
    /// Glob pattern.
    pub pattern: String,
    /// Action name.
    pub action: Action,
    /// Optional package manager (for dependency-cache rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<String>,
    /// Optional scope (e.g. `project`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Error returned by [`Config::parse`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    /// TOML parse error.
    #[error("config parse error: {0}")]
    Toml(#[from] toml::de::Error),
    /// Validation error.
    #[error("config validation error: {0}")]
    Validation(String),
}

impl Config {
    /// Parse and validate a `.fs2/config.toml` string.
    ///
    /// # Errors
    /// Returns [`ConfigError`] if the TOML is invalid or validation fails
    /// (unknown action, invalid cache size, unsafe git mode).
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        // Validate cache size strings.
        validate_size_string(&self.cache.max_bytes).map_err(ConfigError::Validation)?;
        validate_size_string(&self.cache.min_free_bytes).map_err(ConfigError::Validation)?;
        // Validate eviction policy.
        if self.cache.eviction != "lru" {
            return Err(ConfigError::Validation(format!(
                "unsupported eviction policy: {} (only `lru` supported)",
                self.cache.eviction
            )));
        }
        // Validate git mode.
        match self.git.mode.as_str() {
            "ignore" | "aware" | "experimental-sync-git-dir" => {}
            other => {
                return Err(ConfigError::Validation(format!(
                    "unknown git mode: {other}"
                )));
            }
        }
        if self.git.sync_git_dir && self.git.mode != "experimental-sync-git-dir" {
            return Err(ConfigError::Validation(
                "sync_git_dir=true requires git mode `experimental-sync-git-dir`".to_owned(),
            ));
        }
        // Validate env mode.
        if self.env.mode != "encrypted" {
            return Err(ConfigError::Validation(format!(
                "unsupported env mode: {} (only `encrypted` supported)",
                self.env.mode
            )));
        }
        match self.env.materialize.as_str() {
            "never" | "on-command" | "on-mount" => {}
            other => {
                return Err(ConfigError::Validation(format!(
                    "unknown env materialize mode: {other}"
                )));
            }
        }
        // Validate rule actions are known (serde already checks, but double-check).
        for rule in &self.rules {
            // Action is parsed by serde; verify the pattern is non-empty.
            if rule.pattern.is_empty() {
                return Err(ConfigError::Validation(
                    "rule with empty pattern".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Validate a human-readable size string like `50GiB`, `20MiB`, `1000`.
fn validate_size_string(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("empty size string".to_owned());
    }
    // Allow plain integers.
    if s.chars().all(|c| c.is_ascii_digit()) {
        return Ok(());
    }
    // Allow <number><unit> where unit is KiB/MiB/GiB/TiB (case-sensitive IEC).
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    if num.is_empty() {
        return Err(format!("size string `{s}` has no numeric part"));
    }
    match unit {
        "KiB" | "MiB" | "GiB" | "TiB" | "KB" | "MB" | "GB" | "TB" => Ok(()),
        _ => Err(format!("size string `{s}` has unknown unit `{unit}`")),
    }
}

fn default_version() -> u32 {
    1
}
fn default_file_policy() -> Action {
    Action::Lazy
}
fn default_new_file_policy() -> Action {
    Action::Normal
}
fn default_eviction() -> String {
    "lru".to_owned()
}
fn default_git_mode() -> String {
    "aware".to_owned()
}
fn default_env_mode() -> String {
    "encrypted".to_owned()
}
fn default_materialize() -> String {
    "on-command".to_owned()
}
fn default_materialize_filename() -> String {
    ".env.fs2".to_owned()
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
version = 1
workspace_name = "personal-code"
default_file_policy = "lazy"
default_new_file_policy = "normal"
case_policy = "portable"

[cache]
max_bytes = "50GiB"
min_free_bytes = "20GiB"
eviction = "lru"

[git]
mode = "aware"
auto_fetch = true
auto_merge = false
sync_git_dir = false

[env]
mode = "encrypted"
materialize = "on-command"
materialize_filename = ".env.fs2"

[[rules]]
pattern = "node_modules/**"
action = "dependency-cache"
manager = "node"

[[rules]]
pattern = "apps/*/.env"
action = "secret"
scope = "project"

[[rules]]
pattern = "datasets/**"
action = "lazy"
"#;
        let config = Config::parse(toml).unwrap();
        assert_eq!(config.workspace_name, "personal-code");
        assert_eq!(config.default_file_policy, Action::Lazy);
        assert_eq!(config.rules.len(), 3);
        assert_eq!(config.rules[0].action, Action::DependencyCache);
        assert_eq!(config.rules[0].manager.as_deref(), Some("node"));
        assert_eq!(config.rules[1].action, Action::Secret);
        assert_eq!(config.rules[2].action, Action::Lazy);
    }

    #[test]
    fn defaults_applied() {
        let toml = r#"
workspace_name = "test"
"#;
        let config = Config::parse(toml).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.default_file_policy, Action::Lazy);
        assert_eq!(config.cache.max_bytes, "50GiB");
        assert_eq!(config.git.mode, "aware");
        assert_eq!(config.env.mode, "encrypted");
    }

    #[test]
    fn invalid_cache_size_rejected() {
        let toml = r#"
workspace_name = "test"
[cache]
max_bytes = "bogus"
min_free_bytes = "20GiB"
"#;
        let err = Config::parse(toml).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn unsafe_git_mode_rejected() {
        let toml = r#"
workspace_name = "test"
[git]
mode = "aware"
sync_git_dir = true
"#;
        let err = Config::parse(toml).unwrap_err();
        assert!(err.to_string().contains("sync_git_dir"));
    }

    #[test]
    fn unknown_action_rejected() {
        let toml = r#"
workspace_name = "test"
[[rules]]
pattern = "x"
action = "bogus"
"#;
        let err = Config::parse(toml);
        assert!(err.is_err());
    }

    #[test]
    fn size_string_units() {
        validate_size_string("50GiB").unwrap();
        validate_size_string("1000").unwrap();
        validate_size_string("20MiB").unwrap();
        assert!(validate_size_string("").is_err());
        assert!(validate_size_string("GiB").is_err());
        assert!(validate_size_string("50Foo").is_err());
    }
}
