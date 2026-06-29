//! Git repository detection and metadata extraction.
//!
//! Detects `.git` directory or file, parses remote URLs, current branch,
//! HEAD commit, dirty status summary, and `.gitmodules`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Git metadata for a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitMetadata {
    /// Whether a Git repository was detected.
    pub is_repo: bool,
    /// Remote URL (first remote).
    pub remote_url: Option<String>,
    /// Current branch name.
    pub branch: Option<String>,
    /// HEAD commit hash.
    pub head_commit: Option<String>,
    /// Whether the working tree has uncommitted changes.
    pub dirty: bool,
    /// Submodule paths (from `.gitmodules`).
    pub submodules: Vec<SubmoduleEntry>,
}

/// A submodule entry from `.gitmodules`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmoduleEntry {
    /// Submodule path.
    pub path: String,
    /// Submodule remote URL.
    pub url: String,
}

/// Git status summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GitStatus {
    /// Whether the working tree is dirty.
    pub dirty: bool,
    /// Number of modified files.
    pub modified: usize,
    /// Number of untracked files.
    pub untracked: usize,
    /// Number of staged files.
    pub staged: usize,
}

/// Detect Git metadata for a directory.
///
/// Checks for `.git` directory or file, then runs `git` commands to extract
/// metadata. If `git` is not installed or the directory is not a repo, returns
/// metadata with `is_repo = false`.
///
/// # Errors
/// Returns an error only if the path cannot be read.
pub fn detect_git(path: &Path) -> anyhow::Result<GitMetadata> {
    let git_dir = path.join(".git");
    let is_repo = git_dir.exists();

    if !is_repo {
        return Ok(GitMetadata {
            is_repo: false,
            remote_url: None,
            branch: None,
            head_commit: None,
            dirty: false,
            submodules: detect_gitmodules(path)?,
        });
    }

    // Run git commands to extract metadata.
    let remote_url = run_git(path, &["remote", "get-url", "origin"]).ok();
    let branch = run_git(path, &["rev-parse", "--abbrev-ref", "HEAD"]).ok();
    let head_commit = run_git(path, &["rev-parse", "HEAD"]).ok();
    let dirty = run_git(path, &["status", "--porcelain"]).is_ok_and(|s| !s.trim().is_empty());

    Ok(GitMetadata {
        is_repo: true,
        remote_url,
        branch,
        head_commit,
        dirty,
        submodules: detect_gitmodules(path)?,
    })
}

/// Detect submodules from `.gitmodules`.
///
/// # Errors
/// Returns an error if the `.gitmodules` file cannot be read.
pub fn detect_gitmodules(path: &Path) -> anyhow::Result<Vec<SubmoduleEntry>> {
    let gitmodules = path.join(".gitmodules");
    if !gitmodules.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&gitmodules)?;
    let mut submodules = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_url: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("[submodule ") {
            // Save previous submodule if complete.
            if let (Some(p), Some(u)) = (current_path.take(), current_url.take()) {
                submodules.push(SubmoduleEntry { path: p, url: u });
            }
        } else if let Some(rest) = line.strip_prefix("path = ") {
            current_path = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("url = ") {
            current_url = Some(rest.trim().to_owned());
        }
    }
    // Save last submodule.
    if let (Some(p), Some(u)) = (current_path, current_url) {
        submodules.push(SubmoduleEntry { path: p, url: u });
    }

    Ok(submodules)
}

fn run_git(path: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn detect_non_git_directory() {
        let tmp = TempDir::new().unwrap();
        let meta = detect_git(tmp.path()).unwrap();
        assert!(!meta.is_repo);
        assert!(meta.remote_url.is_none());
        assert!(meta.branch.is_none());
    }

    #[test]
    fn detect_git_directory() {
        let tmp = TempDir::new().unwrap();
        // Initialize a git repo.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        // Configure user for commits.
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let meta = detect_git(tmp.path()).unwrap();
        assert!(meta.is_repo);
        // A fresh repo without commits may not have a branch yet.
        // The important thing is that is_repo is true.
    }

    #[test]
    fn detect_gitmodules_empty() {
        let tmp = TempDir::new().unwrap();
        let subs = detect_gitmodules(tmp.path()).unwrap();
        assert!(subs.is_empty());
    }

    #[test]
    fn detect_gitmodules_parsed() {
        let tmp = TempDir::new().unwrap();
        let content = "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = https://github.com/example/lib.git\n[submodule \"vendor/other\"]\n\tpath = vendor/other\n\turl = https://github.com/example/other.git\n";
        fs::write(tmp.path().join(".gitmodules"), content).unwrap();
        let subs = detect_gitmodules(tmp.path()).unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].path, "vendor/lib");
        assert_eq!(subs[0].url, "https://github.com/example/lib.git");
        assert_eq!(subs[1].path, "vendor/other");
        assert_eq!(subs[1].url, "https://github.com/example/other.git");
    }

    #[test]
    fn git_metadata_serialization() {
        let meta = GitMetadata {
            is_repo: true,
            remote_url: Some("https://github.com/example/repo.git".to_owned()),
            branch: Some("main".to_owned()),
            head_commit: Some("abc123".to_owned()),
            dirty: false,
            submodules: vec![],
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: GitMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
    }
}
