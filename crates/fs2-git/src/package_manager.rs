//! Package manager detection.
//!
//! Detects the package manager for a project by checking:
//! 1. `packageManager` field in `package.json`
//! 2. Lockfile presence (pnpm-lock.yaml, yarn.lock, package-lock.json, bun.lockb)
//! 3. Workspace files (pnpm-workspace.yaml, turbo.json, nx.json)
//!
//! Also detects Rust (Cargo.toml), Python (pyproject.toml, requirements.txt),
//! and Go (go.mod) project roots.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Detected package manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    /// Node.js with npm.
    Npm,
    /// Node.js with pnpm.
    Pnpm,
    /// Node.js with yarn.
    Yarn,
    /// Node.js with bun.
    Bun,
    /// Rust with cargo.
    Cargo,
    /// Python with pip/uv/poetry.
    Python,
    /// Go.
    Go,
    /// No package manager detected.
    None,
}

impl PackageManager {
    /// Returns the install command for this package manager.
    #[must_use]
    pub fn install_command(&self) -> &'static [&'static str] {
        match self {
            Self::Npm => &["npm", "install"],
            Self::Pnpm => &["pnpm", "install"],
            Self::Yarn => &["yarn", "install"],
            Self::Bun => &["bun", "install"],
            Self::Cargo => &["cargo", "build"],
            Self::Python => &["pip", "install", "-r", "requirements.txt"],
            Self::Go => &["go", "mod", "download"],
            Self::None => &[],
        }
    }

    /// Returns the human-readable name.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
            Self::Cargo => "cargo",
            Self::Python => "pip",
            Self::Go => "go",
            Self::None => "none",
        }
    }
}

/// Detect the package manager for a project directory.
///
/// Checks for lockfiles and manifest files in priority order.
#[must_use]
pub fn detect_package_manager(path: &Path) -> PackageManager {
    // Node.js: check packageManager field first.
    if path.join("package.json").exists() {
        if let Ok(content) = std::fs::read_to_string(path.join("package.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(pm) = json.get("packageManager").and_then(|v| v.as_str()) {
                    if pm.starts_with("pnpm") {
                        return PackageManager::Pnpm;
                    } else if pm.starts_with("yarn") {
                        return PackageManager::Yarn;
                    } else if pm.starts_with("npm") {
                        return PackageManager::Npm;
                    } else if pm.starts_with("bun") {
                        return PackageManager::Bun;
                    }
                }
            }
        }
        // Fallback to lockfile detection.
        if path.join("pnpm-lock.yaml").exists() {
            return PackageManager::Pnpm;
        }
        if path.join("yarn.lock").exists() {
            return PackageManager::Yarn;
        }
        if path.join("bun.lockb").exists() {
            return PackageManager::Bun;
        }
        if path.join("package-lock.json").exists() {
            return PackageManager::Npm;
        }
        // Default to npm if package.json exists but no lockfile.
        return PackageManager::Npm;
    }

    // Rust.
    if path.join("Cargo.toml").exists() {
        return PackageManager::Cargo;
    }

    // Python.
    if path.join("pyproject.toml").exists()
        || path.join("requirements.txt").exists()
        || path.join("requirements-dev.txt").exists()
    {
        return PackageManager::Python;
    }

    // Go.
    if path.join("go.mod").exists() {
        return PackageManager::Go;
    }

    PackageManager::None
}

/// Check if a path is a generated/dependency directory that should not be synced.
///
/// Returns the package manager that owns it, if any.
#[must_use]
pub fn is_generated_dir(name: &str) -> Option<PackageManager> {
    match name {
        "target" => Some(PackageManager::Cargo),
        ".venv" | "venv" | "__pycache__" | ".pytest_cache" => Some(PackageManager::Python),
        "node_modules" | ".next" | ".nuxt" | ".turbo" | "coverage" | "dist" => {
            Some(PackageManager::Npm)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detect_npm() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Npm);
    }

    #[test]
    fn detect_pnpm_via_lockfile() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Pnpm);
    }

    #[test]
    fn detect_pnpm_via_package_manager_field() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"packageManager": "pnpm@9.0.0"}"#,
        )
        .unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Pnpm);
    }

    #[test]
    fn detect_cargo() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Cargo);
    }

    #[test]
    fn detect_python() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("requirements.txt"), "requests").unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Python);
    }

    #[test]
    fn detect_go() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module test").unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Go);
    }

    #[test]
    fn detect_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::None);
    }

    #[test]
    fn is_generated_dir_node_modules() {
        assert!(is_generated_dir("node_modules").is_some());
        assert!(is_generated_dir("target").is_some());
        assert!(is_generated_dir(".venv").is_some());
        assert!(is_generated_dir("src").is_none());
    }

    #[test]
    fn install_commands() {
        assert_eq!(PackageManager::Pnpm.install_command(), &["pnpm", "install"]);
        assert_eq!(PackageManager::Cargo.install_command(), &["cargo", "build"]);
        assert_eq!(PackageManager::None.install_command(), &[] as &[&str]);
    }
}
