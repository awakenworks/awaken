//! Catalog diagnostic surface for [`AgentSpec`].
//!
//! Produces a list of [`ValidationIssue`]s describing problems with the
//! four tool catalog fields. Callers decide policy based on
//! [`IssueSeverity`]: warnings can be logged and ignored; errors should
//! cause the caller to refuse to load the spec.

use super::AgentSpec;

/// One validation finding produced by [`AgentSpec::validate_catalog`].
/// Callers decide whether to surface as a warning or hard error based on
/// `severity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    pub field: &'static str,
    pub entry: String,
    pub message: String,
}

/// Severity tier for a [`ValidationIssue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    /// The catalog entry is loadable but probably not what the user
    /// intended (e.g. `*` in a literal field).
    Warning,
    /// The catalog entry is unparseable; caller should refuse to load.
    Error,
}

impl AgentSpec {
    /// Validate the catalog fields. Returns issues; empty list = OK.
    ///
    /// - `*` in `allowed_tools` / `excluded_tools` yields a `Warning`
    ///   (the entry loads as a literal that's unlikely to match anything).
    /// - Unparseable entries in `allowed_tool_patterns` /
    ///   `excluded_tool_patterns` yield an `Error` (caller should refuse
    ///   to load).
    ///
    /// Callers decide policy: log warnings, refuse on errors, etc.
    #[must_use]
    pub fn validate_catalog(&self) -> Vec<ValidationIssue> {
        let mut out = Vec::new();
        // Literal fields: warn on unescaped `*`.
        for (field, list) in [
            ("allowed_tools", self.allowed_tools.as_deref()),
            ("excluded_tools", self.excluded_tools.as_deref()),
        ] {
            if let Some(entries) = list {
                for e in entries {
                    if contains_unescaped_star(e) {
                        out.push(ValidationIssue {
                            severity: IssueSeverity::Warning,
                            field,
                            entry: e.clone(),
                            message: format!(
                                "{field}[{e}] contains '*' but literal fields never \
                                 parse patterns; this entry will only match the literal \
                                 tool id '{e}'. Move it to {field}_patterns if you \
                                 intended a glob."
                            ),
                        });
                    }
                }
            }
        }
        // Pattern fields: error on unparseable entries.
        for (field, list) in [
            (
                "allowed_tool_patterns",
                self.allowed_tool_patterns.as_deref(),
            ),
            (
                "excluded_tool_patterns",
                self.excluded_tool_patterns.as_deref(),
            ),
        ] {
            if let Some(entries) = list {
                for e in entries {
                    if let Err(err) = awaken_tool_pattern::validate_tool_id_pattern(e) {
                        out.push(ValidationIssue {
                            severity: IssueSeverity::Error,
                            field,
                            entry: e.clone(),
                            message: format!("{field}[{e}] is not a valid pattern: {err}"),
                        });
                    }
                }
            }
        }
        out
    }
}

/// True if `s` contains a `*` that is not preceded by a `\` escape.
/// Mirrors the escape rules in [`awaken_tool_pattern`].
fn contains_unescaped_star(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 2;
            continue;
        }
        if b[i] == b'*' {
            return true;
        }
        i += 1;
    }
    false
}
