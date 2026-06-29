//! Rule precedence engine.
//!
//! Precedence order (highest to lowest):
//! 1. Explicit CLI override for a path.
//! 2. `.fs2/config.toml` rule with the most specific pattern.
//! 3. `.fs2ignore` rule with the last matching line (gitignore-style).
//! 4. Built-in profile rule.
//! 5. Workspace default.
//!
//! "Most specific" for config rules means the pattern with the longest match
//! (most path segments consumed). Ties are broken by insertion order (last
//! wins), matching the config file's bottom-to-top read order.

use std::fmt;

use crate::action::Action;
use crate::fs2ignore::Fs2IgnoreEntry;
use crate::glob::Glob;

/// Where a rule came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSource {
    /// Explicit CLI override.
    Cli,
    /// `.fs2/config.toml` structured rule.
    Config,
    /// `.fs2ignore` line.
    Fs2Ignore,
    /// Built-in language/tool profile.
    Profile(String),
    /// Built-in git-aware rule (`.git/**` ignored).
    BuiltinGit,
    /// Workspace default policy.
    Default,
}

impl fmt::Display for RuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli => f.write_str("cli"),
            Self::Config => f.write_str("config"),
            Self::Fs2Ignore => f.write_str("fs2ignore"),
            Self::Profile(p) => write!(f, "profile:{p}"),
            Self::BuiltinGit => f.write_str("builtin:git"),
            Self::Default => f.write_str("default"),
        }
    }
}

/// A rule entry used by the engine, independent of its source format.
#[derive(Debug, Clone)]
pub struct RuleEntry {
    /// Glob pattern.
    pub pattern: String,
    /// Action.
    pub action: Action,
    /// Source of this rule.
    pub source: RuleSource,
}

/// The effective rule for a path, with an explanation of why it applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRule {
    /// The resolved action.
    pub action: Action,
    /// The pattern that matched, or `default` if no rule matched.
    pub pattern: String,
    /// Where the rule came from.
    pub source: RuleSource,
}

/// The rule engine. Combines built-in profiles, config rules, fs2ignore rules,
/// and CLI overrides to resolve the effective action for a path.
#[derive(Debug, Clone)]
pub struct RuleEngine {
    builtin: Vec<RuleEntry>,
    config_rules: Vec<RuleEntry>,
    ignore_rules: Vec<Fs2IgnoreEntry>,
    cli_overrides: Vec<RuleEntry>,
    default: Action,
}

impl RuleEngine {
    /// Create a new engine with built-in profile rules and a workspace default.
    #[must_use]
    pub fn new(builtin: Vec<RuleEntry>, default: Action) -> Self {
        Self {
            builtin,
            config_rules: Vec::new(),
            ignore_rules: Vec::new(),
            cli_overrides: Vec::new(),
            default,
        }
    }

    /// Add `.fs2/config.toml` structured rules.
    #[must_use]
    pub fn with_config_rules(mut self, rules: Vec<RuleEntry>) -> Self {
        self.config_rules = rules;
        self
    }

    /// Add `.fs2ignore` parsed entries.
    #[must_use]
    pub fn with_ignore_rules(mut self, entries: Vec<Fs2IgnoreEntry>) -> Self {
        self.ignore_rules = entries;
        self
    }

    /// Add CLI override rules.
    #[must_use]
    pub fn with_cli_overrides(mut self, rules: Vec<RuleEntry>) -> Self {
        self.cli_overrides = rules;
        self
    }

    /// Resolve the effective rule for a workspace-relative path.
    ///
    /// Precedence: CLI > config (most specific) > fs2ignore (last match) >
    /// built-in profile > default.
    #[must_use]
    pub fn resolve(&self, path: &str) -> EffectiveRule {
        // 1. CLI overrides (first match, highest priority).
        for entry in &self.cli_overrides {
            if matches_pattern(&entry.pattern, path) {
                return EffectiveRule {
                    action: entry.action,
                    pattern: entry.pattern.clone(),
                    source: entry.source.clone(),
                };
            }
        }
        // 2. Config rules — most specific (longest pattern) wins; ties go to
        //    last insertion.
        if let Some(best) = most_specific_config_match(&self.config_rules, path) {
            return EffectiveRule {
                action: best.action,
                pattern: best.pattern.clone(),
                source: best.source.clone(),
            };
        }
        // 3. fs2ignore — last match wins (gitignore-style).
        for entry in self.ignore_rules.iter().rev() {
            if entry.glob.matches(path) {
                return EffectiveRule {
                    action: entry.action,
                    pattern: entry.pattern.clone(),
                    source: RuleSource::Fs2Ignore,
                };
            }
        }
        // 4. Built-in profiles — most specific wins.
        if let Some(best) = most_specific_match(&self.builtin, path) {
            return EffectiveRule {
                action: best.action,
                pattern: best.pattern.clone(),
                source: best.source.clone(),
            };
        }
        // 5. Workspace default.
        EffectiveRule {
            action: self.default,
            pattern: "<default>".to_owned(),
            source: RuleSource::Default,
        }
    }
}

fn matches_pattern(pattern: &str, path: &str) -> bool {
    // Compile on the fly; in hot paths this should be cached.
    match Glob::compile(pattern) {
        Ok(g) => g.matches(path),
        Err(_) => false,
    }
}

/// Find the most specific matching rule by longest pattern (segment count).
fn most_specific_match<'a>(rules: &'a [RuleEntry], path: &str) -> Option<&'a RuleEntry> {
    let mut best: Option<&RuleEntry> = None;
    let mut best_score: usize = 0;
    for entry in rules {
        if matches_pattern(&entry.pattern, path) {
            let score = specificity(&entry.pattern);
            if score >= best_score {
                best_score = score;
                best = Some(entry);
            }
        }
    }
    best
}

/// Config rules use the same specificity logic but are checked separately so
/// the source is `Config`.
fn most_specific_config_match<'a>(rules: &'a [RuleEntry], path: &str) -> Option<&'a RuleEntry> {
    most_specific_match(rules, path)
}

/// Specificity: count the number of non-wildcard literal segments, then total
/// segments. More literal segments = more specific.
fn specificity(pattern: &str) -> usize {
    let p = pattern.trim_start_matches('/');
    if p.is_empty() {
        return 0;
    }
    p.split('/').filter(|s| !s.is_empty()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs2ignore::Fs2IgnoreParser;
    use crate::profiles::builtin_profiles;

    #[test]
    fn default_when_no_rules() {
        let engine = RuleEngine::new(vec![], Action::Normal);
        let rule = engine.resolve("anything");
        assert_eq!(rule.action, Action::Normal);
        assert_eq!(rule.source, RuleSource::Default);
    }

    #[test]
    fn config_overrides_profile() {
        let engine =
            RuleEngine::new(builtin_profiles(), Action::Lazy).with_config_rules(vec![RuleEntry {
                pattern: "node_modules/**".to_owned(),
                action: Action::Ignore,
                source: RuleSource::Config,
            }]);
        let rule = engine.resolve("node_modules/react/index.js");
        assert_eq!(rule.action, Action::Ignore);
        assert_eq!(rule.source, RuleSource::Config);
    }

    #[test]
    fn fs2ignore_overrides_profile() {
        let ignore = Fs2IgnoreParser.parse(":ignore node_modules/\n").unwrap();
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy).with_ignore_rules(ignore);
        let rule = engine.resolve("node_modules/react/index.js");
        assert_eq!(rule.action, Action::Ignore);
        assert_eq!(rule.source, RuleSource::Fs2Ignore);
    }

    #[test]
    fn cli_overrides_all() {
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy)
            .with_config_rules(vec![RuleEntry {
                pattern: "node_modules/**".to_owned(),
                action: Action::Ignore,
                source: RuleSource::Config,
            }])
            .with_cli_overrides(vec![RuleEntry {
                pattern: "node_modules/**".to_owned(),
                action: Action::Pin,
                source: RuleSource::Cli,
            }]);
        let rule = engine.resolve("node_modules/react/index.js");
        assert_eq!(rule.action, Action::Pin);
        assert_eq!(rule.source, RuleSource::Cli);
    }

    #[test]
    fn most_specific_config_wins() {
        let engine = RuleEngine::new(vec![], Action::Normal).with_config_rules(vec![
            RuleEntry {
                pattern: "**".to_owned(),
                action: Action::Lazy,
                source: RuleSource::Config,
            },
            RuleEntry {
                pattern: "apps/**".to_owned(),
                action: Action::Pin,
                source: RuleSource::Config,
            },
        ]);
        let rule = engine.resolve("apps/web/file.ts");
        assert_eq!(rule.action, Action::Pin);
    }

    #[test]
    fn fs2ignore_last_match_wins() {
        let ignore = Fs2IgnoreParser
            .parse(":lazy fixtures/**\n:pin fixtures/**\n")
            .unwrap();
        let engine = RuleEngine::new(vec![], Action::Normal).with_ignore_rules(ignore);
        let rule = engine.resolve("fixtures/data.json");
        assert_eq!(rule.action, Action::Pin);
    }

    #[test]
    fn creating_node_modules_does_not_enqueue_sync() {
        // The acceptance criterion: node_modules under default Node profile
        // has an action that suppresses upload.
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy);
        let rule = engine.resolve("apps/web/node_modules/react/index.js");
        assert!(rule.action.suppresses_upload());
    }
}
