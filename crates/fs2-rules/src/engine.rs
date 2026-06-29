//! Rule engine: combines built-in profiles, `.fs2ignore`, `.fs2/config.toml`,
//! and CLI overrides to answer "what action applies to this path?"
//!
//! Precedence (highest wins), per `design.md` §8.4:
//!
//! 1. Explicit CLI override for the exact path.
//! 2. `.fs2/config.toml` rule with the most specific matching pattern.
//! 3. `.fs2ignore` rule with the last matching line (gitignore-style).
//! 4. Built-in default profiles (last match wins among profile entries).
//! 5. Workspace default (`default_file_policy` from config).

use std::fmt;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::action::Action;
use crate::config::{ConfigRule, Fs2Config};
use crate::fs2ignore::{Fs2IgnoreEntry, Fs2IgnoreFile};
use crate::profiles::builtin_entries;

/// Where a rule came from. Used for diagnostics and `fs2 rules list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// Built-in default profile.
    Builtin,
    /// `.fs2ignore` file.
    Fs2Ignore,
    /// `.fs2/config.toml` structured rule.
    Config,
    /// Explicit CLI override.
    Cli,
    /// Workspace default fallback.
    Default,
}

impl fmt::Display for RuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Builtin => "builtin",
            Self::Fs2Ignore => ".fs2ignore",
            Self::Config => ".fs2/config.toml",
            Self::Cli => "cli",
            Self::Default => "default",
        })
    }
}

/// The result of evaluating rules for a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRule {
    /// The action that applies.
    pub action: Action,
    /// Where the action came from.
    pub source: RuleSource,
    /// The pattern that matched, or `None` for the workspace default.
    pub pattern: Option<String>,
    /// Human-readable explanation of why this action was chosen.
    pub explanation: String,
}

/// The rule engine. Built once from config + ignore file + built-ins, then
/// queried for each path.
#[derive(Debug)]
pub struct RuleEngine {
    config_rules: Vec<CompiledConfigRule>,
    ignore_entries: Vec<Fs2IgnoreEntry>,
    builtin_entries: Vec<Fs2IgnoreEntry>,
    cli_overrides: Vec<CompiledConfigRule>,
    default_file_policy: Action,
}

#[derive(Debug)]
struct CompiledConfigRule {
    pattern: String,
    action: Action,
    glob: GlobSet,
    /// Number of path segments in the pattern, used as a specificity score.
    specificity: usize,
}

impl CompiledConfigRule {
    fn new(rule: &ConfigRule) -> Result<Self, globset::Error> {
        let glob = Glob::new(&rule.pattern)?;
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let glob = builder.build()?;
        let specificity = rule.pattern.split('/').count();
        Ok(Self {
            pattern: rule.pattern.clone(),
            action: rule.action,
            glob,
            specificity,
        })
    }
}

impl RuleEngine {
    /// Build a rule engine from a parsed config, parsed ignore file, and the
    /// built-in profiles.
    ///
    /// # Errors
    /// Returns [`globset::Error`] if any glob pattern is invalid.
    pub fn new(config: &Fs2Config, ignore: &Fs2IgnoreFile) -> Result<Self, globset::Error> {
        let config_rules = config
            .rules
            .iter()
            .map(CompiledConfigRule::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            config_rules,
            ignore_entries: ignore.entries.clone(),
            builtin_entries: builtin_entries(),
            cli_overrides: Vec::new(),
            default_file_policy: config.default_file_policy,
        })
    }

    /// Build a rule engine with only built-in profiles and a default policy.
    ///
    /// Useful for tests and for workspaces without a config file yet.
    /// # Errors
    /// Returns [`globset::Error`] if any built-in pattern is invalid.
    pub fn with_builtins(default_file_policy: Action) -> Result<Self, globset::Error> {
        let config = Fs2Config {
            default_file_policy,
            ..Fs2Config::default()
        };
        Self::new(&config, &Fs2IgnoreFile::default())
    }

    /// Add an explicit CLI override for a pattern.
    ///
    /// CLI overrides take the highest precedence.
    /// # Errors
    /// Returns [`globset::Error`] if the pattern is invalid.
    pub fn add_cli_override(
        &mut self,
        pattern: &str,
        action: Action,
    ) -> Result<(), globset::Error> {
        let rule = ConfigRule::new(pattern, action);
        let compiled = CompiledConfigRule::new(&rule)?;
        self.cli_overrides.push(compiled);
        Ok(())
    }

    /// Evaluate the effective rule for a workspace-relative path.
    #[must_use]
    pub fn evaluate(&self, path: &str) -> EffectiveRule {
        // 1. CLI override (highest precedence).
        if let Some(rule) = self.match_cli_override(path) {
            return EffectiveRule {
                action: rule.action,
                source: RuleSource::Cli,
                pattern: Some(rule.pattern.clone()),
                explanation: format!("CLI override '{}' matched path '{}'", rule.pattern, path),
            };
        }

        // 2. .fs2/config.toml most-specific rule.
        if let Some(rule) = self.match_config_rule(path) {
            return EffectiveRule {
                action: rule.action,
                source: RuleSource::Config,
                pattern: Some(rule.pattern.clone()),
                explanation: format!(
                    "config rule '{}' matched path '{}' (most specific)",
                    rule.pattern, path
                ),
            };
        }

        // 3. .fs2ignore last match.
        if let Some(entry) = self.match_ignore_entry(path) {
            return EffectiveRule {
                action: entry.action,
                source: RuleSource::Fs2Ignore,
                pattern: Some(entry.pattern.clone()),
                explanation: format!(
                    ".fs2ignore line {} '{}' matched path '{}' -> {} (last match wins)",
                    entry.line, entry.pattern, path, entry.action
                ),
            };
        }

        // 4. Built-in profiles (last match wins).
        if let Some(entry) = self.match_builtin_entry(path) {
            return EffectiveRule {
                action: entry.action,
                source: RuleSource::Builtin,
                pattern: Some(entry.pattern.clone()),
                explanation: format!(
                    "built-in profile rule '{}' matched path '{}'",
                    entry.pattern, path
                ),
            };
        }

        // 5. Workspace default.
        EffectiveRule {
            action: self.default_file_policy,
            source: RuleSource::Default,
            pattern: None,
            explanation: format!(
                "workspace default policy '{}' applied to path '{}'",
                self.default_file_policy, path
            ),
        }
    }

    fn match_cli_override(&self, path: &str) -> Option<&CompiledConfigRule> {
        self.cli_overrides.iter().find(|r| r.glob.is_match(path))
    }

    fn match_config_rule(&self, path: &str) -> Option<&CompiledConfigRule> {
        // Most-specific pattern wins: highest specificity among matching rules.
        self.config_rules
            .iter()
            .filter(|r| r.glob.is_match(path))
            .max_by_key(|r| r.specificity)
    }

    fn match_ignore_entry(&self, path: &str) -> Option<&Fs2IgnoreEntry> {
        // Last match wins: iterate in order, keep the last matching entry.
        let mut last: Option<&Fs2IgnoreEntry> = None;
        for entry in &self.ignore_entries {
            if glob_matches(&entry.pattern, path) {
                last = Some(entry);
            }
        }
        last
    }

    fn match_builtin_entry(&self, path: &str) -> Option<&Fs2IgnoreEntry> {
        let mut last: Option<&Fs2IgnoreEntry> = None;
        for entry in &self.builtin_entries {
            if glob_matches(&entry.pattern, path) {
                last = Some(entry);
            }
        }
        last
    }
}

/// Match a gitignore-style glob pattern against a path.
///
/// This uses `globset` with a per-call matcher. For hot paths the engine
/// pre-compiles config rules; this helper is used for ignore/builtin entries
/// where patterns are simple and the cost of building a `GlobSet` per entry
/// is acceptable.
fn glob_matches(pattern: &str, path: &str) -> bool {
    // gitignore-style: a pattern ending in `/` matches directories and their
    // contents. We approximate by matching the pattern as a glob and also
    // matching `pattern + **`.
    if let Ok(glob) = Glob::new(pattern) {
        let matcher = glob.compile_matcher();
        if matcher.is_match(path) {
            return true;
        }
    }
    // Also try the pattern as a prefix for directory-style matches.
    let dir_pattern = if pattern.ends_with('/') {
        format!("{pattern}**")
    } else {
        format!("{pattern}/**")
    };
    if let Ok(glob) = Glob::new(&dir_pattern) {
        let matcher = glob.compile_matcher();
        if matcher.is_match(path) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs2ignore::Fs2IgnoreFile;

    fn engine_with_ignore(ignore_text: &str) -> RuleEngine {
        let ignore = Fs2IgnoreFile::parse(ignore_text).unwrap();
        RuleEngine::new(&Fs2Config::default(), &ignore).unwrap()
    }

    #[test]
    fn default_policy_applies_when_nothing_matches() {
        let engine = RuleEngine::with_builtins(Action::Lazy).unwrap();
        let r = engine.evaluate("apps/web/src/index.ts");
        assert_eq!(r.action, Action::Lazy);
        assert_eq!(r.source, RuleSource::Default);
    }

    #[test]
    fn builtin_node_modules_is_dependency_cache() {
        let engine = RuleEngine::with_builtins(Action::Normal).unwrap();
        let r = engine.evaluate("node_modules/pkg/index.js");
        assert_eq!(r.action, Action::DependencyCache);
        assert_eq!(r.source, RuleSource::Builtin);
    }

    #[test]
    fn builtin_target_is_generated() {
        let engine = RuleEngine::with_builtins(Action::Normal).unwrap();
        let r = engine.evaluate("target/debug/app");
        assert_eq!(r.action, Action::Generated);
    }

    #[test]
    fn ignore_file_overrides_builtin() {
        let engine = engine_with_ignore(":normal node_modules/\n");
        let r = engine.evaluate("node_modules/pkg/index.js");
        assert_eq!(r.action, Action::Normal);
        assert_eq!(r.source, RuleSource::Fs2Ignore);
    }

    #[test]
    fn ignore_last_match_wins() {
        let engine = engine_with_ignore(":normal foo\n:ignore foo\n");
        let r = engine.evaluate("foo");
        assert_eq!(r.action, Action::Ignore);
    }

    #[test]
    fn config_rule_overrides_ignore() {
        let mut config = Fs2Config::default();
        config
            .rules
            .push(ConfigRule::new("node_modules/**", Action::Normal));
        let ignore = Fs2IgnoreFile::parse(":generated node_modules/").unwrap();
        let engine = RuleEngine::new(&config, &ignore).unwrap();
        let r = engine.evaluate("node_modules/pkg/index.js");
        assert_eq!(r.action, Action::Normal);
        assert_eq!(r.source, RuleSource::Config);
    }

    #[test]
    fn cli_override_takes_precedence() {
        let mut engine = RuleEngine::with_builtins(Action::Normal).unwrap();
        engine
            .add_cli_override("node_modules/**", Action::Pin)
            .unwrap();
        let r = engine.evaluate("node_modules/pkg/index.js");
        assert_eq!(r.action, Action::Pin);
        assert_eq!(r.source, RuleSource::Cli);
    }

    #[test]
    fn config_most_specific_wins() {
        let mut config = Fs2Config::default();
        config.rules.push(ConfigRule::new("apps/**", Action::Lazy));
        config
            .rules
            .push(ConfigRule::new("apps/web/**", Action::Pin));
        let engine = RuleEngine::new(&config, &Fs2IgnoreFile::default()).unwrap();
        let r = engine.evaluate("apps/web/index.ts");
        assert_eq!(r.action, Action::Pin);
        let r2 = engine.evaluate("apps/mobile/index.ts");
        assert_eq!(r2.action, Action::Lazy);
    }

    #[test]
    fn node_modules_does_not_enqueue_sync_under_default_profile() {
        // This is the acceptance criterion from todo 4.4.
        let engine = RuleEngine::with_builtins(Action::Normal).unwrap();
        let r = engine.evaluate("node_modules/react/index.js");
        assert!(r.action.suppresses_upload());
        assert!(r.action.suppresses_download());
    }

    #[test]
    fn package_json_is_pinned_by_builtin() {
        let engine = RuleEngine::with_builtins(Action::Normal).unwrap();
        let r = engine.evaluate("package-lock.json");
        assert_eq!(r.action, Action::Pin);
    }

    #[test]
    fn explanation_is_human_readable() {
        let engine = engine_with_ignore(":generated node_modules/\n");
        let r = engine.evaluate("node_modules/pkg/index.js");
        assert!(r.explanation.contains("node_modules"));
        assert!(r.explanation.contains("generated"));
    }
}
