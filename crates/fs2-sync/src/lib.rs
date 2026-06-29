//! Sync engine: API client, outbound/inbound loops, operation replay.
//!
//! The local `SQLite` store lives here because the sync engine owns local state
//! and operation replay.

pub mod local_store;

pub use local_store::{LocalStore, LocalStoreError, LocalStoreResult};
