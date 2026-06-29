//! Git-aware integration: detection, metadata, materialization.
//!
//! Detects Git repositories, parses remote URLs, branches, HEAD commits,
//! dirty status, and `.gitmodules`. Does not sync `.git` internals.

pub mod detect;

pub use detect::{detect_git, detect_gitmodules, GitMetadata, GitStatus};
