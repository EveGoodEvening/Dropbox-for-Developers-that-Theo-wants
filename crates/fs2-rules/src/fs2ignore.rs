//! `.fs2ignore` parser.
//!
//! Syntax: gitignore-compatible globs with optional `:action` prefixes.
//! Lines without a prefix default to `ignore`. Comments and blank lines are
//! skipped. Last-match-wins semantics are applied by the precedence engine.

use std::str::FromStr;

use crate::action::Action;
use crate::glob::Glob;

/// A single parsed `.fs2ignore` entry.
#[derive(Debug, Clone)]
pub struct Fs2IgnoreEntry {
    /// 1-based line number in the source file.
    pub line: usize,
    /// The glob pattern as written.
    pub pattern: String,
    /// The action assigned to this pattern.
    pub action: Action,
    /// The compiled glob.
    pub glob: Glob,
}

/// Error returned by [`Fs2IgnoreParser::parse`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Fs2IgnoreError {
    /// Invalid action prefix.
    #[error("line {line}: unknown action prefix `{prefix}`")]
    UnknownAction {
        /// 1-based line number.
        line: usize,
        /// The unrecognized prefix string.
        prefix: String,
    },
    /// Invalid glob pattern.
    #[error("line {line}: invalid glob pattern: {message}")]
    InvalidGlob {
        /// 1-based line number.
        line: usize,
        /// Error message.
        message: String,
    },
}

/// Parser for `.fs2ignore` content.
#[derive(Debug, Default)]
pub struct Fs2IgnoreParser;

impl Fs2IgnoreParser {
    /// Parse `.fs2ignore` content into an ordered list of entries.
    ///
    /// # Errors
    /// Returns [`Fs2IgnoreError`] if a line has an unknown action prefix or an
    /// invalid glob pattern.
    pub fn parse(&self, content: &str) -> Result<Vec<Fs2IgnoreEntry>, Fs2IgnoreError> {
        let mut entries = Vec::new();
        for (idx, raw_line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw_line.trim_end();
            // Skip blank lines and comments.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Detect action prefix: `:action pattern`.
            let (action, pattern_str) = if let Some(rest) = line.strip_prefix(':') {
                // Split on first whitespace.
                let (prefix, pat) = match rest.split_once(char::is_whitespace) {
                    Some((p, rest_pat)) => (p, rest_pat.trim_start()),
                    None => (rest, ""),
                };
                let act = Action::from_str(prefix).map_err(|_e| Fs2IgnoreError::UnknownAction {
                    line: line_no,
                    prefix: prefix.to_owned(),
                })?;
                (act, pat)
            } else {
                (Action::Ignore, line)
            };
            if pattern_str.is_empty() {
                // A line like `:generated` with no pattern is invalid but we
                // skip it silently to be forgiving (gitignore skips empty).
                continue;
            }
            let glob = Glob::compile(pattern_str).map_err(|e| Fs2IgnoreError::InvalidGlob {
                line: line_no,
                message: e.to_string(),
            })?;
            entries.push(Fs2IgnoreEntry {
                line: line_no,
                pattern: pattern_str.to_owned(),
                action,
                glob,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Vec<Fs2IgnoreEntry> {
        Fs2IgnoreParser
            .parse(content)
            .expect("parse should succeed")
    }

    #[test]
    fn comments_and_blanks_skipped() {
        let entries = parse("# comment\n\n   \n*.ts\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pattern, "*.ts");
        assert_eq!(entries[0].action, Action::Ignore);
        assert_eq!(entries[0].line, 4);
    }

    #[test]
    fn action_prefixes() {
        let content = "\
:generated node_modules/
:local-only .env.local
:lazy fixtures/**
:pin README.md
:normal src/
:secret .env
:dependency-cache .pnpm-store/
*.swp
";
        let entries = parse(content);
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0].action, Action::Generated);
        assert_eq!(entries[0].pattern, "node_modules/");
        assert_eq!(entries[1].action, Action::LocalOnly);
        assert_eq!(entries[2].action, Action::Lazy);
        assert_eq!(entries[3].action, Action::Pin);
        assert_eq!(entries[4].action, Action::Normal);
        assert_eq!(entries[5].action, Action::Secret);
        assert_eq!(entries[6].action, Action::DependencyCache);
        assert_eq!(entries[7].action, Action::Ignore);
        assert_eq!(entries[7].pattern, "*.swp");
    }

    #[test]
    fn unknown_action_errors() {
        let err = Fs2IgnoreParser.parse(":bogus pattern").unwrap_err();
        assert!(matches!(
            err,
            Fs2IgnoreError::UnknownAction { line: 1, prefix } if prefix == "bogus"
        ));
    }

    #[test]
    fn invalid_glob_errors() {
        let err = Fs2IgnoreParser.parse("[abc").unwrap_err();
        assert!(matches!(err, Fs2IgnoreError::InvalidGlob { line: 1, .. }));
    }

    #[test]
    fn deterministic_order() {
        let content = "*.ts\n:generated target/\n*.rs\n";
        let entries = parse(content);
        assert_eq!(entries[0].pattern, "*.ts");
        assert_eq!(entries[1].pattern, "target/");
        assert_eq!(entries[2].pattern, "*.rs");
    }
}
