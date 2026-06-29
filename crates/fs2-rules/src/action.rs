//! Rule actions and their parsing.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The sync action a rule assigns to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Do not sync metadata or content. Hide remote ignored paths in the mount.
    Ignore,
    /// Allow path to exist locally, never upload it. Other machines do not see it.
    LocalOnly,
    /// Path is produced from source/lockfiles; do not sync content. May be rebuilt.
    Generated,
    /// Sync metadata immediately, hydrate content on access.
    Lazy,
    /// Sync metadata and proactively hydrate content. Do not evict.
    Pin,
    /// Sync metadata and content normally, but content may still be evicted.
    Normal,
    /// Treat path as a materialized secret file. Contents come from secret store.
    Secret,
    /// Special generated dependency directory with package-manager integration.
    DependencyCache,
}

impl Action {
    /// All action variants in a stable order.
    #[must_use]
    pub fn all() -> &'static [Action] {
        &[
            Action::Ignore,
            Action::LocalOnly,
            Action::Generated,
            Action::Lazy,
            Action::Pin,
            Action::Normal,
            Action::Secret,
            Action::DependencyCache,
        ]
    }

    /// Stable string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Ignore => "ignore",
            Action::LocalOnly => "local-only",
            Action::Generated => "generated",
            Action::Lazy => "lazy",
            Action::Pin => "pin",
            Action::Normal => "normal",
            Action::Secret => "secret",
            Action::DependencyCache => "dependency-cache",
        }
    }

    /// Whether this action suppresses content upload.
    ///
    /// `ignore`, `local-only`, `generated`, and `dependency-cache` never
    /// produce upload queue entries.
    #[must_use]
    pub fn suppresses_upload(self) -> bool {
        matches!(
            self,
            Action::Ignore | Action::LocalOnly | Action::Generated | Action::DependencyCache
        )
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an action from a string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionParseError {
    /// The action name is not recognized.
    #[error("unknown action: {0}")]
    Unknown(String),
}

impl FromStr for Action {
    type Err = ActionParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ignore" => Ok(Self::Ignore),
            "local-only" => Ok(Self::LocalOnly),
            "generated" => Ok(Self::Generated),
            "lazy" => Ok(Self::Lazy),
            "pin" => Ok(Self::Pin),
            "normal" => Ok(Self::Normal),
            "secret" => Ok(Self::Secret),
            "dependency-cache" => Ok(Self::DependencyCache),
            other => Err(ActionParseError::Unknown(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_roundtrip() {
        for a in Action::all() {
            let s = a.as_str();
            assert_eq!(Action::from_str(s).unwrap(), *a);
        }
        assert!(Action::from_str("bogus").is_err());
    }

    #[test]
    fn suppresses_upload() {
        assert!(Action::Generated.suppresses_upload());
        assert!(Action::Ignore.suppresses_upload());
        assert!(Action::LocalOnly.suppresses_upload());
        assert!(Action::DependencyCache.suppresses_upload());
        assert!(!Action::Normal.suppresses_upload());
        assert!(!Action::Lazy.suppresses_upload());
        assert!(!Action::Pin.suppresses_upload());
    }
}
