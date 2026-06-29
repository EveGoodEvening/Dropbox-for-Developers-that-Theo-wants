#![allow(
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::struct_field_names,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments
)]

//! Rule parsing, configuration, built-in profiles, and effective rule resolution.

use fs2_core::{CasePolicy, FsRule, RuleAction, WorkspacePath};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, fmt, path::PathBuf, str::FromStr};

/// Returns the crate name for smoke tests and early workspace validation.
pub const fn crate_name() -> &'static str {
    "fs2-rules"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    pub location: String,
    pub message: String,
}

impl RuleError {
    fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.location, self.message)
    }
}

impl std::error::Error for RuleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RulePathKind {
    File,
    Directory,
    Symlink,
    Unknown,
}

impl RulePathKind {
    const fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvaluationPurpose {
    ExistingOrRemoteLookup,
    NewLocalCreate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuiltinProfile {
    Git,
    Node,
    Rust,
    Python,
    Go,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvictionPolicy {
    #[default]
    Lru,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitMode {
    Ignore,
    #[default]
    Aware,
    ExperimentalSyncGitDir,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvMode {
    #[default]
    Encrypted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializeMode {
    Never,
    #[default]
    OnCommand,
    OnMount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ByteSize(pub u64);

impl FromStr for ByteSize {
    type Err = RuleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_byte_size(value).map(ByteSize)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    pub max_bytes: Option<ByteSize>,
    pub min_free_bytes: Option<ByteSize>,
    #[serde(default)]
    pub eviction: EvictionPolicy,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: None,
            min_free_bytes: None,
            eviction: EvictionPolicy::Lru,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    #[serde(default)]
    pub mode: GitMode,
    #[serde(default = "default_true")]
    pub auto_fetch: bool,
    #[serde(default)]
    pub auto_merge: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    #[serde(default)]
    pub mode: EnvMode,
    #[serde(default)]
    pub materialize: MaterializeMode,
    #[serde(default = "default_materialize_filename")]
    pub materialize_filename: String,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            mode: EnvMode::Encrypted,
            materialize: MaterializeMode::OnCommand,
            materialize_filename: default_materialize_filename(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesConfig {
    #[serde(default = "default_true")]
    pub node: bool,
    #[serde(default = "default_true")]
    pub rust: bool,
    #[serde(default = "default_true")]
    pub python: bool,
    #[serde(default = "default_true")]
    pub go: bool,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            node: true,
            rust: true,
            python: true,
            go: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRule {
    pub pattern: String,
    pub action: RuleAction,
    pub manager: Option<String>,
    pub scope: Option<String>,
}

impl ConfigRule {
    fn fs_rule(&self) -> FsRule {
        FsRule {
            action: self.action,
            manager: self.manager.clone(),
            scope: self.scope.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u16,
    pub workspace_name: Option<String>,
    #[serde(default = "default_file_policy")]
    pub default_file_policy: RuleAction,
    #[serde(default = "default_new_file_policy")]
    pub default_new_file_policy: RuleAction,
    #[serde(default = "default_case_policy")]
    pub case_policy: CasePolicy,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub env: EnvConfig,
    #[serde(default)]
    pub profiles: ProfilesConfig,
    #[serde(default)]
    pub rules: Vec<ConfigRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            workspace_name: None,
            default_file_policy: RuleAction::Lazy,
            default_new_file_policy: RuleAction::Normal,
            case_policy: CasePolicy::Portable,
            cache: CacheConfig::default(),
            git: GitConfig::default(),
            env: EnvConfig::default(),
            profiles: ProfilesConfig::default(),
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IgnoreRule {
    pub line_number: usize,
    pub pattern: String,
    pub rule: FsRule,
    pub order: usize,
    matcher: RulePattern,
}

#[derive(Debug, Clone)]
pub struct CompiledConfigRule {
    pub rule_index: usize,
    pub pattern: String,
    pub rule: FsRule,
    pub order: usize,
    pub specificity: SpecificityKey,
    matcher: RulePattern,
}

#[derive(Debug, Clone)]
pub struct BuiltinRule {
    pub profile: BuiltinProfile,
    pub pattern: String,
    pub rule: FsRule,
    pub order: usize,
    pub specificity: SpecificityKey,
    matcher: RulePattern,
}

#[derive(Debug, Clone)]
struct RulePattern {
    matcher: Gitignore,
}

impl RulePattern {
    fn compile(pattern: &str, location: impl Into<String>) -> Result<Self, RuleError> {
        let location = location.into();
        if pattern.trim().is_empty() {
            return Err(RuleError::new(&location, "pattern must not be empty"));
        }
        if pattern.starts_with('#') {
            return Err(RuleError::new(&location, "pattern must not be a comment"));
        }
        if pattern.contains('\0') {
            return Err(RuleError::new(&location, "pattern must not contain NUL"));
        }
        if has_unclosed_class(pattern) {
            return Err(RuleError::new(
                &location,
                "pattern has an unclosed character class",
            ));
        }
        let mut builder = GitignoreBuilder::new("");
        builder
            .add_line(None::<PathBuf>, pattern)
            .map_err(|error| RuleError::new(&location, error.to_string()))?;
        let matcher = builder
            .build()
            .map_err(|error| RuleError::new(&location, error.to_string()))?;
        Ok(Self { matcher })
    }

    fn is_match(&self, path: &WorkspacePath, kind: RulePathKind) -> bool {
        self.matcher
            .matched_path_or_any_parents(path.as_str(), kind.is_dir())
            .is_ignore()
    }
}

fn has_unclosed_class(pattern: &str) -> bool {
    let mut escaped = false;
    let mut open = false;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' if !open => open = true,
            ']' if open => open = false,
            _ => {}
        }
    }
    open
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecificityKey {
    pub literal_component_count: usize,
    pub component_count: usize,
    pub anchored_rank: usize,
    pub literal_char_count: usize,
    pub glob_meta_count: usize,
}

impl SpecificityKey {
    pub fn from_pattern(pattern: &str) -> Self {
        let analysis = pattern
            .strip_prefix('!')
            .unwrap_or(pattern)
            .strip_prefix('/')
            .unwrap_or_else(|| pattern.strip_prefix('!').unwrap_or(pattern))
            .strip_suffix('/')
            .unwrap_or_else(|| pattern.strip_suffix('/').unwrap_or(pattern));
        let components: Vec<&str> = analysis
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let literal_component_count = components
            .iter()
            .filter(|component| **component != "**" && !has_unescaped_glob_meta(component))
            .count();
        let literal_char_count = count_literal_chars(analysis);
        let glob_meta_count = count_unescaped_glob_meta(pattern);
        Self {
            literal_component_count,
            component_count: components.len(),
            anchored_rank: usize::from(pattern.starts_with('/') || pattern.contains('/')),
            literal_char_count,
            glob_meta_count,
        }
    }
}

fn has_unescaped_glob_meta(value: &str) -> bool {
    count_unescaped_glob_meta(value) > 0
}

fn count_unescaped_glob_meta(value: &str) -> usize {
    let mut escaped = false;
    let mut count = 0;
    for ch in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '*' | '?' | '[' | ']' => count += 1,
            _ => {}
        }
    }
    count
}

fn count_literal_chars(value: &str) -> usize {
    let mut escaped = false;
    let mut count = 0;
    for ch in value.chars() {
        if escaped {
            if ch != '/' {
                count += 1;
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '/' | '*' | '?' | '[' | ']' => {}
            _ => count += 1,
        }
    }
    count
}

impl Ord for SpecificityKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.literal_component_count
            .cmp(&other.literal_component_count)
            .then(self.component_count.cmp(&other.component_count))
            .then(self.anchored_rank.cmp(&other.anchored_rank))
            .then(self.literal_char_count.cmp(&other.literal_char_count))
            .then(other.glob_meta_count.cmp(&self.glob_meta_count))
    }
}

impl PartialOrd for SpecificityKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RuleSource {
    CliOverride {
        path: String,
    },
    ConfigToml {
        rule_index: usize,
        pattern: String,
    },
    Fs2Ignore {
        line_number: usize,
        pattern: String,
    },
    BuiltinProfile {
        profile: BuiltinProfile,
        pattern: String,
    },
    WorkspaceDefault {
        field: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleCandidate {
    pub precedence: u8,
    pub source: RuleSource,
    pub action: RuleAction,
    pub manager: Option<String>,
    pub scope: Option<String>,
    pub pattern: Option<String>,
    pub line_number: Option<usize>,
    pub rule_index: Option<usize>,
    pub profile: Option<String>,
    pub order: usize,
    pub specificity: Option<SpecificityKey>,
    pub selected: bool,
    pub ignored_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleExplanation {
    pub path: String,
    pub path_kind: RulePathKind,
    pub purpose: EvaluationPurpose,
    pub selected: RuleCandidate,
    pub matched_candidates: Vec<RuleCandidate>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleResolution {
    pub effective_rule: FsRule,
    pub source: RuleSource,
    pub explanation: RuleExplanation,
}

#[derive(Debug, Clone)]
pub struct RuleEngine {
    config: Config,
    config_rules: Vec<CompiledConfigRule>,
    ignore_rules: Vec<IgnoreRule>,
    builtin_rules: Vec<BuiltinRule>,
}

impl RuleEngine {
    pub fn new(config: Config, ignore_rules: Vec<IgnoreRule>) -> Result<Self, RuleError> {
        validate_config(&config)?;
        let config_rules = compile_config_rules(&config.rules)?;
        let builtin_rules = built_in_rules(&config.profiles)?;
        Ok(Self {
            config,
            config_rules,
            ignore_rules,
            builtin_rules,
        })
    }

    pub fn resolve(
        &self,
        path: &WorkspacePath,
        kind: RulePathKind,
        purpose: EvaluationPurpose,
        cli_override: Option<FsRule>,
    ) -> Result<RuleResolution, RuleError> {
        let mut candidates = Vec::new();
        if let Some(rule) = cli_override {
            validate_fs_rule(&rule, "cli_override")?;
            candidates.push(candidate(
                5,
                RuleSource::CliOverride {
                    path: path.as_str().to_owned(),
                },
                &rule,
                None,
                0,
                None,
                true,
                None,
            ));
            self.append_lower_candidates(path, kind, purpose, 5, &mut candidates);
            return Ok(resolution(path, kind, purpose, candidates));
        }

        let config_matches = self.matching_config_rules(path, kind);
        if let Some(selected) = config_matches.first() {
            for item in &config_matches {
                let selected_item = item.rule_index == selected.rule_index;
                candidates.push(candidate(
                    4,
                    RuleSource::ConfigToml {
                        rule_index: item.rule_index,
                        pattern: item.pattern.clone(),
                    },
                    &item.rule,
                    Some(item.pattern.clone()),
                    item.order,
                    Some(item.specificity),
                    selected_item,
                    (!selected_item).then_some("less-specific".to_owned()),
                ));
            }
            self.append_lower_candidates(path, kind, purpose, 4, &mut candidates);
            return Ok(resolution(path, kind, purpose, candidates));
        }

        let ignore_matches = self.matching_ignore_rules(path, kind);
        if let Some(selected) = ignore_matches.last() {
            for item in &ignore_matches {
                let selected_item = item.order == selected.order;
                candidates.push(candidate(
                    3,
                    RuleSource::Fs2Ignore {
                        line_number: item.line_number,
                        pattern: item.pattern.clone(),
                    },
                    &item.rule,
                    Some(item.pattern.clone()),
                    item.order,
                    None,
                    selected_item,
                    (!selected_item).then_some("earlier-fs2ignore-line".to_owned()),
                ));
            }
            self.append_lower_candidates(path, kind, purpose, 3, &mut candidates);
            return Ok(resolution(path, kind, purpose, candidates));
        }

        let builtin_matches = self.matching_builtin_rules(path, kind);
        if let Some(selected) = builtin_matches.first() {
            for item in &builtin_matches {
                let selected_item = item.order == selected.order;
                candidates.push(candidate(
                    2,
                    RuleSource::BuiltinProfile {
                        profile: item.profile,
                        pattern: item.pattern.clone(),
                    },
                    &item.rule,
                    Some(item.pattern.clone()),
                    item.order,
                    Some(item.specificity),
                    selected_item,
                    (!selected_item).then_some("earlier-builtin-rule".to_owned()),
                ));
            }
            self.append_lower_candidates(path, kind, purpose, 2, &mut candidates);
            return Ok(resolution(path, kind, purpose, candidates));
        }

        candidates.push(self.workspace_default_candidate(kind, purpose, true, None));
        Ok(resolution(path, kind, purpose, candidates))
    }

    fn matching_config_rules(
        &self,
        path: &WorkspacePath,
        kind: RulePathKind,
    ) -> Vec<&CompiledConfigRule> {
        let mut matches: Vec<&CompiledConfigRule> = self
            .config_rules
            .iter()
            .filter(|rule| rule.matcher.is_match(path, kind))
            .collect();
        matches.sort_by(|left, right| {
            right
                .specificity
                .cmp(&left.specificity)
                .then(right.order.cmp(&left.order))
        });
        matches
    }

    fn matching_ignore_rules(&self, path: &WorkspacePath, kind: RulePathKind) -> Vec<&IgnoreRule> {
        let mut matches: Vec<&IgnoreRule> = self
            .ignore_rules
            .iter()
            .filter(|rule| rule.matcher.is_match(path, kind))
            .collect();
        matches.sort_by_key(|rule| rule.order);
        matches
    }

    fn matching_builtin_rules(
        &self,
        path: &WorkspacePath,
        kind: RulePathKind,
    ) -> Vec<&BuiltinRule> {
        let mut matches: Vec<&BuiltinRule> = self
            .builtin_rules
            .iter()
            .filter(|rule| rule.matcher.is_match(path, kind))
            .collect();
        matches.sort_by(|left, right| {
            right
                .specificity
                .cmp(&left.specificity)
                .then(right.order.cmp(&left.order))
        });
        matches
    }

    fn append_lower_candidates(
        &self,
        path: &WorkspacePath,
        kind: RulePathKind,
        purpose: EvaluationPurpose,
        selected_precedence: u8,
        candidates: &mut Vec<RuleCandidate>,
    ) {
        if selected_precedence > 4 {
            for item in self.matching_config_rules(path, kind) {
                candidates.push(candidate(
                    4,
                    RuleSource::ConfigToml {
                        rule_index: item.rule_index,
                        pattern: item.pattern.clone(),
                    },
                    &item.rule,
                    Some(item.pattern.clone()),
                    item.order,
                    Some(item.specificity),
                    false,
                    Some("lower-precedence".to_owned()),
                ));
            }
        }
        if selected_precedence > 3 {
            for item in self.matching_ignore_rules(path, kind) {
                candidates.push(candidate(
                    3,
                    RuleSource::Fs2Ignore {
                        line_number: item.line_number,
                        pattern: item.pattern.clone(),
                    },
                    &item.rule,
                    Some(item.pattern.clone()),
                    item.order,
                    None,
                    false,
                    Some("lower-precedence".to_owned()),
                ));
            }
        }
        if selected_precedence > 2 {
            for item in self.matching_builtin_rules(path, kind) {
                candidates.push(candidate(
                    2,
                    RuleSource::BuiltinProfile {
                        profile: item.profile,
                        pattern: item.pattern.clone(),
                    },
                    &item.rule,
                    Some(item.pattern.clone()),
                    item.order,
                    Some(item.specificity),
                    false,
                    Some("lower-precedence".to_owned()),
                ));
            }
        }
        if selected_precedence > 1 {
            candidates.push(self.workspace_default_candidate(
                kind,
                purpose,
                false,
                Some("lower-precedence".to_owned()),
            ));
        }
    }

    fn workspace_default_candidate(
        &self,
        kind: RulePathKind,
        purpose: EvaluationPurpose,
        selected: bool,
        ignored_reason: Option<String>,
    ) -> RuleCandidate {
        let (field, action) = match (kind, purpose) {
            (RulePathKind::Directory, _) => ("directory-default", RuleAction::Normal),
            (_, EvaluationPurpose::NewLocalCreate) => (
                "default_new_file_policy",
                self.config.default_new_file_policy,
            ),
            _ => ("default_file_policy", self.config.default_file_policy),
        };
        let rule = FsRule {
            action,
            manager: None,
            scope: None,
        };
        candidate(
            1,
            RuleSource::WorkspaceDefault {
                field: field.to_owned(),
            },
            &rule,
            None,
            0,
            None,
            selected,
            ignored_reason,
        )
    }
}

pub fn parse_fs2ignore(input: &str) -> Result<Vec<IgnoreRule>, RuleError> {
    let mut rules = Vec::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (action, pattern) = parse_ignore_line(line, line_number)?;
        let rule = FsRule {
            action,
            manager: None,
            scope: None,
        };
        let matcher = RulePattern::compile(&pattern, format!(".fs2ignore:{line_number}"))?;
        rules.push(IgnoreRule {
            line_number,
            pattern,
            rule,
            order: rules.len(),
            matcher,
        });
    }
    Ok(rules)
}

pub fn parse_config_toml(input: &str) -> Result<Config, RuleError> {
    let config: Config =
        toml::from_str(input).map_err(|error| RuleError::new("config", error.to_string()))?;
    validate_config(&config)?;
    let _compiled = compile_config_rules(&config.rules)?;
    Ok(config)
}

pub fn built_in_rules(profiles: &ProfilesConfig) -> Result<Vec<BuiltinRule>, RuleError> {
    let mut out = Vec::new();
    push_builtin(
        &mut out,
        BuiltinProfile::Git,
        ".git/",
        RuleAction::LocalOnly,
        None,
    )?;
    push_builtin(
        &mut out,
        BuiltinProfile::Git,
        ".git/**",
        RuleAction::LocalOnly,
        None,
    )?;
    if profiles.node {
        push_builtin(
            &mut out,
            BuiltinProfile::Node,
            "node_modules/",
            RuleAction::DependencyCache,
            Some("node"),
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Node,
            ".next/",
            RuleAction::Generated,
            None,
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Node,
            ".nuxt/",
            RuleAction::Generated,
            None,
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Node,
            ".turbo/",
            RuleAction::Generated,
            None,
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Node,
            "coverage/",
            RuleAction::Generated,
            None,
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Node,
            ".vercel/",
            RuleAction::LocalOnly,
            None,
        )?;
        for pattern in [
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "bun.lockb",
        ] {
            push_builtin(
                &mut out,
                BuiltinProfile::Node,
                pattern,
                RuleAction::Normal,
                None,
            )?;
        }
    }
    if profiles.rust {
        push_builtin(
            &mut out,
            BuiltinProfile::Rust,
            "target/",
            RuleAction::Generated,
            None,
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Rust,
            "Cargo.lock",
            RuleAction::Normal,
            None,
        )?;
    }
    if profiles.python {
        for pattern in [".venv/", "venv/", "__pycache__/", ".pytest_cache/"] {
            push_builtin(
                &mut out,
                BuiltinProfile::Python,
                pattern,
                RuleAction::Generated,
                None,
            )?;
        }
        for pattern in ["requirements*.txt", "uv.lock", "poetry.lock"] {
            push_builtin(
                &mut out,
                BuiltinProfile::Python,
                pattern,
                RuleAction::Normal,
                None,
            )?;
        }
    }
    if profiles.go {
        push_builtin(
            &mut out,
            BuiltinProfile::Go,
            "go.sum",
            RuleAction::Normal,
            None,
        )?;
        push_builtin(
            &mut out,
            BuiltinProfile::Go,
            "go.work.sum",
            RuleAction::Normal,
            None,
        )?;
    }
    Ok(out)
}

fn parse_ignore_line(line: &str, line_number: usize) -> Result<(RuleAction, String), RuleError> {
    if let Some(rest) = line.strip_prefix("\\:") {
        return Ok((RuleAction::Ignore, format!(":{rest}")));
    }
    if let Some(rest) = line.strip_prefix("\\#") {
        return Ok((RuleAction::Ignore, format!("\\#{rest}")));
    }
    if let Some(rest) = line.strip_prefix("\\!") {
        return Ok((RuleAction::Ignore, format!("\\!{rest}")));
    }
    if let Some(stripped) = line.strip_prefix(':') {
        let Some((action_text, pattern)) = stripped.split_once(char::is_whitespace) else {
            return Err(RuleError::new(
                format!(".fs2ignore:{line_number}"),
                "invalid FS2 action prefix",
            ));
        };
        let action = parse_action(action_text).ok_or_else(|| {
            RuleError::new(
                format!(".fs2ignore:{line_number}"),
                "unknown FS2 action prefix",
            )
        })?;
        let pattern = pattern.trim_start();
        if pattern.is_empty() {
            return Err(RuleError::new(
                format!(".fs2ignore:{line_number}"),
                "pattern must not be empty",
            ));
        }
        if pattern.starts_with('!') {
            return Err(RuleError::new(
                format!(".fs2ignore:{line_number}"),
                "explicit actions cannot use gitignore negation",
            ));
        }
        return Ok((action, pattern.to_owned()));
    }
    if let Some(pattern) = line.strip_prefix('!') {
        if pattern.is_empty() {
            return Err(RuleError::new(
                format!(".fs2ignore:{line_number}"),
                "pattern must not be empty",
            ));
        }
        return Ok((RuleAction::Normal, pattern.to_owned()));
    }
    Ok((RuleAction::Ignore, line.to_owned()))
}

fn parse_action(value: &str) -> Option<RuleAction> {
    Some(match value {
        "ignore" => RuleAction::Ignore,
        "local-only" => RuleAction::LocalOnly,
        "generated" => RuleAction::Generated,
        "lazy" => RuleAction::Lazy,
        "pin" => RuleAction::Pin,
        "normal" => RuleAction::Normal,
        "secret" => RuleAction::Secret,
        "dependency-cache" => RuleAction::DependencyCache,
        _ => return None,
    })
}

fn compile_config_rules(rules: &[ConfigRule]) -> Result<Vec<CompiledConfigRule>, RuleError> {
    let mut out = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        validate_fs_rule(&rule.fs_rule(), format!("rules[{index}]"))?;
        if rule.pattern.starts_with('!') {
            return Err(RuleError::new(
                format!("rules[{index}].pattern"),
                "structured rules must not use gitignore negation",
            ));
        }
        let matcher = RulePattern::compile(&rule.pattern, format!("rules[{index}].pattern"))?;
        out.push(CompiledConfigRule {
            rule_index: index,
            pattern: rule.pattern.clone(),
            rule: rule.fs_rule(),
            order: index,
            specificity: SpecificityKey::from_pattern(&rule.pattern),
            matcher,
        });
    }
    Ok(out)
}

fn validate_config(config: &Config) -> Result<(), RuleError> {
    if config.version != 1 {
        return Err(RuleError::new("version", "config version must be 1"));
    }
    if let Some(name) = &config.workspace_name {
        if name.trim().is_empty() {
            return Err(RuleError::new(
                "workspace_name",
                "workspace name must not be empty",
            ));
        }
    }
    validate_default_policy(config.default_file_policy, "default_file_policy")?;
    validate_default_policy(config.default_new_file_policy, "default_new_file_policy")?;
    if config.git.sync_git_dir && config.git.mode != GitMode::ExperimentalSyncGitDir {
        return Err(RuleError::new(
            "git.sync_git_dir",
            "requires experimental-sync-git-dir mode",
        ));
    }
    fs2_core::NodeName::parse(&config.env.materialize_filename)
        .map_err(|error| RuleError::new("env.materialize_filename", error.to_string()))?;
    for (index, rule) in config.rules.iter().enumerate() {
        if rule.pattern.is_empty() || rule.pattern.contains('\0') {
            return Err(RuleError::new(
                format!("rules[{index}].pattern"),
                "pattern is invalid",
            ));
        }
        validate_fs_rule(&rule.fs_rule(), format!("rules[{index}]"))?;
    }
    Ok(())
}

fn validate_default_policy(action: RuleAction, field: &'static str) -> Result<(), RuleError> {
    if matches!(
        action,
        RuleAction::Normal | RuleAction::Lazy | RuleAction::Pin
    ) {
        Ok(())
    } else {
        Err(RuleError::new(
            field,
            "default policy must be normal, lazy, or pin",
        ))
    }
}

fn validate_fs_rule(rule: &FsRule, location: impl Into<String>) -> Result<(), RuleError> {
    let location = location.into();
    if let Some(manager) = &rule.manager {
        if manager.is_empty() {
            return Err(RuleError::new(&location, "manager must not be empty"));
        }
        if rule.action != RuleAction::DependencyCache {
            return Err(RuleError::new(
                &location,
                "manager is only valid for dependency-cache",
            ));
        }
    }
    if let Some(scope) = &rule.scope {
        if scope.is_empty() {
            return Err(RuleError::new(&location, "scope must not be empty"));
        }
        if rule.action != RuleAction::Secret {
            return Err(RuleError::new(&location, "scope is only valid for secret"));
        }
    }
    Ok(())
}

fn push_builtin(
    out: &mut Vec<BuiltinRule>,
    profile: BuiltinProfile,
    pattern: &str,
    action: RuleAction,
    manager: Option<&str>,
) -> Result<(), RuleError> {
    let rule = FsRule {
        action,
        manager: manager.map(str::to_owned),
        scope: None,
    };
    let matcher = RulePattern::compile(pattern, format!("builtin:{profile:?}:{pattern}"))?;
    out.push(BuiltinRule {
        profile,
        pattern: pattern.to_owned(),
        rule,
        order: out.len(),
        specificity: SpecificityKey::from_pattern(pattern),
        matcher,
    });
    Ok(())
}

fn candidate(
    precedence: u8,
    source: RuleSource,
    rule: &FsRule,
    pattern: Option<String>,
    order: usize,
    specificity: Option<SpecificityKey>,
    selected: bool,
    ignored_reason: Option<String>,
) -> RuleCandidate {
    let line_number = match &source {
        RuleSource::Fs2Ignore { line_number, .. } => Some(*line_number),
        _ => None,
    };
    let rule_index = match &source {
        RuleSource::ConfigToml { rule_index, .. } => Some(*rule_index),
        _ => None,
    };
    let profile = match &source {
        RuleSource::BuiltinProfile { profile, .. } => Some(format!("{profile:?}")),
        _ => None,
    };
    RuleCandidate {
        precedence,
        source,
        action: rule.action,
        manager: rule.manager.clone(),
        scope: rule.scope.clone(),
        pattern,
        line_number,
        rule_index,
        profile,
        order,
        specificity,
        selected,
        ignored_reason,
    }
}

fn resolution(
    path: &WorkspacePath,
    kind: RulePathKind,
    purpose: EvaluationPurpose,
    mut candidates: Vec<RuleCandidate>,
) -> RuleResolution {
    let selected_index = candidates
        .iter()
        .position(|candidate| candidate.selected)
        .unwrap_or(0);
    let selected = candidates[selected_index].clone();
    let rule = FsRule {
        action: selected.action,
        manager: selected.manager.clone(),
        scope: selected.scope.clone(),
    };
    for candidate in &mut candidates {
        if !candidate.selected && candidate.ignored_reason.is_none() {
            candidate.ignored_reason = Some("lower-precedence".to_owned());
        }
    }
    let source = selected.source.clone();
    RuleResolution {
        effective_rule: rule,
        source,
        explanation: RuleExplanation {
            path: path.as_str().to_owned(),
            path_kind: kind,
            purpose,
            selected: selected.clone(),
            matched_candidates: candidates,
            summary: format!("selected {:?} for {}", selected.action, path.as_str()),
        },
    }
}

fn parse_byte_size(value: &str) -> Result<u64, RuleError> {
    let suffixes = [
        ("TiB", 1024_u64.pow(4)),
        ("GiB", 1024_u64.pow(3)),
        ("MiB", 1024_u64.pow(2)),
        ("KiB", 1024),
        ("B", 1),
    ];
    let (digits, multiplier) = suffixes
        .iter()
        .find_map(|(suffix, multiplier)| {
            value
                .strip_suffix(suffix)
                .map(|digits| (digits, *multiplier))
        })
        .unwrap_or((value, 1));
    if digits.is_empty() || digits.starts_with('0') || !digits.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(RuleError::new("byte-size", "invalid byte size"));
    }
    let number = digits
        .parse::<u64>()
        .map_err(|error| RuleError::new("byte-size", error.to_string()))?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| RuleError::new("byte-size", "byte size overflow"))
}

const fn default_true() -> bool {
    true
}

fn default_materialize_filename() -> String {
    ".env.fs2".to_owned()
}

const fn default_file_policy() -> RuleAction {
    RuleAction::Lazy
}

const fn default_new_file_policy() -> RuleAction {
    RuleAction::Normal
}

const fn default_case_policy() -> CasePolicy {
    CasePolicy::Portable
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]
    use super::*;

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::parse(value).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn fs2ignore_parser_supports_actions_comments_and_last_match(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rules = parse_fs2ignore("# comment\n\n*.tmp\n:generated node_modules/\n!keep.tmp\n")?;
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].rule.action, RuleAction::Ignore);
        assert_eq!(rules[1].rule.action, RuleAction::Generated);
        assert_eq!(rules[2].rule.action, RuleAction::Normal);
        assert_eq!(rules[1].line_number, 4);
        let engine = RuleEngine::new(Config::default(), rules)?;
        let result = engine.resolve(
            &path("keep.tmp"),
            RulePathKind::File,
            EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Normal);
        Ok(())
    }

    #[test]
    fn fs2ignore_reports_line_number_errors() {
        let error = parse_fs2ignore(":pinned README.md").err();
        assert!(matches!(error, Some(RuleError { location, .. }) if location == ".fs2ignore:1"));
        assert!(parse_fs2ignore(":generated").is_err());
        assert!(parse_fs2ignore(":generated !foo").is_err());
        let error = parse_fs2ignore("[").err();
        assert!(matches!(error, Some(RuleError { location, .. }) if location == ".fs2ignore:1"));
    }

    #[test]
    fn config_parser_accepts_design_example() -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_config_toml(
            r#"version = 1
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
"#,
        )?;
        assert_eq!(config.workspace_name.as_deref(), Some("personal-code"));
        assert_eq!(config.cache.max_bytes, Some(ByteSize(50 * 1024_u64.pow(3))));
        assert_eq!(config.rules.len(), 2);
        Ok(())
    }

    #[test]
    fn config_parser_rejects_bad_values() {
        assert!(parse_config_toml("version = 2").is_err());
        assert!(parse_config_toml("version = 1\ndefault_file_policy = \"ignore\"").is_err());
        assert!(parse_config_toml("version = 1\n[cache]\nmax_bytes = \"1GB\"").is_err());
        assert!(parse_config_toml("version = 1\n[git]\nsync_git_dir = true").is_err());
        assert!(parse_config_toml("version = 1\n[env]\nmaterialize_filename = \"a/b\"").is_err());
        assert!(parse_config_toml(
            "version = 1\n[[rules]]\npattern = \"x\"\naction = \"generated\"\nmanager = \"node\""
        )
        .is_err());
        let error =
            parse_config_toml("version = 1\n[[rules]]\npattern = \"[\"\naction = \"normal\"").err();
        assert!(
            matches!(error, Some(RuleError { location, .. }) if location == "rules[0].pattern")
        );
        assert!(parse_config_toml(
            "version = 1\n[[rules]]\npattern = \"   \"\naction = \"normal\""
        )
        .is_err());
        assert!(parse_config_toml(
            "version = 1\n[[rules]]\npattern = \"#tmp\"\naction = \"normal\""
        )
        .is_err());
        assert!(parse_config_toml(
            "version = 1\n[[rules]]\npattern = \"!foo\"\naction = \"normal\""
        )
        .is_err());
    }

    #[test]
    fn specificity_treats_escaped_glob_meta_as_literal() {
        assert!(SpecificityKey::from_pattern("\\*") > SpecificityKey::from_pattern("*"));
        assert!(
            SpecificityKey::from_pattern("literal/\\?") > SpecificityKey::from_pattern("literal/*")
        );
    }

    #[test]
    fn precedence_uses_documented_tiers() -> Result<(), Box<dyn std::error::Error>> {
        let config =
            parse_config_toml("version = 1\n[[rules]]\npattern = \"**\"\naction = \"normal\"\n")?;
        let ignore = parse_fs2ignore(":generated target/")?;
        let engine = RuleEngine::new(config, ignore)?;
        let result = engine.resolve(
            &path("target/file.o"),
            RulePathKind::File,
            EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Normal);
        let result = engine.resolve(
            &path("target/file.o"),
            RulePathKind::File,
            EvaluationPurpose::ExistingOrRemoteLookup,
            Some(FsRule {
                action: RuleAction::Pin,
                manager: None,
                scope: None,
            }),
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Pin);
        let invalid_cli = engine.resolve(
            &path("target/file.o"),
            RulePathKind::File,
            EvaluationPurpose::ExistingOrRemoteLookup,
            Some(FsRule {
                action: RuleAction::Normal,
                manager: Some("node".to_owned()),
                scope: None,
            }),
        );
        assert!(invalid_cli.is_err());
        Ok(())
    }

    #[test]
    fn config_specificity_and_later_ties_win() -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_config_toml(
            "version = 1\n[[rules]]\npattern = \".env\"\naction = \"lazy\"\n[[rules]]\npattern = \"apps/*/.env\"\naction = \"secret\"\nscope = \"project\"\n[[rules]]\npattern = \"apps/*/.env\"\naction = \"local-only\"\n",
        )?;
        let engine = RuleEngine::new(config, Vec::new())?;
        let result = engine.resolve(
            &path("apps/web/.env"),
            RulePathKind::File,
            EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::LocalOnly);
        Ok(())
    }

    #[test]
    fn builtins_cover_generated_dependency_defaults() -> Result<(), Box<dyn std::error::Error>> {
        let engine = RuleEngine::new(Config::default(), Vec::new())?;
        let directory = engine.resolve(
            &path("node_modules"),
            RulePathKind::Directory,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(directory.effective_rule.action, RuleAction::DependencyCache);
        assert_eq!(directory.effective_rule.manager.as_deref(), Some("node"));
        let descendant = engine.resolve(
            &path("node_modules/pkg/index.js"),
            RulePathKind::File,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(
            descendant.effective_rule.action,
            RuleAction::DependencyCache
        );
        let dist = engine.resolve(
            &path("dist/index.js"),
            RulePathKind::File,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(dist.effective_rule.action, RuleAction::Normal);
        let git_index = engine.resolve(
            &path(".git/index"),
            RulePathKind::File,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(git_index.effective_rule.action, RuleAction::LocalOnly);
        let git_package_lock = engine.resolve(
            &path(".git/package-lock.json"),
            RulePathKind::File,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(
            git_package_lock.effective_rule.action,
            RuleAction::LocalOnly
        );
        Ok(())
    }

    #[test]
    fn profile_disable_and_workspace_defaults_work() -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_config_toml("version = 1\n[profiles]\nnode = false\n")?;
        let engine = RuleEngine::new(config, Vec::new())?;
        let result = engine.resolve(
            &path("node_modules/pkg/index.js"),
            RulePathKind::File,
            EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Lazy);
        let result = engine.resolve(
            &path("unmatched.txt"),
            RulePathKind::File,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Normal);
        let result = engine.resolve(
            &path("unmatched-dir"),
            RulePathKind::Directory,
            EvaluationPurpose::ExistingOrRemoteLookup,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Normal);
        Ok(())
    }

    #[test]
    fn explanation_reports_structured_selected_source() -> Result<(), Box<dyn std::error::Error>> {
        let ignore = parse_fs2ignore(":generated target/")?;
        let engine = RuleEngine::new(Config::default(), ignore)?;
        let result = engine.resolve(
            &path("target/debug/app"),
            RulePathKind::File,
            EvaluationPurpose::NewLocalCreate,
            None,
        )?;
        assert_eq!(result.effective_rule.action, RuleAction::Generated);
        assert!(matches!(
            result.source,
            RuleSource::Fs2Ignore { line_number: 1, .. }
        ));
        assert!(result.explanation.selected.selected);
        assert!(!result.explanation.matched_candidates.is_empty());
        assert_eq!(result.explanation.selected.line_number, Some(1));
        assert!(result
            .explanation
            .matched_candidates
            .iter()
            .any(|candidate| matches!(candidate.source, RuleSource::WorkspaceDefault { .. })));
        assert!(result
            .explanation
            .matched_candidates
            .iter()
            .any(|candidate| candidate.ignored_reason.as_deref() == Some("lower-precedence")));
        Ok(())
    }
}
