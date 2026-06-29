//! Gitignore-style glob matching.
//!
//! This implements the subset of gitignore glob semantics needed for `.fs2ignore`
//! and config rules:
//! - `*` matches any characters except `/`.
//! - `**` matches any number of path segments (including zero).
//! - `?` matches a single character except `/`.
//! - `[...]` character classes are supported (including ranges and negation).
//! - A leading `/` anchors the pattern to the workspace root.
//! - A trailing `/` matches directories only (we treat it as matching the
//!   path prefix for simplicity in the MVP).
//! - No leading `/` means the pattern may match at any depth (like gitignore).
//!
//! Patterns are matched against workspace-relative paths using `/` separators.

/// A compiled glob pattern.
#[derive(Debug, Clone)]
pub struct Glob {
    raw: String,
    anchored: bool,
    directory_only: bool,
    segments: Vec<GlobSegment>,
}

#[derive(Debug, Clone)]
enum GlobSegment {
    /// Literal string segment.
    Literal(String),
    /// A segment containing wildcards (but no `**`).
    Wildcard(WildcardSeg),
    /// A `**` double-star segment matching zero or more path segments.
    DoubleStar,
}

#[derive(Debug, Clone)]
struct WildcardSeg {
    tokens: Vec<WildToken>,
}

#[derive(Debug, Clone)]
enum WildToken {
    Lit(String),
    Star, // * within a segment
    Question,
    Class {
        negated: bool,
        chars: Vec<(char, char)>,
    },
}

impl Glob {
    /// Compile a glob pattern.
    ///
    /// # Errors
    /// Returns an error if the pattern contains invalid syntax (e.g. an
    /// unterminated character class).
    pub fn compile(pattern: &str) -> Result<Self, GlobError> {
        let raw = pattern.to_owned();
        let mut p = pattern;
        let anchored = if let Some(rest) = p.strip_prefix('/') {
            p = rest;
            true
        } else {
            false
        };
        let directory_only = p.ends_with('/');
        if directory_only {
            p = &p[..p.len() - 1];
        }
        let segments: Vec<GlobSegment> = if p.is_empty() {
            Vec::new()
        } else {
            p.split('/')
                .map(|seg| -> Result<GlobSegment, GlobError> {
                    if seg == "**" {
                        Ok(GlobSegment::DoubleStar)
                    } else if !seg.contains('*') && !seg.contains('?') && !seg.contains('[') {
                        Ok(GlobSegment::Literal(seg.to_owned()))
                    } else {
                        Ok(GlobSegment::Wildcard(compile_wildcard(seg)?))
                    }
                })
                .collect::<Result<_, _>>()?
        };
        Ok(Self {
            raw,
            anchored,
            directory_only,
            segments,
        })
    }

    /// The original pattern string.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Whether this pattern is anchored to the root.
    #[must_use]
    pub fn anchored(&self) -> bool {
        self.anchored
    }

    /// Test whether this glob matches the given workspace-relative path.
    ///
    /// `path` must use `/` separators and must not start with `/`.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let path_segs: Vec<&str> = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').collect()
        };
        if self.segments.is_empty() {
            return path_segs.is_empty();
        }
        if self.directory_only {
            // A trailing-slash pattern matches the directory itself and
            // everything under it (prefix match on segments).
            match_glob_prefix(&self.segments, &path_segs, self.anchored)
        } else {
            match_glob(&self.segments, &path_segs, self.anchored)
        }
    }
}

fn compile_wildcard(seg: &str) -> Result<WildcardSeg, GlobError> {
    let chars: Vec<char> = seg.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut lit = String::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if !lit.is_empty() {
                    tokens.push(WildToken::Lit(std::mem::take(&mut lit)));
                }
                tokens.push(WildToken::Star);
                i += 1;
            }
            '?' => {
                if !lit.is_empty() {
                    tokens.push(WildToken::Lit(std::mem::take(&mut lit)));
                }
                tokens.push(WildToken::Question);
                i += 1;
            }
            '[' => {
                if !lit.is_empty() {
                    tokens.push(WildToken::Lit(std::mem::take(&mut lit)));
                }
                let mut negated = false;
                i += 1;
                if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
                    negated = true;
                    i += 1;
                }
                let mut ranges = Vec::new();
                // A leading `]` is literal.
                if i < chars.len() && chars[i] == ']' {
                    ranges.push((']', ']'));
                    i += 1;
                }
                while i < chars.len() && chars[i] != ']' {
                    let start = chars[i];
                    i += 1;
                    if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] != ']' {
                        let end = chars[i + 1];
                        ranges.push((start, end));
                        i += 2;
                    } else {
                        ranges.push((start, start));
                    }
                }
                if i >= chars.len() {
                    return Err(GlobError::UnterminatedClass);
                }
                // skip ']'
                i += 1;
                tokens.push(WildToken::Class {
                    negated,
                    chars: ranges,
                });
            }
            _ => {
                lit.push(c);
                i += 1;
            }
        }
    }
    if !lit.is_empty() {
        tokens.push(WildToken::Lit(lit));
    }
    Ok(WildcardSeg { tokens })
}

/// Match a sequence of glob segments against path segments.
fn match_glob(segs: &[GlobSegment], path: &[&str], anchored: bool) -> bool {
    if anchored {
        match_seq(segs, path)
    } else {
        // Unanchored: try matching at every offset.
        for start in 0..=path.len() {
            if match_seq(segs, &path[start..]) {
                return true;
            }
        }
        false
    }
}

/// Prefix match for directory-only patterns: the pattern segments must match
/// a prefix of the path segments (the path may have additional children).
fn match_glob_prefix(segs: &[GlobSegment], path: &[&str], anchored: bool) -> bool {
    if anchored {
        match_seq_prefix(segs, path)
    } else {
        for start in 0..=path.len() {
            if match_seq_prefix(segs, &path[start..]) {
                return true;
            }
        }
        false
    }
}

fn match_seq_prefix(segs: &[GlobSegment], path: &[&str]) -> bool {
    if segs.is_empty() {
        return true;
    }
    let (first, rest) = segs.split_first().unwrap();
    match first {
        GlobSegment::DoubleStar => {
            for skip in 0..=path.len() {
                if match_seq_prefix(rest, &path[skip..]) {
                    return true;
                }
            }
            false
        }
        GlobSegment::Literal(lit) => {
            if let Some((p, rest_path)) = path.split_first() {
                lit == p && match_seq_prefix(rest, rest_path)
            } else {
                false
            }
        }
        GlobSegment::Wildcard(w) => {
            if let Some((p, rest_path)) = path.split_first() {
                match_wildcard_seg(w, p) && match_seq_prefix(rest, rest_path)
            } else {
                false
            }
        }
    }
}

fn match_seq(segs: &[GlobSegment], path: &[&str]) -> bool {
    if segs.is_empty() {
        return path.is_empty();
    }
    let (first, rest) = segs.split_first().unwrap();
    match first {
        GlobSegment::DoubleStar => {
            // ** matches zero or more segments.
            for skip in 0..=path.len() {
                if match_seq(rest, &path[skip..]) {
                    return true;
                }
            }
            false
        }
        GlobSegment::Literal(lit) => {
            if let Some((p, rest_path)) = path.split_first() {
                lit == p && match_seq(rest, rest_path)
            } else {
                false
            }
        }
        GlobSegment::Wildcard(w) => {
            if let Some((p, rest_path)) = path.split_first() {
                match_wildcard_seg(w, p) && match_seq(rest, rest_path)
            } else {
                false
            }
        }
    }
}

fn match_wildcard_seg(w: &WildcardSeg, s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    match_tokens(&w.tokens, &chars)
}

fn match_tokens(tokens: &[WildToken], chars: &[char]) -> bool {
    if tokens.is_empty() {
        return chars.is_empty();
    }
    let (first, rest) = tokens.split_first().unwrap();
    match first {
        WildToken::Lit(lit) => {
            let lit_chars: Vec<char> = lit.chars().collect();
            if chars.len() >= lit_chars.len() && chars[..lit_chars.len()] == lit_chars[..] {
                match_tokens(rest, &chars[lit_chars.len()..])
            } else {
                false
            }
        }
        WildToken::Star => {
            // * matches zero or more chars (not /, but we're within a segment).
            for skip in 0..=chars.len() {
                if match_tokens(rest, &chars[skip..]) {
                    return true;
                }
            }
            false
        }
        WildToken::Question => {
            if let Some((c, rest_chars)) = chars.split_first() {
                match_char(*c) && match_tokens(rest, rest_chars)
            } else {
                false
            }
        }
        WildToken::Class {
            negated,
            chars: ranges,
        } => {
            if let Some((c, rest_chars)) = chars.split_first() {
                let in_range = ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(c));
                let matched = if *negated { !in_range } else { in_range };
                matched && match_tokens(rest, rest_chars)
            } else {
                false
            }
        }
    }
}

fn match_char(c: char) -> bool {
    // ? matches any single char except '/', but we're within a segment so '/' is
    // already excluded.
    let _ = c;
    true
}

/// Error returned by [`Glob::compile`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlobError {
    /// Unterminated `[...]` character class.
    #[error("unterminated character class in glob")]
    UnterminatedClass,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_match() {
        let g = Glob::compile("package.json").unwrap();
        assert!(g.matches("package.json"));
        assert!(!g.matches("package-lock.json"));
        // Unanchored: matches at any depth.
        assert!(g.matches("apps/web/package.json"));
    }

    #[test]
    fn anchored_match() {
        let g = Glob::compile("/package.json").unwrap();
        assert!(g.matches("package.json"));
        assert!(!g.matches("apps/web/package.json"));
    }

    #[test]
    fn star_match() {
        let g = Glob::compile("*.ts").unwrap();
        assert!(g.matches("foo.ts"));
        assert!(!g.matches("foo.tsx"));
        assert!(g.matches("src/foo.ts"));
    }

    #[test]
    fn double_star_match() {
        let g = Glob::compile("node_modules/**").unwrap();
        assert!(g.matches("node_modules"));
        assert!(g.matches("node_modules/foo"));
        assert!(g.matches("node_modules/foo/bar"));
    }

    #[test]
    fn directory_only() {
        let g = Glob::compile("dist/").unwrap();
        assert!(g.matches("dist"));
        assert!(g.matches("dist/x"));
    }

    #[test]
    fn char_class() {
        let g = Glob::compile("[abc].ts").unwrap();
        assert!(g.matches("a.ts"));
        assert!(g.matches("b.ts"));
        assert!(!g.matches("d.ts"));
    }

    #[test]
    fn negated_class() {
        let g = Glob::compile("[!abc].ts").unwrap();
        assert!(!g.matches("a.ts"));
        assert!(g.matches("d.ts"));
    }

    #[test]
    fn question_mark() {
        let g = Glob::compile("?.ts").unwrap();
        assert!(g.matches("a.ts"));
        assert!(!g.matches("ab.ts"));
    }

    #[test]
    fn nested_pattern() {
        let g = Glob::compile("apps/*/build/**").unwrap();
        assert!(g.matches("apps/web/build/out.js"));
        assert!(g.matches("apps/web/build"));
        assert!(!g.matches("apps/web/src/x"));
    }

    #[test]
    fn unterminated_class_errors() {
        assert_eq!(
            Glob::compile("[abc.ts").unwrap_err(),
            GlobError::UnterminatedClass
        );
    }
}
