//! Pure ignore and platform policy for the sync engine.
//!
//! This module intentionally defines a product-specific `.syncignore` mechanism
//! instead of reading `.gitignore`. The syntax is deliberately close to the
//! useful subset of gitignore globs so it is familiar, but the semantics are for
//! Dropbox/Drive-style synchronization:
//!
//! - policy input is only caller-supplied `.syncignore` text; `.gitignore` is
//!   never consulted;
//! - project-local rules have higher precedence than user-global rules, and both
//!   override ordinary built-in defaults; Git repository metadata remains
//!   local-only and cannot be negated back to sync;
//! - within one rule set, the last matching rule wins;
//! - `!pattern` negates a prior match and returns the path to normal sync,
//!   including under an ignored parent, because evaluation is pure and does not
//!   depend on traversal state;
//! - trailing `/` marks a directory-style rule, matching that path segment and
//!   descendants without matching similarly named files such as `build.log`;
//! - ignored paths are known excluded metadata, not "missing" remote content.
//!
//! Built-in Git disposition is explicit and non-overridable: `.git/` directories
//! and submodule `.git` pointer files are local-only metadata, `.gitmodules` is
//! excluded too, and whole-folder sync replaces submodule workflows by syncing
//! ordinary folder contents while refusing to transmit repository control metadata.

use crate::foundation::{Architecture, OsFamily, Platform};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path};

pub const MODULE_NAME: &str = "policy";
pub const SYNCIGNORE_FILE_NAME: &str = ".syncignore";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Default disposition: content is eligible for normal sync.
    Sync,
    /// Excluded content. The sync engine should record it as known-ignored,
    /// not as absent or missing.
    Ignore,
    /// Dependency content that should not be byte-synced. Later chunks can
    /// record package manifests and rebuild it on the destination machine.
    RebuildLocally,
    /// Platform-bound content. The platform tag is written from CHUNK-01's
    /// shared platform identity and must match before hydration.
    PlatformPin(PlatformPin),
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Ignore => "ignore",
            Self::RebuildLocally => "rebuild-locally",
            Self::PlatformPin(_) => "platform-pin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformPin {
    pub os_family: OsFamily,
    pub architecture: Architecture,
}

impl PlatformPin {
    pub fn from_platform(platform: &Platform) -> Self {
        Self {
            os_family: platform.os_family.clone(),
            architecture: platform.architecture.clone(),
        }
    }

    pub fn matches(&self, platform: &Platform) -> bool {
        self.os_family == platform.os_family && self.architecture == platform.architecture
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleEffect {
    Ignore,
    Sync,
}

impl RuleEffect {
    fn to_action(self) -> Action {
        match self {
            Self::Ignore => Action::Ignore,
            Self::Sync => Action::Sync,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleSet {
    rules: Vec<SyncignoreRule>,
}

impl RuleSet {
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn parse(text: &str) -> Result<Self, RuleParseError> {
        let mut rules = Self::empty();
        for (index, line) in text.lines().enumerate() {
            if let Some(rule) = SyncignoreRule::parse(line, index + 1)? {
                rules.rules.push(rule);
            }
        }
        Ok(rules)
    }

    pub fn add_rule(&mut self, line: &str) -> Result<bool, RuleParseError> {
        let Some(rule) = SyncignoreRule::parse(line, self.rules.len() + 1)? else {
            return Ok(false);
        };
        self.rules.push(rule);
        Ok(true)
    }

    pub fn remove_rule(&mut self, line: &str) -> Result<bool, RuleParseError> {
        let Some(rule) = SyncignoreRule::parse(line, 1)? else {
            return Ok(false);
        };
        let Some(index) = self.rules.iter().rposition(|existing| existing.same_matcher(&rule)) else {
            return Ok(false);
        };
        self.rules.remove(index);
        Ok(true)
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[SyncignoreRule] {
        &self.rules
    }

    fn evaluate(&self, segments: &[&str], case_sensitive: bool) -> Option<RuleEffect> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches(segments, case_sensitive))
            .map(SyncignoreRule::effect)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncignoreRule {
    original: String,
    effect: RuleEffect,
    pattern: RulePattern,
}

impl SyncignoreRule {
    fn parse(line: &str, line_number: usize) -> Result<Option<Self>, RuleParseError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let escaped_leading_marker = trimmed.starts_with("\\#") || trimmed.starts_with("\\!");
        if !escaped_leading_marker && trimmed.starts_with('#') {
            return Ok(None);
        }

        let (effect, pattern_text) = if !escaped_leading_marker && trimmed.starts_with('!') {
            (RuleEffect::Sync, trimmed[1..].trim_start())
        } else {
            (RuleEffect::Ignore, trimmed)
        };

        let pattern_text = pattern_text
            .strip_prefix("\\#")
            .or_else(|| pattern_text.strip_prefix("\\!"))
            .unwrap_or(pattern_text);

        Ok(Some(Self {
            original: trimmed.to_owned(),
            effect,
            pattern: RulePattern::parse(pattern_text, line_number)?,
        }))
    }

    pub fn original(&self) -> &str {
        &self.original
    }

    pub fn effect(&self) -> RuleEffect {
        self.effect
    }

    fn same_matcher(&self, other: &Self) -> bool {
        self.effect == other.effect && self.pattern == other.pattern
    }

    fn matches(&self, segments: &[&str], case_sensitive: bool) -> bool {
        self.pattern.matches(segments, case_sensitive)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RulePattern {
    anchored: bool,
    directory_only: bool,
    contains_slash: bool,
    segments: Vec<String>,
}

impl RulePattern {
    fn parse(pattern: &str, line_number: usize) -> Result<Self, RuleParseError> {
        let anchored = pattern.starts_with('/');
        let directory_only = pattern.ends_with('/');
        let pattern = pattern.trim_start_matches('/').trim_end_matches('/');
        if pattern.is_empty() {
            return Err(RuleParseError::new(
                line_number,
                "rule pattern must not be empty",
            ));
        }

        let contains_slash = pattern.contains('/');
        let segments = pattern
            .split('/')
            .filter(|segment| !segment.is_empty() && *segment != ".")
            .map(str::to_owned)
            .collect::<Vec<_>>();

        if segments.is_empty() {
            return Err(RuleParseError::new(
                line_number,
                "rule pattern must contain at least one path segment",
            ));
        }

        Ok(Self {
            anchored,
            directory_only,
            contains_slash,
            segments,
        })
    }

    fn matches(&self, path_segments: &[&str], case_sensitive: bool) -> bool {
        if path_segments.is_empty() {
            return false;
        }

        if self.segments.len() == 1 && !self.contains_slash {
            let pattern = self.segments[0].as_str();
            if self.anchored {
                return segment_glob_matches(pattern, path_segments[0], case_sensitive);
            }
            return path_segments
                .iter()
                .any(|segment| segment_glob_matches(pattern, segment, case_sensitive));
        }

        if self.anchored {
            return match_segment_sequence(
                &self.segments,
                path_segments,
                case_sensitive,
                self.directory_only,
            );
        }

        (0..path_segments.len()).any(|start| {
            match_segment_sequence(
                &self.segments,
                &path_segments[start..],
                case_sensitive,
                self.directory_only,
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleParseError {
    line: usize,
    message: String,
}

impl RuleParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RuleParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} line {}: {}",
            SYNCIGNORE_FILE_NAME, self.line, self.message
        )
    }
}

impl Error for RuleParseError {}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Policy {
    project_rules: RuleSet,
    user_rules: RuleSet,
}

impl Policy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_syncignore(
        project_local: &str,
        user_global: &str,
    ) -> Result<Self, RuleParseError> {
        Ok(Self {
            project_rules: RuleSet::parse(project_local)?,
            user_rules: RuleSet::parse(user_global)?,
        })
    }

    pub fn with_project_rules(mut self, rules: RuleSet) -> Self {
        self.project_rules = rules;
        self
    }

    pub fn with_user_rules(mut self, rules: RuleSet) -> Self {
        self.user_rules = rules;
        self
    }

    pub fn project_rules(&self) -> &RuleSet {
        &self.project_rules
    }

    pub fn user_rules(&self) -> &RuleSet {
        &self.user_rules
    }

    /// Pure policy evaluation for watcher hot loops. The caller supplies the
    /// path relative to the project root and the already-detected CHUNK-01
    /// platform identity; this method performs no filesystem or environment I/O.
    pub fn evaluate<P: AsRef<Path>>(&self, path: P, platform: &Platform) -> Action {
        let path = path.as_ref();
        let segments = normalized_segments(path);
        let case_sensitive = platform.capabilities.case_sensitive_paths;

        if is_git_metadata_path(&segments, case_sensitive) {
            return Action::Ignore;
        }

        if let Some(effect) = self.project_rules.evaluate(&segments, case_sensitive) {
            return effect.to_action();
        }

        if let Some(effect) = self.user_rules.evaluate(&segments, case_sensitive) {
            return effect.to_action();
        }

        built_in_action(&segments, platform)
    }
}

const REBUILD_LOCALLY_DIRS: &[&str] = &["node_modules", ".venv", "venv"];
const GENERATED_IGNORE_DIRS: &[&str] = &[
    "dist",
    "build",
    ".next",
    "target",
    "__pycache__",
    "coverage",
    ".pytest_cache",
    ".mypy_cache",
    ".cache",
];
const OS_ARTIFACT_FILES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", "ehthumbs.db", "Icon\r"];

fn built_in_action(segments: &[&str], platform: &Platform) -> Action {
    if segments.is_empty() {
        return Action::Sync;
    }

    let case_sensitive = platform.capabilities.case_sensitive_paths;

    if is_git_metadata_path(segments, case_sensitive) {
        return Action::Ignore;
    }

    if contains_any_segment(segments, REBUILD_LOCALLY_DIRS, case_sensitive) {
        return Action::RebuildLocally;
    }

    if contains_any_segment(segments, GENERATED_IGNORE_DIRS, case_sensitive) {
        return Action::Ignore;
    }

    if leaf_matches_any(segments, OS_ARTIFACT_FILES, case_sensitive) {
        return Action::Ignore;
    }

    if is_native_binary_name(segments[segments.len() - 1], case_sensitive) {
        return Action::PlatformPin(PlatformPin::from_platform(platform));
    }

    Action::Sync
}

fn is_git_metadata_path(segments: &[&str], case_sensitive: bool) -> bool {
    contains_segment(segments, ".git", case_sensitive)
        || leaf_matches(segments, ".gitmodules", case_sensitive)
}

fn contains_any_segment(segments: &[&str], needles: &[&str], case_sensitive: bool) -> bool {
    needles
        .iter()
        .any(|needle| contains_segment(segments, needle, case_sensitive))
}

fn contains_segment(segments: &[&str], needle: &str, case_sensitive: bool) -> bool {
    segments
        .iter()
        .any(|segment| text_matches(segment, needle, case_sensitive))
}

fn leaf_matches_any(segments: &[&str], needles: &[&str], case_sensitive: bool) -> bool {
    needles
        .iter()
        .any(|needle| leaf_matches(segments, needle, case_sensitive))
}

fn leaf_matches(segments: &[&str], needle: &str, case_sensitive: bool) -> bool {
    segments
        .last()
        .is_some_and(|leaf| text_matches(leaf, needle, case_sensitive))
}

fn text_matches(left: &str, right: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        left == right
    } else {
        left.eq_ignore_ascii_case(right)
    }
}

fn is_native_binary_name(name: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        return has_native_binary_suffix(name);
    }

    has_native_binary_suffix(&name.to_ascii_lowercase())
}

fn has_native_binary_suffix(name: &str) -> bool {
    name.ends_with(".node")
        || name.ends_with(".dylib")
        || name.ends_with(".dll")
        || name.ends_with(".exe")
        || name.ends_with(".so")
        || name.contains(".so.")
}

fn normalized_segments(path: &Path) -> Vec<&str> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let Some(value) = value.to_str() else {
                    continue;
                };
                segments.extend(
                    value
                        .split(['/', '\\'])
                        .filter(|segment| !segment.is_empty() && *segment != "."),
                );
            }
            Component::ParentDir => segments.push(".."),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    segments
}

fn match_segment_sequence(
    pattern: &[String],
    path: &[&str],
    case_sensitive: bool,
    allow_prefix: bool,
) -> bool {
    if pattern.is_empty() {
        return allow_prefix || path.is_empty();
    }

    if pattern[0] == "**" {
        if match_segment_sequence(&pattern[1..], path, case_sensitive, allow_prefix) {
            return true;
        }
        return (0..path.len()).any(|index| {
            match_segment_sequence(&pattern[1..], &path[index + 1..], case_sensitive, allow_prefix)
        });
    }

    let Some((head, tail)) = path.split_first() else {
        return false;
    };

    segment_glob_matches(pattern[0].as_str(), head, case_sensitive)
        && match_segment_sequence(&pattern[1..], tail, case_sensitive, allow_prefix)
}

fn segment_glob_matches(pattern: &str, text: &str, case_sensitive: bool) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let mut pattern_index = 0;
    let mut text_index = 0;
    let mut last_star = None;
    let mut retry_text_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?'
                || chars_match(pattern[pattern_index], text[text_index], case_sensitive))
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            last_star = Some(pattern_index);
            pattern_index += 1;
            retry_text_index = text_index;
        } else if let Some(star_index) = last_star {
            pattern_index = star_index + 1;
            retry_text_index += 1;
            text_index = retry_text_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
}

fn chars_match(left: char, right: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        left == right
    } else {
        left.eq_ignore_ascii_case(&right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundation::{
        MachineId, MachineIdProvenance, PlatformCapabilities,
    };

    fn platform(os_family: OsFamily, architecture: Architecture) -> Platform {
        let capabilities = PlatformCapabilities::for_os(&os_family);
        Platform {
            os_family,
            os_version: None,
            architecture,
            capabilities,
            machine_id: MachineId {
                value: "test-machine".to_owned(),
                provenance: MachineIdProvenance::Fallback,
            },
        }
    }

    fn linux() -> Platform {
        platform(OsFamily::Linux, Architecture::X86_64)
    }

    fn macos() -> Platform {
        platform(OsFamily::Macos, Architecture::Aarch64)
    }

    #[test]
    fn rule_precedence_keeps_ordinary_builtin_defaults_overrideable() {
        let platform = linux();
        let policy = Policy::from_syncignore("!dist/\n!*.tmp\n", "*.tmp\n").unwrap();

        assert_eq!(policy.evaluate("dist/app.js", &platform), Action::Sync);
        assert_eq!(policy.evaluate("scratch.tmp", &platform), Action::Sync);
        assert_eq!(policy.evaluate("build/app.js", &platform), Action::Ignore);

        let user_policy = Policy::from_syncignore("", "!build/\n").unwrap();
        assert_eq!(user_policy.evaluate("build/app.js", &platform), Action::Sync);

        let os_artifact_policy = Policy::from_syncignore("!.DS_Store\n", "").unwrap();
        assert_eq!(
            os_artifact_policy.evaluate("docs/.DS_Store", &platform),
            Action::Sync
        );
    }

    #[test]
    fn syncignore_semantics_include_negation_anchoring_and_directory_rules() {
        let platform = linux();
        let policy = Policy::from_syncignore(
            "# comments are ignored\n*.log\n!keep.log\nbuild/\n/src/*.tmp\n",
            "",
        )
        .unwrap();

        assert_eq!(policy.evaluate("logs/error.log", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("logs/keep.log", &platform), Action::Sync);
        assert_eq!(policy.evaluate("build/output.js", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("build.log", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("build.txt", &platform), Action::Sync);
        assert_eq!(policy.evaluate("src/generated.tmp", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("nested/src/generated.tmp", &platform), Action::Sync);
    }

    #[test]
    fn gitignore_is_not_a_policy_input() {
        let platform = linux();
        let policy = Policy::new();

        assert_eq!(policy.evaluate(".gitignore", &platform), Action::Sync);
        assert_eq!(policy.evaluate("commonly-gitignored.env", &platform), Action::Sync);
    }

    #[test]
    fn node_modules_rebuilds_locally_instead_of_syncing_or_ignoring() {
        let platform = linux();
        let action = Policy::new().evaluate("app/node_modules/pkg/index.js", &platform);

        assert_eq!(action, Action::RebuildLocally);
        assert_ne!(action, Action::Ignore);
        assert_ne!(action, Action::Sync);
        assert_eq!(
            Policy::new().evaluate(".venv/lib/python", &platform),
            Action::RebuildLocally
        );
    }

    #[test]
    fn generated_and_os_artifact_paths_are_ignored() {
        let platform = linux();
        let policy = Policy::new();

        assert_eq!(policy.evaluate("dist/app.js", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("build/output.o", &platform), Action::Ignore);
        assert_eq!(policy.evaluate(".next/server/app.js", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("target/debug/app", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("pkg/__pycache__/module.pyc", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("docs/.DS_Store", &platform), Action::Ignore);
    }

    #[test]
    fn native_binaries_are_platform_pinned_with_shared_platform_identity() {
        let linux = linux();
        let macos = macos();
        let linux_pin = PlatformPin::from_platform(&linux);
        let macos_pin = PlatformPin::from_platform(&macos);

        assert_eq!(
            Policy::new().evaluate("native/libaddon.so.1", &linux),
            Action::PlatformPin(linux_pin.clone())
        );
        assert_eq!(
            Policy::new().evaluate("native/addon.dylib", &macos),
            Action::PlatformPin(macos_pin)
        );
        assert!(linux_pin.matches(&linux));
        assert!(!linux_pin.matches(&macos));
    }

    #[test]
    fn all_action_variants_have_stable_wire_names() {
        let pin = PlatformPin {
            os_family: OsFamily::Linux,
            architecture: Architecture::X86_64,
        };

        assert_eq!(Action::Sync.as_str(), "sync");
        assert_eq!(Action::Ignore.as_str(), "ignore");
        assert_eq!(Action::RebuildLocally.as_str(), "rebuild-locally");
        assert_eq!(Action::PlatformPin(pin).as_str(), "platform-pin");
    }

    #[test]
    fn git_metadata_is_local_only_while_submodule_contents_sync_as_folders() {
        let platform = linux();
        let policy = Policy::new();

        assert_eq!(policy.evaluate(".git/config", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("vendor/lib/.git", &platform), Action::Ignore);
        assert_eq!(policy.evaluate(".gitmodules", &platform), Action::Ignore);
        assert_eq!(policy.evaluate("vendor/lib/src/lib.rs", &platform), Action::Sync);
    }

    #[test]
    fn git_metadata_cannot_be_overridden_by_syncignore_negations() {
        let platform = linux();
        let project_policy =
            Policy::from_syncignore("!.git/\n!vendor/lib/.git\n!.gitmodules\n", "").unwrap();
        let user_policy =
            Policy::from_syncignore("", "!.git/\n!vendor/lib/.git\n!.gitmodules\n").unwrap();

        for policy in [&project_policy, &user_policy] {
            assert_eq!(policy.evaluate(".git/config", &platform), Action::Ignore);
            assert_eq!(policy.evaluate("vendor/lib/.git", &platform), Action::Ignore);
            assert_eq!(policy.evaluate(".gitmodules", &platform), Action::Ignore);
        }
    }

    #[test]
    fn rules_can_be_added_and_removed_without_io() {
        let platform = linux();
        let mut rules = RuleSet::empty();

        assert!(rules.add_rule("*.local").unwrap());
        let policy = Policy::new().with_project_rules(rules.clone());
        assert_eq!(policy.evaluate("settings.local", &platform), Action::Ignore);

        assert!(rules.remove_rule("*.local").unwrap());
        let policy = Policy::new().with_project_rules(rules);
        assert_eq!(policy.evaluate("settings.local", &platform), Action::Sync);
    }
}
