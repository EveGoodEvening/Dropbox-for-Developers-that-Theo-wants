//! Hosted control plane: Axum server, metadata store, blob service.
//!
//! For local development without Postgres, the backend uses an in-memory
//! metadata store. When Postgres is available, the migration SQL files in
//! `migrations/postgres/` can be applied and the store swapped.

pub mod auth;
pub mod blob_store;
pub mod config;
pub mod error;
pub mod routes;
pub mod store;

pub use config::BackendConfig;
pub use error::{BackendError, BackendResult};
pub use store::MemoryStore;
