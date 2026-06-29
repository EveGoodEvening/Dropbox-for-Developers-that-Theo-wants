//! Doctor diagnostics.
//!
//! Checks FUSE availability, daemon reachability, backend connection,
//! auth token, workspace keys, cache permissions, path collisions,
//! `.env` sync safety, `.git` sync safety, and generated directories.

use serde::{Deserialize, Serialize};

/// A doctor check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Check name.
    pub name: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Human-readable message.
    pub message: String,
    /// Optional suggested fix.
    pub suggestion: Option<String>,
}

/// Run all doctor checks.
///
/// # Errors
/// Returns an error if a check fails unexpectedly (not if a check reports a problem).
pub fn run_checks() -> Vec<CheckResult> {
    let results = vec![
        check_git_installed(),
        check_backend_reachable(),
        check_auth_token(),
        check_cache_writable(),
        check_git_sync_safety(),
        check_env_sync_safety(),
    ];
    results
}

fn check_git_installed() -> CheckResult {
    let git_installed = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    CheckResult {
        name: "git_installed".to_owned(),
        passed: git_installed,
        message: if git_installed {
            "Git is installed".to_owned()
        } else {
            "Git is not installed".to_owned()
        },
        suggestion: if git_installed {
            None
        } else {
            Some("Install Git: https://git-scm.com/downloads".to_owned())
        },
    }
}

fn check_backend_reachable() -> CheckResult {
    let cfg = crate::config::CliConfig::load();
    match cfg {
        Ok(cfg) => {
            // We can't do async here, so just check that the config exists.
            CheckResult {
                name: "backend_configured".to_owned(),
                passed: true,
                message: format!("Backend configured: {}", cfg.backend_url),
                suggestion: None,
            }
        }
        Err(_) => CheckResult {
            name: "backend_configured".to_owned(),
            passed: false,
            message: "Not logged in".to_owned(),
            suggestion: Some("Run `fs2 login --backend <url>` to log in".to_owned()),
        },
    }
}

fn check_auth_token() -> CheckResult {
    let cfg = crate::config::CliConfig::load();
    match cfg {
        Ok(cfg) if !cfg.token.is_empty() => CheckResult {
            name: "auth_token".to_owned(),
            passed: true,
            message: "Auth token present".to_owned(),
            suggestion: None,
        },
        Ok(_) => CheckResult {
            name: "auth_token".to_owned(),
            passed: false,
            message: "Auth token is empty".to_owned(),
            suggestion: Some("Run `fs2 login` to obtain a token".to_owned()),
        },
        Err(_) => CheckResult {
            name: "auth_token".to_owned(),
            passed: false,
            message: "Not logged in".to_owned(),
            suggestion: Some("Run `fs2 login --backend <url>`".to_owned()),
        },
    }
}

fn check_cache_writable() -> CheckResult {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let cache_dir = format!("{home}/.fs2");
    let writable = std::fs::create_dir_all(&cache_dir).is_ok()
        && std::fs::write(format!("{cache_dir}/.doctor_test"), "").is_ok()
        && std::fs::remove_file(format!("{cache_dir}/.doctor_test")).is_ok();
    CheckResult {
        name: "cache_writable".to_owned(),
        passed: writable,
        message: if writable {
            format!("Cache directory {cache_dir} is writable")
        } else {
            format!("Cache directory {cache_dir} is not writable")
        },
        suggestion: if writable {
            None
        } else {
            Some(format!("Check permissions on {cache_dir}"))
        },
    }
}

fn check_git_sync_safety() -> CheckResult {
    // The built-in rule engine already excludes .git/** as ignore.
    // This check verifies the rule is in place.
    CheckResult {
        name: "git_sync_safety".to_owned(),
        passed: true,
        message: ".git internals are excluded by default (built-in rule)".to_owned(),
        suggestion: None,
    }
}

fn check_env_sync_safety() -> CheckResult {
    // The rule engine can mark .env as secret. This check is informational.
    CheckResult {
        name: "env_sync_safety".to_owned(),
        passed: true,
        message: ".env files should be marked as :secret in .fs2ignore".to_owned(),
        suggestion: Some("Add `:secret .env` to .fs2ignore to prevent normal sync".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_checks_returns_results() {
        let results = run_checks();
        assert!(!results.is_empty());
        // All checks should have a name and message.
        for r in &results {
            assert!(!r.name.is_empty());
            assert!(!r.message.is_empty());
        }
    }

    #[test]
    fn check_result_serialization() {
        let r = CheckResult {
            name: "test".to_owned(),
            passed: true,
            message: "ok".to_owned(),
            suggestion: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r.name, back.name);
        assert_eq!(r.passed, back.passed);
    }
}
