//! `.fs2ignore` parser.
//!
//! Syntax is gitignore-compatible globs with optional `:action` prefixes.
//! Lines without a prefix default to `ignore`. Comments (`#`) and blank lines
//! are skipped. Last match wins, matching gitignore semantics.
//!
//! Example:
//!
//! ```text
//! # Default action is ignore
//! .DS_Store
//! *.swp
//!
//! :generated node_modules/
//! :local-only .env.local
//! :pin package.json
//! :secret .env
//! ```

use std::fmt;

use crate::action::Action;

/// A single parsed `.fs2ignore` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fs2IgnoreEntry {
    /// 1-based line number in the source file, for error reporting.
    pub line: usize,
    /// The action prefix, or `Action::Ignore` if no prefix was given.
    pub action: Action,
    /// The glob pattern as written (after stripping the prefix and whitespace).
    pub pattern: String,
}

/// A fully parsed `.fs2ignore` file: an ordered list of entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fs2IgnoreFile {
    /// Entries in file order. Last match wins.
    pub entries: Vec<Fs2IgnoreEntry>,
}

/// Error returned when a `.fs2ignore` line cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fs2IgnoreParseError {
    /// 1-based line number where the error occurred.
    pub line: usize,
    /// Human-readable explanation.
    pub message: String,
}

impl fmt::Display for Fs2IgnoreParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ".fs2ignore:{}: {}", self.line, self.message)
    }
}

impl std::error::Error for Fs2IgnoreParseError {}

impl Fs2IgnoreFile {
    /// Parse a `.fs2ignore` file from its text content.
    ///
    /// # Errors
    /// Returns [`Fs2IgnoreParseError`] if a line has an unrecognized action
    /// prefix or an empty pattern after a prefix.
    pub fn parse(text: &str) -> Result<Self, Fs2IgnoreParseError> {
        let mut entries = Vec::new();
        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = raw_line.trim();

            // Skip blank lines and comments.
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let (action, pattern) = if let Some(rest) = trimmed.strip_prefix(':') {
                // `:action pattern` — split on first whitespace.
                let (tag, pat) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let action = Action::parse_tag(tag.trim()).ok_or_else(|| Fs2IgnoreParseError {
                    line: line_no,
                    message: format!("unknown action prefix: {:?}", tag.trim()),
                })?;
                let pat = pat.trim();
                if pat.is_empty() {
                    return Err(Fs2IgnoreParseError {
                        line: line_no,
                        message: "action prefix with empty pattern".to_owned(),
                    });
                }
                (action, pat.to_owned())
            } else {
                (Action::Ignore, trimmed.to_owned())
            };

            entries.push(Fs2IgnoreEntry {
                line: line_no,
                action,
                pattern,
            });
        }
        Ok(Self { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_produces_no_entries() {
        let f = Fs2IgnoreFile::parse("").unwrap();
        assert!(f.entries.is_empty());
    }

    #[test]
    fn comments_and_blanks_skipped() {
        let text = "# a comment\n\n   \n# another\n";
        let f = Fs2IgnoreFile::parse(text).unwrap();
        assert!(f.entries.is_empty());
    }

    #[test]
    fn bare_patterns_default_to_ignore() {
        let f = Fs2IgnoreFile::parse(".DS_Store\n*.swp").unwrap();
        assert_eq!(f.entries.len(), 2);
        assert_eq!(f.entries[0].action, Action::Ignore);
        assert_eq!(f.entries[0].pattern, ".DS_Store");
        assert_eq!(f.entries[0].line, 1);
        assert_eq!(f.entries[1].action, Action::Ignore);
        assert_eq!(f.entries[1].pattern, "*.swp");
    }

    #[test]
    fn action_prefix_parsed() {
        let text =
            ":generated node_modules/\n:local-only .env.local\n:pin package.json\n:secret .env\n";
        let f = Fs2IgnoreFile::parse(text).unwrap();
        assert_eq!(f.entries.len(), 4);
        assert_eq!(f.entries[0].action, Action::Generated);
        assert_eq!(f.entries[0].pattern, "node_modules/");
        assert_eq!(f.entries[1].action, Action::LocalOnly);
        assert_eq!(f.entries[2].action, Action::Pin);
        assert_eq!(f.entries[3].action, Action::Secret);
    }

    #[test]
    fn all_action_prefixes_recognized() {
        let text = "\
:ignore a
:local-only b
:generated c
:lazy d
:pin e
:normal f
:secret g
:dependency-cache h
";
        let f = Fs2IgnoreFile::parse(text).unwrap();
        let actions: Vec<_> = f.entries.iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            [
                Action::Ignore,
                Action::LocalOnly,
                Action::Generated,
                Action::Lazy,
                Action::Pin,
                Action::Normal,
                Action::Secret,
                Action::DependencyCache,
            ]
        );
    }

    #[test]
    fn unknown_action_prefix_errors_with_line_number() {
        let err = Fs2IgnoreFile::parse(":bogus pattern").unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains("bogus"));
    }

    #[test]
    fn empty_pattern_after_prefix_errors() {
        let err = Fs2IgnoreFile::parse(":generated").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn last_match_wins_order_preserved() {
        // The engine evaluates last-match-wins; here we just check the file
        // preserves order so the engine can do that.
        let text = ":normal foo\n:ignore foo\n";
        let f = Fs2IgnoreFile::parse(text).unwrap();
        assert_eq!(f.entries.len(), 2);
        assert_eq!(f.entries[0].action, Action::Normal);
        assert_eq!(f.entries[1].action, Action::Ignore);
    }

    #[test]
    fn whitespace_around_pattern_trimmed() {
        let f = Fs2IgnoreFile::parse("  :pin   package.json  ").unwrap();
        assert_eq!(f.entries[0].pattern, "package.json");
        assert_eq!(f.entries[0].action, Action::Pin);
    }
}
