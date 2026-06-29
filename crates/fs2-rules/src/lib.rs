//! `fs2-rules`: `.fs2ignore` and `.fs2/config.toml` rule engine.
//!
//! This crate answers the question "what should FS2 do with this path?" with
//! one of eight actions (`ignore`, `local-only`, `generated`, `lazy`, `pin`,
//! `normal`, `secret`, `dependency-cache`). Rules come from four sources, in
//! increasing precedence:
//!
//! 1. Built-in default profiles (Node, Rust, Python, Go).
//! 2. `.fs2ignore` file (gitignore-style globs with optional `:action` prefixes;
//!    last match wins).
//! 3. `.fs2/config.toml` structured rules (most-specific pattern wins).
//! 4. Explicit CLI override for a path.
//!
//! See `design.md` section 8 for the full rationale.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod action;
pub mod config;
pub mod engine;
pub mod fs2ignore;
pub mod profiles;

pub use action::{Action, SecretKind};
pub use config::{
    CacheConfig, ConfigRule, EnvConfig, EnvMaterializeMode, Fs2Config, GitConfig, GitMode,
};
pub use engine::{EffectiveRule, RuleEngine, RuleSource};
pub use fs2ignore::{Fs2IgnoreEntry, Fs2IgnoreFile, Fs2IgnoreParseError};
pub use profiles::builtin_profiles;
