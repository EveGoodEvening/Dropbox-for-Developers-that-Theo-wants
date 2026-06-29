//! Typed identifier wrappers.
//!
//! IDs are opaque stable identifiers. They are kept as newtypes around `Uuid`
//! (or `i64` for [`Cursor`]) so they do not appear as raw `Uuid` throughout
//! the codebase except at serialization boundaries.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident, $sname:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a new random identifier using the v4 scheme.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wrap an existing [`Uuid`].
            #[must_use]
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// Return the underlying [`Uuid`].
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        // Prevent accidental `as` casts; newtypes should not be raw Uuids.
        // (No negative impl needed: distinct newtypes are already distinct types.)
        const _: () = { let _ = $sname; };
    };
}

uuid_id!(
    /// User identifier.
    UserId,
    "UserId"
);
uuid_id!(
    /// Workspace identifier.
    WorkspaceId,
    "WorkspaceId"
);
uuid_id!(
    /// Device identifier.
    DeviceId,
    "DeviceId"
);
uuid_id!(
    /// Node identifier. Stable across renames and moves.
    NodeId,
    "NodeId"
);
uuid_id!(
    /// Revision identifier for a node.
    RevisionId,
    "RevisionId"
);
uuid_id!(
    /// Operation identifier. Client-generated and stable for idempotency.
    OpId,
    "OpId"
);

/// Monotonically increasing per-workspace operation cursor.
///
/// Clients replay operations ordered by cursor to reconstruct workspace state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
#[serde(transparent)]
pub struct Cursor(pub i64);

impl Cursor {
    /// Zero cursor, the state before any operation has been committed.
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Advance to the next cursor value.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Raw value.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Cursor {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<i64>().map(Self)
    }
}

impl From<i64> for Cursor {
    fn from(v: i64) -> Self {
        Self(v)
    }
}

/// Content-addressed blob identifier.
///
/// Format: `<algo>:<hex>`, e.g. `sha256:<ciphertext_hash>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct BlobId(pub String);

impl BlobId {
    /// Create a blob id from an algorithm name and hex digest.
    #[must_use]
    pub fn new(algo: &str, hex_digest: &str) -> Self {
        Self(format!("{algo}:{hex_digest}"))
    }

    /// Parse a `algo:hex` string into a [`BlobId`], validating the format.
    ///
    /// # Errors
    /// Returns an error if the string is empty, lacks a `:` separator, has an
    /// empty algorithm, or has an empty digest.
    pub fn parse(s: &str) -> Result<Self, BlobIdParseError> {
        let (algo, digest) = s
            .split_once(':')
            .ok_or(BlobIdParseError::MissingSeparator)?;
        if algo.is_empty() {
            return Err(BlobIdParseError::EmptyAlgorithm);
        }
        if digest.is_empty() {
            return Err(BlobIdParseError::EmptyDigest);
        }
        Ok(Self(s.to_owned()))
    }

    /// Return the raw `algo:hex` string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Error returned by [`BlobId::parse`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlobIdParseError {
    /// No `:` separator found.
    #[error("blob id missing ':' separator")]
    MissingSeparator,
    /// Algorithm prefix is empty.
    #[error("blob id has empty algorithm")]
    EmptyAlgorithm,
    /// Digest is empty.
    #[error("blob id has empty digest")]
    EmptyDigest,
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BlobId {
    type Err = BlobIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn uuid_id_roundtrip() {
        let id = WorkspaceId::new();
        let s = id.to_string();
        let parsed: WorkspaceId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn cursor_advances() {
        assert_eq!(Cursor::zero().as_i64(), 0);
        assert_eq!(Cursor::zero().next().as_i64(), 1);
        assert!(Cursor::from(5) > Cursor::from(3));
    }

    #[test]
    fn blob_id_parse_valid() {
        let id = BlobId::new("sha256", "abcd");
        assert_eq!(id.as_str(), "sha256:abcd");
        let parsed: BlobId = "sha256:abcd".parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn blob_id_parse_errors() {
        assert_eq!(
            BlobId::parse("nope").unwrap_err(),
            BlobIdParseError::MissingSeparator
        );
        assert_eq!(
            BlobId::parse(":abcd").unwrap_err(),
            BlobIdParseError::EmptyAlgorithm
        );
        assert_eq!(
            BlobId::parse("sha256:").unwrap_err(),
            BlobIdParseError::EmptyDigest
        );
    }

    #[test]
    fn id_json_roundtrip() {
        let id = NodeId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn cursor_json_roundtrip() {
        let c = Cursor::from(42);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "42");
        let back: Cursor = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    proptest! {
        #[test]
        fn prop_uuid_id_display_parse_roundtrip(s in "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}") {
            let id = WorkspaceId::from_str(&s).unwrap();
            let display = id.to_string();
            let parsed: WorkspaceId = display.parse().unwrap();
            prop_assert_eq!(id, parsed);
        }

        #[test]
        fn prop_uuid_id_json_roundtrip(s in "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}") {
            let id = NodeId::from_str(&s).unwrap();
            let json = serde_json::to_string(&id).unwrap();
            let back: NodeId = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(id, back);
        }

        #[test]
        fn prop_cursor_json_roundtrip(v in -1000i64..1000) {
            let c = Cursor::from(v);
            let json = serde_json::to_string(&c).unwrap();
            let back: Cursor = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(c, back);
        }

        #[test]
        fn prop_cursor_parse_roundtrip(v in -1000i64..1000) {
            let c = Cursor::from(v);
            let s = c.to_string();
            let back: Cursor = s.parse().unwrap();
            prop_assert_eq!(c, back);
        }

        #[test]
        fn prop_blob_id_roundtrip(algo in "[a-z]{2,10}", digest in "[0-9a-f]{4,64}") {
            let raw = format!("{algo}:{digest}");
            let id = BlobId::parse(&raw).unwrap();
            prop_assert_eq!(id.as_str(), raw);
            let s = id.to_string();
            let back: BlobId = s.parse().unwrap();
            prop_assert_eq!(id, back);
        }
    }
}
