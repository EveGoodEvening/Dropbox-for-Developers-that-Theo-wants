//! Sync engine: API client, outbound/inbound loops, operation replay.
//!
//! The local `SQLite` store lives here because the sync engine owns local state
//! and operation replay.

pub mod api_client;
pub mod local_store;

pub use api_client::ApiClient;
pub use local_store::{LocalStore, LocalStoreError, LocalStoreResult};
