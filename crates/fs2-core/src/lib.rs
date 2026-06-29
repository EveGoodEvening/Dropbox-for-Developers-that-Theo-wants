//! `fs2-core`: core domain model shared by client and backend.
//!
//! Contains typed identifiers, node/revision types, the operation log model,
//! and the structured error model. These types are intentionally kept in a
//! shared crate so the backend and client cannot drift on the wire contract.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod ids;
pub mod node;
pub mod op;
pub mod path;

pub use error::{ErrorCode, Fs2Error};
pub use ids::{BlobId, Cursor, DeviceId, NodeId, OpId, RevisionId, UserId, WorkspaceId};
pub use node::{Node, NodeKind, NodeRevision, RevisionContent};
pub use op::{Operation, OperationKind};
pub use path::{collision_key, CasePolicy, PathError, RelPath};
