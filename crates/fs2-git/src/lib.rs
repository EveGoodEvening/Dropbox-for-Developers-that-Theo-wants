#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]
//! Git repository detection for FS2 diagnostics.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

/// Returns the crate name for smoke tests and early workspace validation.
#[must_use]
pub const fn crate_name() -> &'static str {
    "fs2-git"
}

/// Git inspection error.
#[derive(Debug)]
pub enum GitError {
    Io(std::io::Error),
    Command { args: Vec<String>, stderr: String },
    NotRepository(PathBuf),
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "git IO error: {error}"),
            Self::Command { args, stderr } => {
                write!(formatter, "git {} failed: {stderr}", args.join(" "))
            }
            Self::NotRepository(path) => {
                write!(formatter, "not a git repository: {}", path.display())
            }
        }
    }
}

impl std::error::Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// How the worktree points at its git directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRepositoryKind {
    Standard,
    Worktree,
}

/// Remote name and URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRemote {
    pub name: String,
    pub url: String,
}

/// Dirty worktree entry from porcelain status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyEntry {
    pub code: String,
    pub path: String,
}

/// Parsed `.gitmodules` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSubmodule {
    pub name: String,
    pub path: String,
    pub url: Option<String>,
    pub commit: Option<String>,
}

/// Useful state for `fs2 git status` and backend metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepositoryStatus {
    pub worktree_root: PathBuf,
    pub git_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub kind: GitRepositoryKind,
    pub remotes: Vec<GitRemote>,
    pub current_branch: Option<String>,
    pub head_commit: Option<String>,
    pub dirty_entries: Vec<DirtyEntry>,
    pub submodules: Vec<GitSubmodule>,
}

impl GitRepositoryStatus {
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.dirty_entries.is_empty()
    }

    #[must_use]
    pub fn has_submodules(&self) -> bool {
        !self.submodules.is_empty()
    }
}

/// Detects the repository containing `path`.
pub fn detect_repository(path: impl AsRef<Path>) -> Result<GitRepositoryStatus, GitError> {
    let path = path.as_ref();
    let command_dir = git_command_dir(path);
    if !git_success(&command_dir, &["rev-parse", "--is-inside-work-tree"])? {
        return Err(GitError::NotRepository(path.to_path_buf()));
    }

    let worktree_root = absolute_git_path(
        &command_dir,
        &git_output(&command_dir, &["rev-parse", "--show-toplevel"])?,
    );
    let git_dir = absolute_git_path(
        &command_dir,
        &git_output(&command_dir, &["rev-parse", "--git-dir"])?,
    );
    let common_git_dir = absolute_git_path(
        &command_dir,
        &git_output(&command_dir, &["rev-parse", "--git-common-dir"])?,
    );
    let kind = if worktree_root.join(".git").is_file() || git_dir != common_git_dir {
        GitRepositoryKind::Worktree
    } else {
        GitRepositoryKind::Standard
    };
    let mut submodules = parse_gitmodules(&worktree_root)?;
    attach_submodule_commits(
        &mut submodules,
        optional_git_output(&worktree_root, &["submodule", "status", "--recursive"])
            .transpose()?
            .as_deref()
            .unwrap_or(""),
    );
    Ok(GitRepositoryStatus {
        remotes: parse_remotes(&git_output(&worktree_root, &["remote", "-v"])?),
        current_branch: optional_git_output(
            &worktree_root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .transpose()?,
        head_commit: optional_git_output(&worktree_root, &["rev-parse", "--verify", "HEAD"])
            .transpose()?,
        dirty_entries: parse_dirty_entries(&git_output(
            &worktree_root,
            &["status", "--porcelain=v1"],
        )?),
        submodules,
        worktree_root,
        git_dir,
        common_git_dir,
        kind,
    })
}

fn git_command_dir(path: &Path) -> PathBuf {
    if path.is_file() {
        path.parent().map_or_else(
            || PathBuf::from("."),
            |parent| {
                if parent.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    parent.to_path_buf()
                }
            },
        )
    } else {
        path.to_path_buf()
    }
}

fn absolute_git_path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value.trim());
    let full_path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    full_path.canonicalize().unwrap_or(full_path)
}

fn git_success(path: &Path, args: &[&str]) -> Result<bool, GitError> {
    let output = Command::new("git").args(args).current_dir(path).output()?;
    Ok(output.status.success())
}

fn git_output(path: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git").args(args).current_dir(path).output()?;
    if !output.status.success() {
        return Err(GitError::Command {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn optional_git_output(path: &Path, args: &[&str]) -> Option<Result<String, GitError>> {
    let output = Command::new("git").args(args).current_dir(path).output();
    match output {
        Ok(output) if output.status.success() => Some(Ok(String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_owned())),
        Ok(_) => None,
        Err(error) => Some(Err(GitError::Io(error))),
    }
}

fn parse_remotes(value: &str) -> Vec<GitRemote> {
    let mut remotes = BTreeMap::new();
    for line in value.lines() {
        let Some((name, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some((url, kind)) = rest.trim().rsplit_once(' ') else {
            continue;
        };
        if kind == "(fetch)" {
            remotes.insert(name.to_owned(), url.to_owned());
        }
    }
    remotes
        .into_iter()
        .map(|(name, url)| GitRemote { name, url })
        .collect()
}

fn parse_dirty_entries(value: &str) -> Vec<DirtyEntry> {
    value
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            Some(DirtyEntry {
                code: line[..2].to_owned(),
                path: line[3..].to_owned(),
            })
        })
        .collect()
}

fn parse_gitmodules(root: &Path) -> Result<Vec<GitSubmodule>, GitError> {
    let path = root.join(".gitmodules");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    let mut current_name = None;
    let mut current_path = None;
    let mut current_url = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with("[submodule ") {
            push_submodule(
                &mut entries,
                &mut current_name,
                &mut current_path,
                &mut current_url,
            );
            current_name = line
                .split('"')
                .nth(1)
                .map(str::to_owned)
                .or_else(|| Some(line.to_owned()));
        } else if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "path" => current_path = Some(value.trim().to_owned()),
                "url" => current_url = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    push_submodule(
        &mut entries,
        &mut current_name,
        &mut current_path,
        &mut current_url,
    );
    Ok(entries)
}

fn attach_submodule_commits(submodules: &mut [GitSubmodule], status: &str) {
    let commits = parse_submodule_commits(status);
    for submodule in submodules {
        if let Some(commit) = commits.get(&submodule.path) {
            submodule.commit = Some(commit.clone());
        }
    }
}

fn parse_submodule_commits(status: &str) -> BTreeMap<String, String> {
    let mut commits = BTreeMap::new();
    for line in status.lines() {
        let line = line.trim_start_matches([' ', '-', '+', 'U']);
        let mut parts = line.split_whitespace();
        let Some(commit) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        if commit.len() == 40 && commit.chars().all(|ch| ch.is_ascii_hexdigit()) {
            commits.insert(path.to_owned(), commit.to_owned());
        }
    }
    commits
}

fn push_submodule(
    entries: &mut Vec<GitSubmodule>,
    name: &mut Option<String>,
    path: &mut Option<String>,
    url: &mut Option<String>,
) {
    if let (Some(name), Some(path)) = (name.take(), path.take()) {
        entries.push(GitSubmodule {
            name,
            path,
            url: url.take(),
            commit: None,
        });
    }
    *url = None;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use std::{fs, path::Path};
    use tempfile::TempDir;

    #[test]
    fn exposes_crate_name() {
        assert_eq!(crate_name(), "fs2-git");
    }

    #[test]
    fn parses_submodule_status_commits() {
        let commits = parse_submodule_commits(
            " aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa vendor/lib (heads/main)\n-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb third_party/missing\n",
        );

        assert_eq!(
            commits.get("vendor/lib").map(String::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            commits.get("third_party/missing").map(String::as_str),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }
    #[test]
    fn detects_normal_repo_state() -> Result<(), Box<dyn std::error::Error>> {
        let repo = init_repo()?;
        run_git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        )?;
        fs::write(repo.path().join("changed.txt"), "dirty")?;
        fs::write(
            repo.path().join(".gitmodules"),
            "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = https://example.invalid/lib.git\n",
        )?;

        let status = detect_repository(repo.path())?;

        assert_eq!(status.kind, GitRepositoryKind::Standard);
        assert_eq!(status.current_branch.as_deref(), Some("main"));
        assert!(status
            .head_commit
            .as_ref()
            .is_some_and(|head| head.len() == 40));
        assert_eq!(status.remotes[0].name, "origin");
        assert!(status.is_dirty());
        assert_eq!(status.submodules[0].path, "vendor/lib");
        assert_eq!(
            status.submodules[0].url.as_deref(),
            Some("https://example.invalid/lib.git")
        );
        Ok(())
    }

    #[test]
    fn detects_repo_from_subdirectory_and_file_path() -> Result<(), Box<dyn std::error::Error>> {
        let repo = init_repo()?;
        let nested = repo.path().join("crates/fs2-cli");
        fs::create_dir_all(&nested)?;
        let nested_file = nested.join("Cargo.toml");
        fs::write(&nested_file, "[package]\nname = \"demo\"\n")?;

        let from_dir = detect_repository(&nested)?;
        let from_file = detect_repository(&nested_file)?;

        assert_eq!(from_dir.kind, GitRepositoryKind::Standard);
        assert_eq!(from_file.kind, GitRepositoryKind::Standard);
        assert_eq!(from_dir.git_dir, from_dir.common_git_dir);
        assert_eq!(from_file.git_dir, from_file.common_git_dir);
        Ok(())
    }

    #[test]
    fn detects_worktree_git_file() -> Result<(), Box<dyn std::error::Error>> {
        let repo = init_repo()?;
        let worktree = TempDir::new()?;
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--detach",
                worktree.path().to_str().expect("utf8 path"),
            ],
        )?;

        let status = detect_repository(worktree.path())?;

        assert_eq!(status.kind, GitRepositoryKind::Worktree);
        assert!(worktree.path().join(".git").is_file());
        Ok(())
    }

    #[test]
    fn rejects_non_repo() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;

        assert!(matches!(
            detect_repository(dir.path()),
            Err(GitError::NotRepository(_))
        ));
        Ok(())
    }

    fn init_repo() -> Result<TempDir, Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        run_git(repo.path(), &["init", "--initial-branch=main"])?;
        run_git(
            repo.path(),
            &["config", "user.email", "fs2@example.invalid"],
        )?;
        run_git(repo.path(), &["config", "user.name", "FS2 Tests"])?;
        fs::write(repo.path().join("README.md"), "hello")?;
        run_git(repo.path(), &["add", "README.md"])?;
        run_git(repo.path(), &["commit", "-m", "initial"])?;
        Ok(repo)
    }

    fn run_git(path: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        let output = Command::new("git").args(args).current_dir(path).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("git {} failed", args.join(" ")).into())
        }
    }
}
