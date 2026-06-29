//! Built-in default profiles for common language ecosystems.
//!
//! These provide sensible defaults for `node_modules`, `target`, `.venv`, etc.
//! Users can disable them via `.fs2/config.toml` (future) or override with
//! `.fs2ignore` entries.

use crate::action::Action;
use crate::fs2ignore::Fs2IgnoreEntry;

/// A named built-in profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Profile name, e.g. `"node"`, `"rust"`, `"python"`, `"go"`.
    pub name: &'static str,
    /// Entries the profile contributes.
    pub entries: &'static [ProfileEntry],
}

/// A single entry in a built-in profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    /// Glob pattern.
    pub pattern: &'static str,
    /// Action.
    pub action: Action,
}

const NODE_PROFILE_ENTRIES: &[ProfileEntry] = &[
    ProfileEntry {
        pattern: "node_modules/",
        action: Action::DependencyCache,
    },
    ProfileEntry {
        pattern: ".next/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: ".nuxt/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: ".turbo/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: ".vercel/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: "coverage/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: "package-lock.json",
        action: Action::Pin,
    },
    ProfileEntry {
        pattern: "pnpm-lock.yaml",
        action: Action::Pin,
    },
    ProfileEntry {
        pattern: "yarn.lock",
        action: Action::Pin,
    },
    ProfileEntry {
        pattern: "bun.lockb",
        action: Action::Pin,
    },
    // NOTE: `dist/` is intentionally NOT included here. The design says not to
    // globally mark `dist/` as generated without a prompt or project-specific
    // rule, because some repos intentionally commit or publish built artifacts.
];

const RUST_PROFILE_ENTRIES: &[ProfileEntry] = &[
    ProfileEntry {
        pattern: "target/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: "Cargo.lock",
        action: Action::Normal,
    },
];

const PYTHON_PROFILE_ENTRIES: &[ProfileEntry] = &[
    ProfileEntry {
        pattern: ".venv/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: "venv/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: "__pycache__/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: ".pytest_cache/",
        action: Action::Generated,
    },
    ProfileEntry {
        pattern: "requirements.txt",
        action: Action::Normal,
    },
    ProfileEntry {
        pattern: "uv.lock",
        action: Action::Pin,
    },
    ProfileEntry {
        pattern: "poetry.lock",
        action: Action::Pin,
    },
];

const GO_PROFILE_ENTRIES: &[ProfileEntry] = &[
    ProfileEntry {
        pattern: "go.sum",
        action: Action::Normal,
    },
    ProfileEntry {
        pattern: "go.work.sum",
        action: Action::Normal,
    },
];

const PROFILES: [Profile; 4] = [
    Profile {
        name: "node",
        entries: NODE_PROFILE_ENTRIES,
    },
    Profile {
        name: "rust",
        entries: RUST_PROFILE_ENTRIES,
    },
    Profile {
        name: "python",
        entries: PYTHON_PROFILE_ENTRIES,
    },
    Profile {
        name: "go",
        entries: GO_PROFILE_ENTRIES,
    },
];

/// Return all built-in profiles.
///
/// The design says the daemon ships language/tool profiles. These are the
/// defaults; user `.fs2ignore` and `.fs2/config.toml` rules override them.
#[must_use]
pub fn builtin_profiles() -> &'static [Profile] {
    &PROFILES
}

/// Return all profile entries from every built-in profile, as ignore-file
/// entries (with synthetic line number 0 since they have no source file).
///
/// The engine treats these as the lowest-precedence source.
#[must_use]
pub fn builtin_entries() -> Vec<Fs2IgnoreEntry> {
    let mut out = Vec::new();
    for profile in builtin_profiles() {
        for entry in profile.entries {
            out.push(Fs2IgnoreEntry {
                line: 0,
                action: entry.action,
                pattern: entry.pattern.to_owned(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_profile_marks_node_modules_dependency_cache() {
        let node = builtin_profiles()
            .iter()
            .find(|p| p.name == "node")
            .unwrap();
        let nm = node
            .entries
            .iter()
            .find(|e| e.pattern == "node_modules/")
            .unwrap();
        assert_eq!(nm.action, Action::DependencyCache);
    }

    #[test]
    fn node_profile_marks_next_and_turbo_generated() {
        let node = builtin_profiles()
            .iter()
            .find(|p| p.name == "node")
            .unwrap();
        for pat in [".next/", ".turbo/", ".nuxt/"] {
            let e = node.entries.iter().find(|e| e.pattern == pat).unwrap();
            assert_eq!(e.action, Action::Generated, "{pat} should be generated");
        }
    }

    #[test]
    fn lockfiles_are_pinned() {
        let node = builtin_profiles()
            .iter()
            .find(|p| p.name == "node")
            .unwrap();
        for pat in ["pnpm-lock.yaml", "yarn.lock", "bun.lockb"] {
            let e = node.entries.iter().find(|e| e.pattern == pat).unwrap();
            assert_eq!(e.action, Action::Pin, "{pat} should be pinned");
        }
    }

    #[test]
    fn rust_profile_marks_target_generated() {
        let rust = builtin_profiles()
            .iter()
            .find(|p| p.name == "rust")
            .unwrap();
        let target = rust
            .entries
            .iter()
            .find(|e| e.pattern == "target/")
            .unwrap();
        assert_eq!(target.action, Action::Generated);
    }

    #[test]
    fn python_profile_marks_venv_and_pycache_generated() {
        let py = builtin_profiles()
            .iter()
            .find(|p| p.name == "python")
            .unwrap();
        for pat in [".venv/", "venv/", "__pycache__/"] {
            let e = py.entries.iter().find(|e| e.pattern == pat).unwrap();
            assert_eq!(e.action, Action::Generated, "{pat} should be generated");
        }
    }

    #[test]
    fn go_profile_marks_go_sum_normal() {
        let go = builtin_profiles().iter().find(|p| p.name == "go").unwrap();
        let sum = go.entries.iter().find(|e| e.pattern == "go.sum").unwrap();
        assert_eq!(sum.action, Action::Normal);
    }

    #[test]
    fn builtin_entries_nonempty() {
        let entries = builtin_entries();
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|e| e.pattern == "node_modules/"));
        assert!(entries.iter().any(|e| e.pattern == "target/"));
        assert!(entries.iter().any(|e| e.pattern == ".venv/"));
        assert!(entries.iter().any(|e| e.pattern == "go.sum"));
    }

    #[test]
    fn all_four_profiles_present() {
        let names: Vec<_> = builtin_profiles().iter().map(|p| p.name).collect();
        assert_eq!(names, ["node", "rust", "python", "go"]);
    }
}
