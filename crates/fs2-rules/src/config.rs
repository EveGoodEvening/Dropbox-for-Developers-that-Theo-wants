//! `.fs2/config.toml` parser.
//!
//! Structured workspace configuration. See `design.md` §8.3 for the schema.

use std::fmt;
use std::str::FromStr;

use fs2_core::CasePolicy;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::action::Action;

/// Error returned when config parsing or validation fails.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// TOML syntax error.
    #[error("config parse error: {0}")]
    Toml(#[from] toml::de::Error),
    /// A field value was invalid (unknown action, bad cache size, etc.).
    #[error("config validation error: {0}")]
    Validation(String),
}

/// Top-level `.fs2/config.toml` structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fs2Config {
    /// Schema version. Must be `1` for the current implementation.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Workspace name.
    pub workspace_name: Option<String>,
    /// Default policy for existing files.
    #[serde(default)]
    pub default_file_policy: Action,
    /// Default policy for newly created files.
    #[serde(default)]
    pub default_new_file_policy: Action,
    /// Case-sensitivity policy.
    #[serde(default)]
    pub case_policy: CasePolicy,
    /// Cache configuration.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Git integration configuration.
    #[serde(default)]
    pub git: GitConfig,
    /// Env var sync configuration.
    #[serde(default)]
    pub env: EnvConfig,
    /// Structured rules. Most-specific pattern wins.
    #[serde(default)]
    pub rules: Vec<ConfigRule>,
}

fn default_version() -> u32 {
    1
}

impl Default for Fs2Config {
    fn default() -> Self {
        Self {
            version: 1,
            workspace_name: None,
            default_file_policy: Action::Lazy,
            default_new_file_policy: Action::Normal,
            case_policy: CasePolicy::Portable,
            cache: CacheConfig::default(),
            git: GitConfig::default(),
            env: EnvConfig::default(),
            rules: Vec::new(),
        }
    }
}

impl Fs2Config {
    /// Parse a `.fs2/config.toml` file from its text content.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for TOML syntax errors or invalid field values.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate field values after parsing.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::Validation(format!(
                "unsupported config version: {} (expected 1)",
                self.version
            )));
        }
        validate_cache_size(&self.cache.max_bytes, "cache.max_bytes")?;
        validate_cache_size(&self.cache.min_free_bytes, "cache.min_free_bytes")?;
        if self.cache.eviction != "lru" {
            return Err(ConfigError::Validation(format!(
                "unsupported eviction policy: {:?} (only \"lru\" is supported)",
                self.cache.eviction
            )));
        }
        for rule in &self.rules {
            if rule.pattern.is_empty() {
                return Err(ConfigError::Validation(
                    "config rule with empty pattern".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Cache configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Maximum cache size, e.g. `"50GiB"`.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: String,
    /// Minimum free disk space, e.g. `"20GiB"`.
    #[serde(default = "default_min_free_bytes")]
    pub min_free_bytes: String,
    /// Eviction strategy. Only `"lru"` is supported in MVP.
    #[serde(default = "default_eviction")]
    pub eviction: String,
}

fn default_max_bytes() -> String {
    "50GiB".to_owned()
}
fn default_min_free_bytes() -> String {
    "20GiB".to_owned()
}
fn default_eviction() -> String {
    "lru".to_owned()
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_max_bytes(),
            min_free_bytes: default_min_free_bytes(),
            eviction: default_eviction(),
        }
    }
}

/// Git integration mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GitMode {
    /// Do not detect or integrate with Git.
    Ignore,
    /// Detect Git repos, record metadata, exclude `.git` internals.
    #[default]
    Aware,
    /// Experimental: sync `.git` directory (not for MVP).
    ExperimentalSyncGitDir,
}

/// Git integration configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitConfig {
    /// Git mode.
    #[serde(default)]
    pub mode: GitMode,
    /// Whether to auto-fetch on machines with an existing clone.
    #[serde(default = "default_true")]
    pub auto_fetch: bool,
    /// Whether to auto-merge. Always false in MVP.
    #[serde(default)]
    pub auto_merge: bool,
    /// Whether to sync `.git` internals. Always false in MVP default.
    #[serde(default)]
    pub sync_git_dir: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            mode: GitMode::Aware,
            auto_fetch: true,
            auto_merge: false,
            sync_git_dir: false,
        }
    }
}

/// Env var sync materialization mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EnvMaterializeMode {
    /// Never materialize; inject only via `fs2 env exec`.
    Never,
    /// Materialize on explicit command.
    #[default]
    OnCommand,
    /// Materialize at mount time.
    OnMount,
}

/// Env var sync configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvConfig {
    /// Encryption mode. Only `"encrypted"` is supported in MVP.
    #[serde(default = "default_encrypted")]
    pub mode: String,
    /// When to materialize the env file.
    #[serde(default)]
    pub materialize: EnvMaterializeMode,
    /// Materialized filename.
    #[serde(default = "default_materialize_filename")]
    pub materialize_filename: String,
}

fn default_encrypted() -> String {
    "encrypted".to_owned()
}
fn default_materialize_filename() -> String {
    ".env.fs2".to_owned()
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            mode: default_encrypted(),
            materialize: EnvMaterializeMode::OnCommand,
            materialize_filename: default_materialize_filename(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Validate a cache size string like `"50GiB"`, `"20MiB"`, `"100KB"`, `"1024"`.
///
/// Accepted format: `<positive integer><optional unit>` where unit is one of
/// `B`, `KB`, `MB`, `GB`, `TiB`, `GiB`, `MiB`, `KiB` (case-sensitive for the
/// `iB` binary forms, case-insensitive for the decimal `KB`/`MB`/`GB` forms).
///
/// # Errors
/// Returns [`ConfigError::Validation`] if the string is not a valid size.
fn validate_cache_size(s: &str, field: &str) -> Result<(), ConfigError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{field}: empty size string"
        )));
    }
    // Find where the digits end.
    let digit_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digit_end == 0 {
        return Err(ConfigError::Validation(format!(
            "{field}: size must start with a positive integer: {s:?}"
        )));
    }
    let unit = &s[digit_end..];
    if unit.is_empty() {
        return Ok(()); // bare byte count
    }
    let valid_units = ["B", "KB", "MB", "GB", "KiB", "MiB", "GiB", "TiB"];
    if !valid_units.contains(&unit) {
        return Err(ConfigError::Validation(format!(
            "{field}: unknown size unit {unit:?} (valid: {valid_units:?})"
        )));
    }
    Ok(())
}

/// A structured rule from `.fs2/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRule {
    /// Glob pattern.
    pub pattern: String,
    /// Action to apply.
    #[serde(default)]
    pub action: Action,
    /// Optional package manager for `dependency-cache` rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<String>,
    /// Optional scope for `secret` rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl ConfigRule {
    /// Create a simple pattern+action rule.
    #[must_use]
    pub fn new(pattern: impl Into<String>, action: Action) -> Self {
        Self {
            pattern: pattern.into(),
            action,
            manager: None,
            scope: None,
        }
    }
}

impl FromStr for Fs2Config {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for Fs2Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = toml::to_string(self).map_err(|_| fmt::Error)?;
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = Fs2Config::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.default_file_policy, Action::Lazy);
        assert_eq!(cfg.case_policy, CasePolicy::Portable);
        assert_eq!(cfg.git.mode, GitMode::Aware);
        assert!(!cfg.git.sync_git_dir);
        assert_eq!(cfg.env.materialize, EnvMaterializeMode::OnCommand);
    }

    #[test]
    fn parse_full_config() {
        let text = r#"
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
        let cfg = Fs2Config::parse(text).unwrap();
        assert_eq!(cfg.workspace_name.as_deref(), Some("personal-code"));
        assert_eq!(cfg.rules.len(), 3);
        assert_eq!(cfg.rules[0].action, Action::DependencyCache);
        assert_eq!(cfg.rules[0].manager.as_deref(), Some("node"));
        assert_eq!(cfg.rules[1].action, Action::Secret);
        assert_eq!(cfg.rules[1].scope.as_deref(), Some("project"));
        assert_eq!(cfg.rules[2].action, Action::Lazy);
    }

    #[test]
    fn invalid_version_rejected() {
        let text = "version = 2\n";
        let err = Fs2Config::parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn empty_pattern_rejected() {
        let text = "[[rules]]\npattern = \"\"\naction = \"ignore\"\n";
        let err = Fs2Config::parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn unknown_action_rejected() {
        let text = "[[rules]]\npattern = \"x\"\naction = \"bogus\"\n";
        assert!(Fs2Config::parse(text).is_err());
    }

    #[test]
    fn git_mode_kebab_case() {
        let text = "[git]\nmode = \"experimental-sync-git-dir\"\n";
        let cfg = Fs2Config::parse(text).unwrap();
        assert_eq!(cfg.git.mode, GitMode::ExperimentalSyncGitDir);
    }

    #[test]
    fn env_materialize_modes() {
        for (s, expected) in [
            ("never", EnvMaterializeMode::Never),
            ("on-command", EnvMaterializeMode::OnCommand),
            ("on-mount", EnvMaterializeMode::OnMount),
        ] {
            let text = format!("[env]\nmaterialize = \"{s}\"\n");
            let cfg = Fs2Config::parse(&text).unwrap();
            assert_eq!(cfg.env.materialize, expected);
        }
    }

    #[test]
    fn defaults_applied_for_missing_sections() {
        let cfg = Fs2Config::parse("version = 1\n").unwrap();
        assert_eq!(cfg.cache.max_bytes, "50GiB");
        assert_eq!(cfg.git.mode, GitMode::Aware);
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn valid_cache_sizes_accepted() {
        for size in ["50GiB", "20GiB", "100MB", "1024", "1TiB", "500KB"] {
            let text = format!("[cache]\nmax_bytes = \"{size}\"\n");
            Fs2Config::parse(&text).unwrap_or_else(|e| panic!("size {size} should be valid: {e}"));
        }
    }

    #[test]
    fn invalid_cache_size_rejected() {
        let text = "[cache]\nmax_bytes = \"abc\"\n";
        assert!(Fs2Config::parse(text).is_err());
        let text = "[cache]\nmax_bytes = \"50XB\"\n";
        assert!(Fs2Config::parse(text).is_err());
        let text = "[cache]\nmax_bytes = \"\"\n";
        assert!(Fs2Config::parse(text).is_err());
    }

    #[test]
    fn invalid_eviction_policy_rejected() {
        let text = "[cache]\neviction = \"fifo\"\n";
        assert!(Fs2Config::parse(text).is_err());
    }

    #[test]
    fn full_config_snapshot() {
        let text = r#"
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
pattern = "datasets/**"
action = "lazy"
"#;
        let cfg = Fs2Config::parse(text).unwrap();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        insta::assert_json_snapshot!(json);
    }

    #[test]
    fn default_config_snapshot() {
        let cfg = Fs2Config::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        insta::assert_json_snapshot!(json);
    }
}
