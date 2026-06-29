//! Git-aware integration: detection, metadata, materialization.
//!
//! Detects Git repositories, parses remote URLs, branches, HEAD commits,
//! dirty status, and `.gitmodules`. Does not sync `.git` internals.

pub mod detect;
pub mod package_manager;

pub use detect::{detect_git, detect_gitmodules, GitMetadata, GitStatus};
pub use package_manager::{detect_package_manager, is_generated_dir, PackageManager};
