use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ToolIdPatternError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern ends with a dangling escape (`\\`)")]
    DanglingEscape,
}

/// Match a tool-id glob pattern against a literal tool id.
///
/// Grammar (anchored full match):
/// - `*` matches any sequence of characters (including `/`, `:`, `_`).
/// - `\` escapes the next character (`\*` is a literal `*`; `\\` a literal `\`).
/// - Every other character is a literal.
#[must_use]
pub fn tool_id_match(pattern: &str, tool_id: &str) -> bool {
    let p = pattern.as_bytes();
    let v = tool_id.as_bytes();
    let mut pi = 0usize;
    let mut vi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < v.len() {
        if pi < p.len() {
            let c = p[pi];
            if c == b'\\' && pi + 1 < p.len() {
                if p[pi + 1] == v[vi] {
                    pi += 2;
                    vi += 1;
                    continue;
                }
            } else if c == b'*' {
                star_pi = Some(pi);
                star_vi = vi;
                pi += 1;
                continue;
            } else if c == v[vi] {
                pi += 1;
                vi += 1;
                continue;
            }
        }
        if let Some(sp) = star_pi {
            pi = sp + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Validate that a tool-id pattern string is syntactically well-formed.
pub fn validate_tool_id_pattern(pattern: &str) -> Result<(), ToolIdPatternError> {
    if pattern.is_empty() {
        return Err(ToolIdPatternError::Empty);
    }
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 1 >= bytes.len() {
                return Err(ToolIdPatternError::DanglingEscape);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_matches_exact() {
        assert!(tool_id_match("Bash", "Bash"));
        assert!(!tool_id_match("Bash", "bash"));
        assert!(!tool_id_match("Bash", "Bashx"));
    }

    #[test]
    fn star_matches_anything() {
        assert!(tool_id_match("*", ""));
        assert!(tool_id_match("*", "Bash"));
        assert!(tool_id_match("*", "mcp:weather/forecast"));
    }

    #[test]
    fn star_prefix_and_suffix() {
        assert!(tool_id_match("mcp:*", "mcp:weather"));
        assert!(tool_id_match("mcp:*", "mcp:fs/read"));
        assert!(!tool_id_match("mcp:*", "Bash"));
        assert!(tool_id_match("*Tool", "BashTool"));
        assert!(tool_id_match("*Tool", "Tool"));
    }

    #[test]
    fn star_in_middle() {
        assert!(tool_id_match("mcp:*/read", "mcp:fs/read"));
        assert!(!tool_id_match("mcp:*/read", "mcp:fs/write"));
    }

    #[test]
    fn escape_literal_star() {
        assert!(tool_id_match(r"foo\*bar", "foo*bar"));
        assert!(!tool_id_match(r"foo\*bar", "foobar"));
        assert!(!tool_id_match(r"foo\*bar", "fooXbar"));
    }

    #[test]
    fn escape_literal_backslash() {
        assert!(tool_id_match(r"foo\\bar", r"foo\bar"));
        assert!(!tool_id_match(r"foo\\bar", "foobar"));
    }

    #[test]
    fn slash_colon_underscore_are_literal() {
        assert!(tool_id_match("a/b:c_d", "a/b:c_d"));
        assert!(!tool_id_match("a/b:c_d", "a/b:c-d"));
    }

    #[test]
    fn validate_rejects_empty() {
        assert_eq!(validate_tool_id_pattern(""), Err(ToolIdPatternError::Empty));
    }

    #[test]
    fn validate_rejects_dangling_escape() {
        assert_eq!(
            validate_tool_id_pattern(r"foo\"),
            Err(ToolIdPatternError::DanglingEscape)
        );
    }

    #[test]
    fn validate_accepts_well_formed() {
        for p in ["*", "Bash", "mcp:*", r"foo\*bar", r"foo\\bar"] {
            assert!(validate_tool_id_pattern(p).is_ok(), "should accept {p}");
        }
    }
}
