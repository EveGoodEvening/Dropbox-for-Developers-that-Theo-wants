//! Workspace path validation and portable sibling collision keys.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// Case policy used when deriving sibling comparison keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CasePolicy {
    /// Portable mode blocks names that differ only by Unicode normalization or case folding.
    Portable,
    /// Exact-string mode for workspaces restricted to case-sensitive devices.
    CaseSensitiveOnly,
}

/// Error returned when parsing workspace paths or node names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePathError {
    message: String,
}

impl ParsePathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParsePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParsePathError {}

/// Canonical UTF-8 workspace-relative path using `/` separators.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// Parses and canonicalizes a workspace-relative path.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, ParsePathError> {
        let raw = value.as_ref();
        if raw.contains('\0') {
            return Err(ParsePathError::new("workspace path must not contain NUL"));
        }
        let normalized = raw.replace('\\', "/");
        if normalized.is_empty() {
            return Ok(Self(String::new()));
        }
        if normalized.starts_with('/') {
            return Err(ParsePathError::new("workspace path must be relative"));
        }
        let mut segments = Vec::new();
        for segment in normalized.split('/') {
            validate_segment(segment, "workspace path segment")?;
            segments.push(segment);
        }
        Ok(Self(segments.join("/")))
    }

    /// Returns true when this path is the workspace root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the canonical path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterates over canonical path segments. Root yields no segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|segment| !segment.is_empty())
    }

    /// Consumes this path and returns the canonical string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for WorkspacePath {
    type Err = ParsePathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Exact display name for one workspace tree node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct NodeName(String);

impl NodeName {
    /// Parses a single display-name component without rewriting it.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, ParsePathError> {
        let value = value.as_ref();
        validate_segment(value, "node name")?;
        if value.contains('\\') {
            return Err(ParsePathError::new(
                "node name must not contain path separators",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the exact display name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes this name and returns the exact display string.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Computes this name's sibling comparison key for the requested case policy.
    pub fn normalized(&self, policy: CasePolicy) -> NormalizedName {
        normalized_name(self.as_str(), policy)
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for NodeName {
    type Err = ParsePathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for NodeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Sibling comparison key derived from an exact display name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NormalizedName(String);

impl NormalizedName {
    /// Returns the comparison key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes this key and returns the comparison string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for NormalizedName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Computes a sibling comparison key after validating the display name.
pub fn try_normalized_name(
    display_name: impl AsRef<str>,
    policy: CasePolicy,
) -> Result<NormalizedName, ParsePathError> {
    let name = NodeName::parse(display_name)?;
    Ok(name.normalized(policy))
}

/// Returns whether two candidate names collide under the same parent for `policy`.
pub fn names_collide(left: &NodeName, right: &NodeName, policy: CasePolicy) -> bool {
    left.normalized(policy) == right.normalized(policy)
}

fn normalized_name(display_name: &str, policy: CasePolicy) -> NormalizedName {
    match policy {
        CasePolicy::Portable => {
            let nfc = display_name.nfc().collect::<String>();
            let folded = nfc.chars().case_fold().collect::<String>();
            NormalizedName(folded.nfc().collect())
        }
        CasePolicy::CaseSensitiveOnly => NormalizedName(display_name.to_owned()),
    }
}

fn validate_segment(segment: &str, label: &'static str) -> Result<(), ParsePathError> {
    if segment.is_empty() {
        return Err(ParsePathError::new(format!("{label} must not be empty")));
    }
    if segment == "." || segment == ".." {
        return Err(ParsePathError::new(format!("{label} must not be . or ..")));
    }
    if segment.contains('/') || segment.contains('\\') {
        return Err(ParsePathError::new(format!(
            "{label} must not contain path separators"
        )));
    }
    if segment.contains('\0') {
        return Err(ParsePathError::new(format!("{label} must not contain NUL")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_path_accepts_and_canonicalizes_safe_paths(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = WorkspacePath::parse("")?;
        assert!(root.is_root());
        assert_eq!(root.as_str(), "");
        assert_eq!(WorkspacePath::parse("src/lib.rs")?.as_str(), "src/lib.rs");
        assert_eq!(WorkspacePath::parse("src\\lib.rs")?.as_str(), "src/lib.rs");
        assert_eq!(WorkspacePath::parse(".gitignore")?.as_str(), ".gitignore");
        let unicode = WorkspacePath::parse("cafe\u{0301}.txt")?;
        assert_eq!(unicode.as_str(), "cafe\u{0301}.txt");
        Ok(())
    }

    #[test]
    fn workspace_path_rejects_malicious_paths() {
        let invalid = [
            "/etc/passwd",
            "/",
            "\\foo",
            "\\\\server\\share",
            "..",
            "../x",
            "a/../b",
            ".",
            "./x",
            "a/./b",
            "a//b",
            "a/",
            "a\\\\b",
            "a/\\b",
            "nul\0name",
        ];
        for path in invalid {
            assert!(WorkspacePath::parse(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn node_name_preserves_display_and_rejects_path_syntax(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for name in ["foo.ts", ".env", "café.txt"] {
            assert_eq!(NodeName::parse(name)?.as_str(), name);
        }
        for name in ["", ".", "..", "a/b", "a\\b", "nul\0name"] {
            assert!(NodeName::parse(name).is_err(), "accepted {name:?}");
        }
        Ok(())
    }

    #[test]
    fn portable_collision_key_blocks_case_and_unicode_hazards(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let upper = NodeName::parse("Foo.ts")?;
        let lower = NodeName::parse("foo.ts")?;
        assert!(names_collide(&upper, &lower, CasePolicy::Portable));

        let composed = NodeName::parse("caf\u{00E9}.txt")?;
        let decomposed = NodeName::parse("cafe\u{0301}.txt")?;
        assert!(names_collide(&composed, &decomposed, CasePolicy::Portable));
        assert_eq!(decomposed.as_str(), "cafe\u{0301}.txt");

        let kelvin = NodeName::parse("\u{212A}ey.txt")?;
        let ascii = NodeName::parse("key.txt")?;
        assert!(names_collide(&kelvin, &ascii, CasePolicy::Portable));
        Ok(())
    }

    #[test]
    fn case_sensitive_policy_preserves_distinctions() -> Result<(), Box<dyn std::error::Error>> {
        let upper = NodeName::parse("Foo.ts")?;
        let lower = NodeName::parse("foo.ts")?;
        assert!(!names_collide(
            &upper,
            &lower,
            CasePolicy::CaseSensitiveOnly
        ));

        let composed = NodeName::parse("caf\u{00E9}.txt")?;
        let decomposed = NodeName::parse("cafe\u{0301}.txt")?;
        assert!(!names_collide(
            &composed,
            &decomposed,
            CasePolicy::CaseSensitiveOnly
        ));
        Ok(())
    }

    #[test]
    fn collision_checks_are_sibling_scoped() -> Result<(), Box<dyn std::error::Error>> {
        let left_parent = WorkspacePath::parse("dir_a")?;
        let right_parent = WorkspacePath::parse("dir_b")?;
        let left_name = NodeName::parse("Foo.ts")?;
        let right_name = NodeName::parse("foo.ts")?;
        assert_ne!(left_parent, right_parent);
        assert!(names_collide(&left_name, &right_name, CasePolicy::Portable));
        Ok(())
    }

    #[test]
    fn path_newtypes_have_stable_json() -> Result<(), Box<dyn std::error::Error>> {
        let path = WorkspacePath::parse("src\\lib.rs")?;
        let name = NodeName::parse("Foo.ts")?;
        assert_eq!(serde_json::to_value(&path)?, json!("src/lib.rs"));
        assert_eq!(serde_json::to_value(&name)?, json!("Foo.ts"));
        assert_eq!(
            serde_json::to_value(name.normalized(CasePolicy::Portable))?,
            json!("foo.ts")
        );
        assert!(serde_json::from_value::<WorkspacePath>(json!("../x")).is_err());
        assert!(serde_json::from_value::<NodeName>(json!("a/b")).is_err());
        Ok(())
    }
}
