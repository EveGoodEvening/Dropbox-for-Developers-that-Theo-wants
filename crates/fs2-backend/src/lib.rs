//! `fs2-backend`: Axum backend service with metadata, blob, auth, and env APIs.
//!
//! The MVP backend uses Postgres for authoritative metadata and S3-compatible
//! object storage for blobs. For development and testing, an in-memory store
//! is provided so the full API surface can be exercised without external
//! dependencies.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::too_many_lines)]

pub mod app;
pub mod auth;
pub mod config;
pub mod device;
pub mod store;

pub use app::run_server;
pub use config::BackendConfig;
pub use store::MemoryStore;
