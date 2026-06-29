//! Built-in language/tool profiles.
//!
//! These ship with the daemon and provide sensible defaults for common
//! project types. Users can disable them via config.

use crate::action::Action;
use crate::precedence::{RuleEntry, RuleSource};

/// Return the built-in profile rules for Node, Rust, Python, and Go.
///
/// These are the lowest-priority rules above the workspace default. They are
/// overridden by any `.fs2ignore` or `.fs2/config.toml` rule.
#[must_use]
pub fn builtin_profiles() -> Vec<RuleEntry> {
    let mut rules = Vec::new();
    // Node profile.
    rules.extend([
        RuleEntry {
            pattern: "node_modules/**".to_owned(),
            action: Action::DependencyCache,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: ".next/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: ".nuxt/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: ".turbo/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: "coverage/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: "package-lock.json".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: "pnpm-lock.yaml".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: "yarn.lock".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("node".to_owned()),
        },
        RuleEntry {
            pattern: "bun.lockb".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("node".to_owned()),
        },
    ]);
    // Rust profile.
    rules.extend([
        RuleEntry {
            pattern: "target/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("rust".to_owned()),
        },
        RuleEntry {
            pattern: "Cargo.lock".to_owned(),
            action: Action::Normal,
            source: RuleSource::Profile("rust".to_owned()),
        },
    ]);
    // Python profile.
    rules.extend([
        RuleEntry {
            pattern: ".venv/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("python".to_owned()),
        },
        RuleEntry {
            pattern: "venv/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("python".to_owned()),
        },
        RuleEntry {
            pattern: "__pycache__/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("python".to_owned()),
        },
        RuleEntry {
            pattern: ".pytest_cache/**".to_owned(),
            action: Action::Generated,
            source: RuleSource::Profile("python".to_owned()),
        },
        RuleEntry {
            pattern: "uv.lock".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("python".to_owned()),
        },
        RuleEntry {
            pattern: "poetry.lock".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("python".to_owned()),
        },
    ]);
    // Go profile.
    rules.extend([
        RuleEntry {
            pattern: "go.sum".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("go".to_owned()),
        },
        RuleEntry {
            pattern: "go.work.sum".to_owned(),
            action: Action::Pin,
            source: RuleSource::Profile("go".to_owned()),
        },
    ]);
    // Git-aware: exclude .git internals by default as local-only/ignored.
    rules.push(RuleEntry {
        pattern: ".git/**".to_owned(),
        action: Action::Ignore,
        source: RuleSource::BuiltinGit,
    });
    // Editor/swap/temp ignore rules so atomic saves and editor swap files
    // don't create noisy synced temp files (design §13.5).
    rules.extend([
        RuleEntry {
            pattern: "*.swp".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: "*.swo".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: "*~".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: ".*~".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: ".#*".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: "#*#".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: "*.tmp".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
        RuleEntry {
            pattern: ".DS_Store".to_owned(),
            action: Action::Ignore,
            source: RuleSource::Profile("editor".to_owned()),
        },
    ]);
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precedence::RuleEngine;

    #[test]
    fn node_modules_is_dependency_cache() {
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy);
        let rule = engine.resolve("apps/web/node_modules/react/index.js");
        assert_eq!(rule.action, Action::DependencyCache);
    }

    #[test]
    fn target_is_generated() {
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy);
        let rule = engine.resolve("project/target/debug/app");
        assert_eq!(rule.action, Action::Generated);
    }

    #[test]
    fn lockfiles_are_pinned() {
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy);
        let rule = engine.resolve("pnpm-lock.yaml");
        assert_eq!(rule.action, Action::Pin);
        let rule = engine.resolve("apps/web/package-lock.json");
        assert_eq!(rule.action, Action::Pin);
    }

    #[test]
    fn git_internals_ignored() {
        let engine = RuleEngine::new(builtin_profiles(), Action::Lazy);
        let rule = engine.resolve(".git/index");
        assert_eq!(rule.action, Action::Ignore);
    }

    #[test]
    fn dist_not_globally_generated() {
        // dist/ should NOT be globally marked generated by built-in profiles.
        let engine = RuleEngine::new(builtin_profiles(), Action::Normal);
        let rule = engine.resolve("apps/web/dist/index.html");
        assert_eq!(rule.action, Action::Normal);
    }

    #[test]
    fn editor_swap_temp_files_ignored() {
        let engine = RuleEngine::new(builtin_profiles(), Action::Normal);
        // vim swap files
        assert_eq!(engine.resolve(".app.ts.swp").action, Action::Ignore);
        assert_eq!(engine.resolve("app.ts.swo").action, Action::Ignore);
        // backup files
        assert_eq!(engine.resolve("app.ts~").action, Action::Ignore);
        // emacs lock files
        assert_eq!(engine.resolve(".#app.ts").action, Action::Ignore);
        // temp files
        assert_eq!(engine.resolve("data.tmp").action, Action::Ignore);
        // macOS .DS_Store
        assert_eq!(engine.resolve(".DS_Store").action, Action::Ignore);
        // Normal files still sync.
        assert_eq!(engine.resolve("app.ts").action, Action::Normal);
    }
}
