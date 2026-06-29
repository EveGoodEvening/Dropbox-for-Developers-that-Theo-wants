//! Shared domain model: IDs, nodes, revisions, operations, errors.
//!
//! These types are shared between the backend and the client so the two
//! cannot drift on the wire contract.

pub mod error;
pub mod id;
pub mod node;
pub mod op;
pub mod path;

pub use error::{Fs2Error, Fs2ErrorCode, Fs2Result};
pub use id::{BlobId, Cursor, DeviceId, NodeId, OpId, RevisionId, UserId, WorkspaceId};
pub use node::{Node, NodeKind, NodeRevision, RevisionContent};
pub use op::{Operation, OperationKind};
pub use path::{
    names_collide, normalized_name, portable_collision_key, CasePolicy, PathError, RelPath,
};
