//! Workspace-relative path utilities and portable collision keys.
//!
//! Paths are derived from `parent_id + name`, not stored as authoritative
//! identity. This module provides:
//!
//! - [`RelPath`]: a workspace-relative path that rejects absolute paths, `..`
//!   traversal, null bytes, and empty segments (except the root).
//! - [`collision_key`]: a portable comparison key combining Unicode NFC
//!   normalization and case folding, so two siblings that differ only by
//!   case or normalization form are detected as collisions on portable
//!   workspaces.
//!
//! See `design.md` sections 5.5 and 25.8 for the rationale.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// Error returned when a workspace-relative path is invalid.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathError {
    /// Path was absolute (started with `/`).
    #[error("absolute paths are not allowed: {0:?}")]
    Absolute(String),
    /// Path contained a `..` traversal segment.
    #[error("path traversal (`..`) is not allowed: {0:?}")]
    Traversal(String),
    /// Path contained a null byte.
    #[error("null bytes are not allowed in paths: {0:?}")]
    NullByte(String),
    /// Path contained an empty segment (other than the root).
    #[error("empty path segment: {0:?}")]
    EmptySegment(String),
    /// Path was empty.
    #[error("empty path")]
    Empty,
}

/// A workspace-relative path.
///
/// Internally stored with `/` separators. The root is represented by an empty
/// segment list. Construct via [`RelPath::new`] or [`FromStr`], both of which
/// validate against the malicious-path rules in `design.md` §25.8.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelPath {
    segments: Vec<String>,
}

impl RelPath {
    /// The workspace root path (empty segment list).
    pub const ROOT: &'static RelPath = &RelPath {
        segments: Vec::new(),
    };

    /// Parse and validate a workspace-relative path string.
    ///
    /// # Errors
    /// Returns [`PathError`] if the path is absolute, contains `..`, null
    /// bytes, or empty segments.
    pub fn new(input: &str) -> Result<Self, PathError> {
        if input.is_empty() {
            return Ok(Self {
                segments: Vec::new(),
            });
        }
        if input.contains('\0') {
            return Err(PathError::NullByte(input.to_owned()));
        }
        if input.starts_with('/') {
            return Err(PathError::Absolute(input.to_owned()));
        }
        let mut segments = Vec::new();
        for seg in input.split('/') {
            if seg.is_empty() {
                return Err(PathError::EmptySegment(input.to_owned()));
            }
            if seg == ".." {
                return Err(PathError::Traversal(input.to_owned()));
            }
            // `.` (self) segments are allowed and silently dropped, matching
            // common path semantics. They should not appear in synced names
            // but are not a security hazard.
            if seg != "." {
                segments.push(seg.to_owned());
            }
        }
        Ok(Self { segments })
    }

    /// Construct a path from already-validated segments.
    ///
    /// Each segment must be non-empty and must not contain `/`, `\0`, or `..`.
    /// This is intended for internal use where segments come from trusted
    /// node names.
    ///
    /// # Errors
    /// Returns [`PathError`] if any segment is invalid.
    pub fn from_segments<I, S>(segments: I) -> Result<Self, PathError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Vec::new();
        for seg in segments {
            let seg = seg.as_ref();
            if seg.is_empty() {
                return Err(PathError::EmptySegment(seg.to_owned()));
            }
            if seg.contains('\0') {
                return Err(PathError::NullByte(seg.to_owned()));
            }
            if seg == ".." {
                return Err(PathError::Traversal(seg.to_owned()));
            }
            if seg.contains('/') {
                return Err(PathError::EmptySegment(seg.to_owned()));
            }
            out.push(seg.to_owned());
        }
        Ok(Self { segments: out })
    }

    /// Whether this is the workspace root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// The path segments, in order.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The final segment (filename), or `None` for the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// The parent path, or `None` if this is the root.
    #[must_use]
    pub fn parent(&self) -> Option<RelPath> {
        if self.segments.is_empty() {
            return None;
        }
        Some(RelPath {
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    /// Join a single child segment, validating it.
    ///
    /// # Errors
    /// Returns [`PathError`] if the child segment is invalid.
    pub fn join(&self, child: &str) -> Result<RelPath, PathError> {
        if child.is_empty() {
            return Err(PathError::EmptySegment(child.to_owned()));
        }
        if child.contains('\0') {
            return Err(PathError::NullByte(child.to_owned()));
        }
        if child == ".." || child.starts_with("../") || child.contains("/..") {
            return Err(PathError::Traversal(child.to_owned()));
        }
        // Validate each segment of the child if it contains slashes.
        let validated = Self::new(child)?;
        let mut segments = self.segments.clone();
        segments.extend(validated.segments);
        Ok(Self { segments })
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.segments.is_empty() {
            return f.write_str("/");
        }
        f.write_str(&self.segments.join("/"))
    }
}

impl FromStr for RelPath {
    type Err = PathError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Compute the portable collision key for a filename.
///
/// The key is the Unicode NFC-normalized, Unicode case-folded form of the
/// name. Two names that differ only by case or by normalization form produce
/// the same key, which is what portable workspaces must block to avoid
/// macOS/Linux filesystem hazards.
///
/// Case folding uses Rust's `char::to_lowercase` (Unicode Default Case
/// Folding) followed by NFC normalization, which is sufficient for the
/// practical `Foo.ts` vs `foo.ts` and composed-vs-decomposed hazards called
/// out in the design.
#[must_use]
pub fn collision_key(name: &str) -> String {
    let folded: String = name.chars().flat_map(char::to_lowercase).collect();
    folded.nfc().collect()
}

/// Case policy for a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CasePolicy {
    /// Block siblings that collide under NFC + case folding. Safe on both
    /// macOS (default case-insensitive APFS) and Linux.
    #[default]
    Portable,
    /// Allow siblings that differ only by case. Requires all enrolled devices
    /// to support case-sensitive filesystems.
    CaseSensitiveOnly,
}

impl CasePolicy {
    /// Stable string tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::CaseSensitiveOnly => "case-sensitive-only",
        }
    }

    /// Whether two sibling names collide under this policy.
    ///
    /// Under `Portable`, names collide if their [`collision_key`]s are equal.
    /// Under `CaseSensitiveOnly`, names only collide if they are byte-identical.
    #[must_use]
    pub fn names_collide(&self, a: &str, b: &str) -> bool {
        match self {
            Self::Portable => collision_key(a) == collision_key(b),
            Self::CaseSensitiveOnly => a == b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_empty() {
        let root = RelPath::new("").unwrap();
        assert!(root.is_root());
        assert!(root.segments().is_empty());
        assert_eq!(root.to_string(), "/");
        assert!(root.parent().is_none());
        assert!(root.file_name().is_none());
    }

    #[test]
    fn simple_path_parses() {
        let p = RelPath::new("apps/web/src/index.ts").unwrap();
        assert_eq!(p.segments(), &["apps", "web", "src", "index.ts"]);
        assert_eq!(p.file_name(), Some("index.ts"));
        assert_eq!(p.parent().unwrap().to_string(), "apps/web/src");
    }

    #[test]
    fn dot_segments_are_dropped() {
        let p = RelPath::new("apps/./web").unwrap();
        assert_eq!(p.segments(), &["apps", "web"]);
    }

    #[test]
    fn rejects_absolute() {
        assert_eq!(
            RelPath::new("/etc/passwd"),
            Err(PathError::Absolute("/etc/passwd".to_owned()))
        );
    }

    #[test]
    fn rejects_traversal() {
        assert!(matches!(
            RelPath::new("../secret"),
            Err(PathError::Traversal(_))
        ));
        assert!(matches!(
            RelPath::new("apps/../../etc"),
            Err(PathError::Traversal(_))
        ));
    }

    #[test]
    fn rejects_null_bytes() {
        assert!(matches!(
            RelPath::new("apps\0web"),
            Err(PathError::NullByte(_))
        ));
    }

    #[test]
    fn rejects_empty_segments() {
        assert!(matches!(
            RelPath::new("apps//web"),
            Err(PathError::EmptySegment(_))
        ));
        assert!(matches!(
            RelPath::new("apps/web/"),
            Err(PathError::EmptySegment(_))
        ));
    }

    #[test]
    fn join_validates_child() {
        let root = RelPath::ROOT;
        let child = root.join("apps").unwrap();
        assert_eq!(child.to_string(), "apps");
        assert!(root.join("../escape").is_err());
        assert!(root.join("a\0b").is_err());
        assert!(root.join("").is_err());
        let nested = child.join("web/package.json").unwrap();
        assert_eq!(nested.to_string(), "apps/web/package.json");
    }

    #[test]
    fn from_segments_validates() {
        let p = RelPath::from_segments(["apps", "web"]).unwrap();
        assert_eq!(p.to_string(), "apps/web");
        assert!(RelPath::from_segments(["apps", ""]).is_err());
        assert!(RelPath::from_segments(["apps", ".."]).is_err());
        assert!(RelPath::from_segments(["apps\0"]).is_err());
        assert!(RelPath::from_segments(["a/b"]).is_err());
    }

    #[test]
    fn collision_key_case_fold() {
        assert_eq!(collision_key("Foo.ts"), collision_key("foo.ts"));
        assert_eq!(collision_key("FOO.TS"), collision_key("foo.ts"));
        assert_ne!(collision_key("Foo.ts"), collision_key("Bar.ts"));
    }

    #[test]
    fn collision_key_unicode_normalization() {
        // U+00E9 (composed) vs U+0065 U+0301 (decomposed)
        let composed = "café";
        let decomposed = "cafe\u{0301}";
        assert_ne!(composed, decomposed);
        assert_eq!(collision_key(composed), collision_key(decomposed));
    }

    #[test]
    fn collision_key_case_and_normalization_combined() {
        // CAFE composed vs cafe + acute decomposed, upper vs lower
        let a = "CAFÉ";
        let b = "cafe\u{0301}";
        assert_eq!(collision_key(a), collision_key(b));
    }

    #[test]
    fn portable_policy_detects_macos_linux_hazards() {
        // The classic macOS/Linux hazard: two files differing only by case.
        assert!(CasePolicy::Portable.names_collide("Foo.ts", "foo.ts"));
        assert!(!CasePolicy::CaseSensitiveOnly.names_collide("Foo.ts", "foo.ts"));
    }

    #[test]
    fn portable_policy_detects_normalization_hazard() {
        assert!(CasePolicy::Portable.names_collide("café", "cafe\u{0301}"));
    }

    #[test]
    fn case_sensitive_only_only_collides_on_exact_match() {
        assert!(CasePolicy::CaseSensitiveOnly.names_collide("foo", "foo"));
        assert!(!CasePolicy::CaseSensitiveOnly.names_collide("foo", "Foo"));
    }

    #[test]
    fn case_policy_serialization() {
        let p = CasePolicy::Portable;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"portable\"");
        let back: CasePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);

        let p = CasePolicy::CaseSensitiveOnly;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"case-sensitive-only\"");
    }
}
