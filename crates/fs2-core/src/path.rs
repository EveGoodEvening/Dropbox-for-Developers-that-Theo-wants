//! Workspace-relative path utilities and collision policy.
//!
//! Paths are derived from `parent_id + name`, not primary keys. This module
//! provides a validated workspace-relative path type that cannot escape the
//! workspace root, plus Unicode normalization and case-folding helpers for
//! portable collision detection.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A workspace-relative path that has been validated to be safe.
///
/// Invariants:
/// - Never absolute (no leading `/`).
/// - Never contains `..` traversal.
/// - Never contains null bytes.
/// - No empty path segments except the root (empty string).
/// - Separators normalized to `/`.
/// - Original display string preserved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelPath {
    /// The validated path string, using `/` separators, no leading slash.
    inner: String,
}

impl RelPath {
    /// The workspace root path, represented as the empty string.
    #[must_use]
    pub fn root() -> Self {
        Self {
            inner: String::new(),
        }
    }

    /// Whether this is the workspace root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.inner.is_empty()
    }

    /// Parse and validate a workspace-relative path string.
    ///
    /// # Errors
    /// Returns [`PathError`] if the path is absolute, contains `..`, contains
    /// null bytes, or has empty segments other than the root.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Ok(Self::root());
        }
        if s.contains('\0') {
            return Err(PathError::NullByte);
        }
        if s.starts_with('/') {
            return Err(PathError::Absolute);
        }
        // Normalize backslashes to forward slashes? No: on POSIX the backslash
        // is a legal filename character. We only normalize `/` as separator.
        let mut segments: Vec<&str> = Vec::new();
        for seg in s.split('/') {
            if seg == ".." {
                return Err(PathError::Traversal);
            }
            if seg == "." {
                // Skip "." segments silently — they are no-ops.
                continue;
            }
            if seg.is_empty() {
                // Empty segments (e.g. "a//b" or trailing "a/") are rejected.
                return Err(PathError::EmptySegment);
            }
            segments.push(seg);
        }
        if segments.is_empty() {
            return Ok(Self::root());
        }
        Ok(Self {
            inner: segments.join("/"),
        })
    }

    /// Construct a child path by appending a single validated name segment.
    ///
    /// # Errors
    /// Returns [`PathError`] if the name is empty, contains `/`, null bytes,
    /// or is `..` / `.`.
    pub fn child(&self, name: &str) -> Result<Self, PathError> {
        if name.is_empty() {
            return Err(PathError::EmptySegment);
        }
        if name.contains('\0') {
            return Err(PathError::NullByte);
        }
        if name.contains('/') {
            return Err(PathError::NameContainsSeparator);
        }
        if name == ".." {
            return Err(PathError::Traversal);
        }
        if name == "." {
            return Ok(self.clone());
        }
        if self.is_root() {
            Ok(Self {
                inner: name.to_owned(),
            })
        } else {
            Ok(Self {
                inner: format!("{}/{}", self.inner, name),
            })
        }
    }

    /// Return the path as a string with `/` separators, no leading slash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// Iterate over the path segments.
    pub fn segments(&self) -> Vec<&str> {
        if self.inner.is_empty() {
            Vec::new()
        } else {
            self.inner.split('/').collect()
        }
    }

    /// The last segment (file/dir name), or `None` for root.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.inner
            .rsplit_once('/')
            .map(|(_, name)| name)
            .or_else(|| (!self.inner.is_empty()).then_some(self.inner.as_str()))
    }

    /// The parent path, or `None` if this is root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        self.inner
            .rsplit_once('/')
            .map(|(parent, _)| Self {
                inner: parent.to_owned(),
            })
            .or_else(|| {
                if self.inner.is_empty() {
                    None
                } else {
                    Some(Self::root())
                }
            })
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

/// Error returned by path validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// Path is absolute (starts with `/`).
    #[error("absolute paths are not allowed")]
    Absolute,
    /// Path contains `..` traversal.
    #[error("path traversal (..) is not allowed")]
    Traversal,
    /// Path contains a null byte.
    #[error("null bytes are not allowed in paths")]
    NullByte,
    /// Path has an empty segment (e.g. `a//b` or trailing slash).
    #[error("empty path segment")]
    EmptySegment,
    /// A name component contains a `/` separator.
    #[error("name contains a path separator")]
    NameContainsSeparator,
}

/// Case policy for a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CasePolicy {
    /// Portable: block collisions under Unicode normalization + case-folding.
    /// Required for macOS compatibility.
    #[default]
    Portable,
    /// Case-sensitive only: allow `Foo` and `foo` as distinct siblings.
    CaseSensitiveOnly,
}

/// Compute a portable collision key for a name.
///
/// This applies Unicode NFC normalization followed by full case-folding, so
/// that names that would collide on a case-insensitive macOS filesystem are
/// detected as collisions.
///
/// # Examples
/// - `Foo.ts` and `foo.ts` collide under portable policy.
/// - Unicode composed and decomposed forms of the same character collide.
#[must_use]
pub fn portable_collision_key(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    // NFC normalization: compose decomposed forms.
    let nfc: String = name.nfc().collect();
    // Full case-folding via Unicode lowercase mapping (covers the common
    // macOS case-insensitive collision cases). This is not a complete Unicode
    // case-fold (which would also handle e.g. ß -> ss), but it catches the
    // practically relevant Foo/foo and composed/decomposed collisions.
    nfc.to_lowercase()
}

/// Compute the normalized comparison key for a name given a case policy.
#[must_use]
pub fn normalized_name(name: &str, policy: CasePolicy) -> String {
    match policy {
        CasePolicy::Portable => portable_collision_key(name),
        CasePolicy::CaseSensitiveOnly => {
            // Still NFC-normalize to catch composed/decomposed collisions,
            // but preserve case.
            use unicode_normalization::UnicodeNormalization;
            name.nfc().collect::<String>()
        }
    }
}

/// Check whether two sibling names collide under the given policy.
#[must_use]
pub fn names_collide(a: &str, b: &str, policy: CasePolicy) -> bool {
    normalized_name(a, policy) == normalized_name(b, policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_paths() {
        assert_eq!(RelPath::parse("").unwrap().as_str(), "");
        assert_eq!(RelPath::parse("apps").unwrap().as_str(), "apps");
        assert_eq!(RelPath::parse("apps/web").unwrap().as_str(), "apps/web");
        assert_eq!(RelPath::parse("apps/./web").unwrap().as_str(), "apps/web");
    }

    #[test]
    fn parse_rejects_absolute() {
        assert_eq!(RelPath::parse("/etc").unwrap_err(), PathError::Absolute);
    }

    #[test]
    fn parse_rejects_traversal() {
        assert_eq!(RelPath::parse("../x").unwrap_err(), PathError::Traversal);
        assert_eq!(RelPath::parse("a/../b").unwrap_err(), PathError::Traversal);
    }

    #[test]
    fn parse_rejects_null_byte() {
        assert_eq!(RelPath::parse("a\0b").unwrap_err(), PathError::NullByte);
    }

    #[test]
    fn parse_rejects_empty_segment() {
        assert_eq!(RelPath::parse("a//b").unwrap_err(), PathError::EmptySegment);
        assert_eq!(RelPath::parse("a/").unwrap_err(), PathError::EmptySegment);
    }

    #[test]
    fn child_appends_name() {
        let root = RelPath::root();
        let apps = root.child("apps").unwrap();
        assert_eq!(apps.as_str(), "apps");
        let web = apps.child("web").unwrap();
        assert_eq!(web.as_str(), "apps/web");
    }

    #[test]
    fn child_rejects_separator_in_name() {
        let root = RelPath::root();
        assert_eq!(
            root.child("a/b").unwrap_err(),
            PathError::NameContainsSeparator
        );
    }

    #[test]
    fn name_and_parent() {
        let p = RelPath::parse("apps/web/package.json").unwrap();
        assert_eq!(p.name(), Some("package.json"));
        assert_eq!(p.parent().unwrap().as_str(), "apps/web");
        assert_eq!(p.parent().unwrap().parent().unwrap().as_str(), "apps");
        assert!(p
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .is_root());
        assert!(RelPath::root().parent().is_none());
        assert!(RelPath::root().name().is_none());
    }

    #[test]
    fn portable_case_collision() {
        assert!(names_collide("Foo.ts", "foo.ts", CasePolicy::Portable));
        assert!(!names_collide(
            "Foo.ts",
            "foo.ts",
            CasePolicy::CaseSensitiveOnly
        ));
    }

    #[test]
    fn portable_unicode_collision() {
        // Composed (é = U+00E9) vs decomposed (e + U+0301) should collide.
        let composed = "caf\u{00E9}";
        let decomposed = "cafe\u{0301}";
        assert!(names_collide(composed, decomposed, CasePolicy::Portable));
        assert!(names_collide(
            composed,
            decomposed,
            CasePolicy::CaseSensitiveOnly
        ));
    }

    #[test]
    fn names_valid_on_linux_unsafe_on_macos() {
        // Linux allows both Foo and foo; macOS APFS default may not.
        // Portable policy flags this as a collision.
        assert!(names_collide("Foo", "foo", CasePolicy::Portable));
    }
}
