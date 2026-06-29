//! Rule engine: `.fs2ignore` and `.fs2/config.toml` parsing, precedence, and
//! built-in language/tool profiles.
//!
//! The rule engine answers a richer question than `.gitignore`: not just
//! "should this be tracked" but "how should this path be synced" — ignored,
//! local-only, generated, lazy, pinned, normal, secret, or dependency-cache.

pub mod action;
pub mod config;
pub mod fs2ignore;
pub mod glob;
pub mod precedence;
pub mod profiles;

pub use action::{Action, ActionParseError};
pub use config::{Config, ConfigError, RuleEntry};
pub use fs2ignore::{Fs2IgnoreEntry, Fs2IgnoreError, Fs2IgnoreParser};
pub use precedence::{EffectiveRule, RuleEngine, RuleSource};
pub use profiles::builtin_profiles;
