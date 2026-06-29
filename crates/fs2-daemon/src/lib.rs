//! Local daemon owning metadata DB, cache, queues, sync loops.
//!
//! The daemon initializes the local `SQLite` store, starts outbound and inbound
//! sync loops, and handles graceful shutdown.

pub mod sync;

pub use sync::{OutboundQueue, SyncState};
