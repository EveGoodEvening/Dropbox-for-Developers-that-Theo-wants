//! Rule actions and their semantics.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The eight actions a rule can assign to a path.
///
/// See `design.md` §8.2 for the meaning of each action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Do not sync metadata or content. Hide remote ignored paths in the mount.
    Ignore,
    /// Allow path to exist locally, never upload. Other machines do not see it.
    LocalOnly,
    /// Path is produced from source/lockfiles; do not sync content. May be rebuilt.
    Generated,
    /// Sync metadata immediately, hydrate content on access.
    Lazy,
    /// Sync metadata and proactively hydrate content. Do not evict.
    Pin,
    /// Sync metadata and content normally, but content may be evicted unless pinned.
    #[default]
    Normal,
    /// Treat path as a materialized secret file. Contents come from secret store.
    Secret,
    /// Special generated dependency directory with package-manager integration.
    DependencyCache,
}

impl Action {
    /// Stable kebab-case wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::LocalOnly => "local-only",
            Self::Generated => "generated",
            Self::Lazy => "lazy",
            Self::Pin => "pin",
            Self::Normal => "normal",
            Self::Secret => "secret",
            Self::DependencyCache => "dependency-cache",
        }
    }

    /// Parse a wire string into an [`Action`].
    #[must_use]
    pub fn parse_tag(s: &str) -> Option<Self> {
        Some(match s {
            "ignore" => Self::Ignore,
            "local-only" => Self::LocalOnly,
            "generated" => Self::Generated,
            "lazy" => Self::Lazy,
            "pin" => Self::Pin,
            "normal" => Self::Normal,
            "secret" => Self::Secret,
            "dependency-cache" => Self::DependencyCache,
            _ => return None,
        })
    }

    /// Whether this action suppresses upload of file content.
    ///
    /// `ignore`, `local-only`, `generated`, and `dependency-cache` never
    /// produce upload queue entries. This is the check the daemon runs before
    /// enqueueing a write (see `design.md` §25.4).
    #[must_use]
    pub fn suppresses_upload(self) -> bool {
        matches!(
            self,
            Self::Ignore | Self::LocalOnly | Self::Generated | Self::DependencyCache
        )
    }

    /// Whether this action suppresses download of file content.
    #[must_use]
    pub fn suppresses_download(self) -> bool {
        matches!(self, Self::Ignore | Self::Generated | Self::DependencyCache)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Action {
    type Err = UnknownAction;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_tag(s).ok_or_else(|| UnknownAction(s.to_owned()))
    }
}

/// Error returned when an action name is not recognized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAction(pub String);

impl fmt::Display for UnknownAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown rule action: {:?}", self.0)
    }
}

impl std::error::Error for UnknownAction {}

/// Whether an env value is an encrypted secret or plain config.
///
/// Both are encrypted at rest with the workspace secret key; this distinction
/// controls redaction in CLI output and materialization behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// Encrypted and redacted in all output.
    Secret,
    /// Synced as config, still encrypted at rest.
    #[default]
    PlainConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_roundtrip() {
        for a in [
            Action::Ignore,
            Action::LocalOnly,
            Action::Generated,
            Action::Lazy,
            Action::Pin,
            Action::Normal,
            Action::Secret,
            Action::DependencyCache,
        ] {
            assert_eq!(Action::parse_tag(a.as_str()), Some(a));
            let parsed: Action = a.as_str().parse().unwrap();
            assert_eq!(parsed, a);
        }
        assert_eq!(Action::parse_tag("nope"), None);
        assert!("nope".parse::<Action>().is_err());
    }

    #[test]
    fn suppresses_upload_for_generated_and_local_only() {
        assert!(Action::Generated.suppresses_upload());
        assert!(Action::LocalOnly.suppresses_upload());
        assert!(Action::Ignore.suppresses_upload());
        assert!(Action::DependencyCache.suppresses_upload());
        assert!(!Action::Normal.suppresses_upload());
        assert!(!Action::Pin.suppresses_upload());
        assert!(!Action::Lazy.suppresses_upload());
    }

    #[test]
    fn suppresses_download_for_generated_but_not_local_only() {
        assert!(Action::Generated.suppresses_download());
        assert!(!Action::LocalOnly.suppresses_download());
        assert!(Action::DependencyCache.suppresses_download());
        assert!(!Action::Normal.suppresses_download());
    }
}
