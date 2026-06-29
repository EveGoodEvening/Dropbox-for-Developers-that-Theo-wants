//! Dotenv parser and materializer.
//!
//! Parses common dotenv syntax: KEY=value, KEY="value", KEY='value',
//! comments (#), blank lines, and export prefix.

use std::collections::HashMap;
use std::path::Path;

/// A parsed dotenv entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotenvEntry {
    /// Variable name.
    pub key: String,
    /// Variable value (unquoted).
    pub value: String,
}

/// Error returned by [`parse_dotenv`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DotenvError {
    /// Unterminated quoted value.
    #[error("line {line}: unterminated quoted value")]
    UnterminatedQuote {
        /// 1-based line number.
        line: usize,
    },
    /// Invalid line syntax.
    #[error("line {line}: invalid syntax (expected KEY=value)")]
    InvalidSyntax {
        /// 1-based line number.
        line: usize,
    },
}

/// Parse dotenv content into a list of entries.
///
/// # Errors
/// Returns [`DotenvError`] if a line has invalid syntax.
pub fn parse_dotenv(content: &str) -> Result<Vec<DotenvEntry>, DotenvError> {
    let mut entries = Vec::new();
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        // Skip blank lines and comments.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Remove optional `export ` prefix.
        let line = line.strip_prefix("export ").unwrap_or(line);
        // Split on first `=`.
        let (key, value_part) = line
            .split_once('=')
            .ok_or(DotenvError::InvalidSyntax { line: line_no })?;
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err(DotenvError::InvalidSyntax { line: line_no });
        }
        // Parse the value: handle quotes.
        let value = parse_value(value_part.trim(), line_no)?;
        entries.push(DotenvEntry { key, value });
    }
    Ok(entries)
}

fn parse_value(s: &str, line: usize) -> Result<String, DotenvError> {
    if let Some(rest) = s.strip_prefix('"') {
        // Double-quoted: find closing quote (no escape handling for MVP).
        let end = rest
            .find('"')
            .ok_or(DotenvError::UnterminatedQuote { line })?;
        Ok(rest[..end].to_owned())
    } else if let Some(rest) = s.strip_prefix('\'') {
        // Single-quoted: find closing quote.
        let end = rest
            .find('\'')
            .ok_or(DotenvError::UnterminatedQuote { line })?;
        Ok(rest[..end].to_owned())
    } else {
        // Unquoted: take the rest of the line (trim trailing whitespace).
        Ok(s.trim_end().to_owned())
    }
}

/// Parse dotenv content into a `HashMap`.
///
/// # Errors
/// Returns [`DotenvError`] if parsing fails.
pub fn parse_dotenv_map(content: &str) -> Result<HashMap<String, String>, DotenvError> {
    parse_dotenv(content).map(|entries| entries.into_iter().map(|e| (e.key, e.value)).collect())
}

/// Materialize env vars into a dotenv file at the given path with mode 0600.
///
/// # Errors
/// Returns an error if the file cannot be written or permissions cannot be set.
pub fn materialize_dotenv<S: std::hash::BuildHasher>(
    path: &Path,
    vars: &HashMap<String, String, S>,
) -> anyhow::Result<()> {
    use std::fmt::Write;
    let mut content = String::new();
    for (key, value) in vars {
        // Quote values that contain spaces or special characters.
        if value.contains(' ') || value.contains('"') || value.contains('\'') {
            let _ = writeln!(content, "{key}=\"{value}\"");
        } else {
            let _ = writeln!(content, "{key}={value}");
        }
    }
    std::fs::write(path, content)?;
    // Set file permissions to 0600 on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let content = "API_URL=https://api.example.com\nPORT=3000\n";
        let entries = parse_dotenv(content).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "API_URL");
        assert_eq!(entries[0].value, "https://api.example.com");
        assert_eq!(entries[1].key, "PORT");
        assert_eq!(entries[1].value, "3000");
    }

    #[test]
    fn parse_with_comments_and_blanks() {
        let content = "# Comment\n\nAPI_KEY=secret\n\n# Another comment\n";
        let entries = parse_dotenv(content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "API_KEY");
    }

    #[test]
    fn parse_with_export_prefix() {
        let content = "export DATABASE_URL=postgres://localhost\n";
        let entries = parse_dotenv(content).unwrap();
        assert_eq!(entries[0].key, "DATABASE_URL");
        assert_eq!(entries[0].value, "postgres://localhost");
    }

    #[test]
    fn parse_double_quoted() {
        let content = "GREETING=\"Hello World\"\n";
        let entries = parse_dotenv(content).unwrap();
        assert_eq!(entries[0].value, "Hello World");
    }

    #[test]
    fn parse_single_quoted() {
        let content = "REGEX='foo.*bar'\n";
        let entries = parse_dotenv(content).unwrap();
        assert_eq!(entries[0].value, "foo.*bar");
    }

    #[test]
    fn parse_unterminated_quote() {
        let content = "KEY=\"unterminated\n";
        assert!(parse_dotenv(content).is_err());
    }

    #[test]
    fn parse_no_equals() {
        let content = "INVALID\n";
        assert!(parse_dotenv(content).is_err());
    }

    #[test]
    fn parse_map() {
        let content = "A=1\nB=2\n";
        let map = parse_dotenv_map(content).unwrap();
        assert_eq!(map.get("A"), Some(&"1".to_owned()));
        assert_eq!(map.get("B"), Some(&"2".to_owned()));
    }

    #[test]
    fn materialize_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        let mut vars = HashMap::new();
        vars.insert("API_KEY".to_owned(), "secret123".to_owned());
        vars.insert("GREETING".to_owned(), "Hello World".to_owned());
        materialize_dotenv(&path, &vars).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("API_KEY=secret123"));
        assert!(content.contains("GREETING=\"Hello World\""));
        // Verify permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(perms & 0o777, 0o600);
        }
        // Parse back.
        let parsed = parse_dotenv_map(&content).unwrap();
        assert_eq!(parsed.get("API_KEY"), Some(&"secret123".to_owned()));
        assert_eq!(parsed.get("GREETING"), Some(&"Hello World".to_owned()));
    }
}
