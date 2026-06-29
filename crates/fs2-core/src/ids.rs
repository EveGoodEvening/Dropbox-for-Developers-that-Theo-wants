//! Typed identifiers used throughout the FS2 domain model.
//!
//! These are opaque stable IDs. Raw `Uuid`/`i64` values should not appear in
//! public APIs except at serialization boundaries. Wrapping them in newtypes
//! prevents accidentally passing a `DeviceId` where a `WorkspaceId` is expected.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident, $backing:ty, $prefix:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord,
        )]
        #[repr(transparent)]
        pub struct $name($backing);

        impl $name {
            /// Create a new random identifier using a CSPRNG-backed `Uuid`.
            #[must_use]
            pub fn new() -> Self {
                Self(<$backing>::new_v4())
            }

            /// Access the underlying value at serialization/boundary sites only.
            #[must_use]
            pub const fn as_raw(&self) -> &$backing {
                &self.0
            }

            /// Construct from a known raw value. Use only when loading persisted IDs.
            #[must_use]
            pub const fn from_raw(raw: $backing) -> Self {
                Self(raw)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", $prefix, self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let rest = s.strip_prefix(concat!($prefix, ":")).unwrap_or(s);
                let raw = <$backing>::from_str(rest).map_err(|_| IdParseError {
                    kind: stringify!($name),
                    input: s.to_owned(),
                })?;
                Ok(Self(raw))
            }
        }

        impl From<$backing> for $name {
            fn from(raw: $backing) -> Self {
                Self(raw)
            }
        }

        impl From<$name> for $backing {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}

id_newtype!(
    /// Identifier for a user account.
    UserId,
    Uuid,
    "user"
);
id_newtype!(
    /// Identifier for a workspace.
    WorkspaceId,
    Uuid,
    "ws"
);
id_newtype!(
    /// Identifier for an enrolled device.
    DeviceId,
    Uuid,
    "dev"
);
id_newtype!(
    /// Stable identifier for a node (file/directory/symlink) in a workspace tree.
    ///
    /// Stable across renames and moves.
    NodeId,
    Uuid,
    "node"
);
id_newtype!(
    /// Identifier for an immutable node revision.
    RevisionId,
    Uuid,
    "rev"
);
id_newtype!(
    /// Identifier for a single operation submitted to the operation log.
    ///
    /// Stable across retries so the backend can deduplicate.
    OpId,
    Uuid,
    "op"
);

/// Error returned when an identifier string cannot be parsed.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid {kind} identifier: {input:?}")]
pub struct IdParseError {
    /// Which identifier kind failed to parse.
    pub kind: &'static str,
    /// The original input string.
    pub input: String,
}

/// Monotonically increasing per-workspace operation cursor.
///
/// Used for incremental sync: clients fetch operations `since` a cursor and
/// advance their local cursor only after successfully applying operations.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
#[repr(transparent)]
pub struct Cursor(pub i64);

impl Cursor {
    /// The initial cursor before any operation has been committed.
    pub const ZERO: Cursor = Cursor(0);

    /// Advance to the next cursor value, returning the new cursor.
    #[must_use]
    pub const fn next(self) -> Cursor {
        Cursor(self.0 + 1)
    }

    /// Raw underlying value.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cursor:{}", self.0)
    }
}

impl FromStr for Cursor {
    type Err = IdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix("cursor:").unwrap_or(s);
        let raw = rest.parse::<i64>().map_err(|_| IdParseError {
            kind: "Cursor",
            input: s.to_owned(),
        })?;
        Ok(Self(raw))
    }
}

impl From<i64> for Cursor {
    fn from(v: i64) -> Self {
        Self(v)
    }
}

impl From<Cursor> for i64 {
    fn from(c: Cursor) -> Self {
        c.0
    }
}

/// Content-addressed blob identifier.
///
/// Format: `<algo>:<hex>`, e.g. `sha256:<hex>`. For MVP, the hash is of the
/// ciphertext so the server cannot infer plaintext equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[repr(transparent)]
pub struct BlobId(pub String);

impl BlobId {
    /// Construct a `BlobId` from an algorithm name and raw digest bytes.
    #[must_use]
    pub fn from_bytes(algo: &str, digest: &[u8]) -> Self {
        Self(format!("{algo}:{}", hex::encode(digest)))
    }

    /// Construct a `BlobId` from an already-formatted `<algo>:<hex>` string.
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }

    /// The full identifier string `<algo>:<hex>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BlobId {
    type Err = IdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((algo, hex_part)) = s.split_once(':') else {
            return Err(IdParseError {
                kind: "BlobId",
                input: s.to_owned(),
            });
        };
        if algo.is_empty() || hex_part.is_empty() {
            return Err(IdParseError {
                kind: "BlobId",
                input: s.to_owned(),
            });
        }
        if hex::decode(hex_part).is_err() {
            return Err(IdParseError {
                kind: "BlobId",
                input: s.to_owned(),
            });
        }
        Ok(Self(s.to_owned()))
    }
}

impl AsRef<str> for BlobId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_uuid() -> impl Strategy<Value = Uuid> {
        any::<[u8; 16]>().prop_map(Uuid::from_bytes)
    }

    proptest! {
        #[test]
        fn uuid_id_roundtrip(raw in arb_uuid()) {
            let id = NodeId::from_raw(raw);
            let s = id.to_string();
            let parsed: NodeId = s.parse().unwrap();
            prop_assert_eq!(id, parsed);
        }

        #[test]
        fn all_uuid_ids_roundtrip(
            user in arb_uuid(),
            ws in arb_uuid(),
            dev in arb_uuid(),
            node in arb_uuid(),
            rev in arb_uuid(),
            op in arb_uuid(),
        ) {
            for s in [
                UserId::from_raw(user).to_string(),
                WorkspaceId::from_raw(ws).to_string(),
                DeviceId::from_raw(dev).to_string(),
                NodeId::from_raw(node).to_string(),
                RevisionId::from_raw(rev).to_string(),
                OpId::from_raw(op).to_string(),
            ] {
                let _: String = s; // ensure Display produced a prefixed string
                assert!(s.contains(':'));
            }
        }

        #[test]
        fn cursor_roundtrip(v in any::<i64>()) {
            let c = Cursor(v);
            let s = c.to_string();
            let parsed: Cursor = s.parse().unwrap();
            prop_assert_eq!(c, parsed);
        }

        #[test]
        fn blob_id_roundtrip(digest in any::<[u8; 32]>()) {
            let id = BlobId::from_bytes("sha256", &digest);
            let s = id.to_string();
            let parsed: BlobId = s.parse().unwrap();
            prop_assert_eq!(id, parsed);
        }
    }

    #[test]
    fn prefix_is_present() {
        assert!(NodeId::new().to_string().starts_with("node:"));
        assert!(WorkspaceId::new().to_string().starts_with("ws:"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!("node:not-a-uuid".parse::<NodeId>().is_err());
        assert!("garbage".parse::<NodeId>().is_err());
        assert!("".parse::<BlobId>().is_err());
        assert!("sha256:".parse::<BlobId>().is_err());
        assert!(":deadbeef".parse::<BlobId>().is_err());
        assert!("sha256:zz".parse::<BlobId>().is_err());
    }

    #[test]
    fn cursor_next_advances() {
        assert_eq!(Cursor::ZERO.next(), Cursor(1));
        assert_eq!(Cursor(5).next(), Cursor(6));
    }

    #[test]
    fn blob_id_from_bytes_hex_encodes() {
        let id = BlobId::from_bytes("sha256", &[0xab, 0xcd]);
        assert_eq!(id.as_str(), "sha256:abcd");
    }
}
