//! Shared foundation crate for the Dropbox-like developer sync tool.
//!
//! CHUNK-01 owns the cross-cutting substrate only: config, logging, error
//! types, platform identity, migration baseline, module shells, and the
//! minimal `version`/`info` CLI smoke path.

pub mod catalog;
pub mod cli;
pub mod env;
pub mod foundation;
pub mod policy;
pub mod sync;
pub mod vfs;
pub mod watcher;

pub use foundation::{Config, ConfigPaths, ErrorCode, Platform, SyncError};

#[cfg(test)]
mod tests {
    #[test]
    fn no_op_test_harness_is_wired() {
        // CHUNK-01 intentionally has no product behavior yet. This test exists
        // so `make test` proves the Rust harness is executable from a clean clone.
    }
}
