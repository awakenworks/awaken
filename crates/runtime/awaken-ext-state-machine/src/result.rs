//! Tool-result matching for result-conditioned transitions.
//!
//! Built directly on [`awaken_tool_pattern`]. A result is viewed through
//! [`ToolResultView`], a thin adapter over the runtime `ToolOutput` (a status
//! bit plus stringified content), so a transition can route on success/error and
//! on the content payload.

use awaken_tool_pattern::{FieldCondition, MatchOp, evaluate_field_condition, evaluate_op};

/// A tool result as the state machine sees it: a success/error status and the
/// stringified content. Adapted from the runtime `ToolOutput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultView<'a> {
    pub is_error: bool,
    pub content: &'a str,
}

impl<'a> ToolResultView<'a> {
    #[must_use]
    pub fn new(is_error: bool, content: &'a str) -> Self {
        Self { is_error, content }
    }
}

/// Condition on a tool result, evaluated at advance time (post-execution).
#[derive(Debug, Clone)]
pub enum ResultMatcher {
    /// Matches any result.
    Any,
    /// Matches on status only.
    Status(StatusMatcher),
    /// Matches on content only.
    Content(ContentMatcher),
    /// Matches on status AND content.
    Both {
        status: StatusMatcher,
        content: ContentMatcher,
    },
}

/// Tool execution status matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusMatcher {
    Success,
    Error,
    Any,
}

/// Content matcher over the result payload.
#[derive(Debug, Clone)]
pub enum ContentMatcher {
    /// Glob/regex/exact over the stringified content.
    Text { op: MatchOp, value: String },
    /// Field conditions over the content parsed as JSON (AND semantics). A
    /// content that is not valid JSON never matches.
    JsonFields(Vec<FieldCondition>),
}

/// Whether a tool result satisfies a matcher.
#[must_use]
pub fn result_matches(matcher: &ResultMatcher, result: &ToolResultView<'_>) -> bool {
    match matcher {
        ResultMatcher::Any => true,
        ResultMatcher::Status(s) => status_match(*s, result),
        ResultMatcher::Content(c) => content_match(c, result),
        ResultMatcher::Both { status, content } => {
            status_match(*status, result) && content_match(content, result)
        }
    }
}

fn status_match(matcher: StatusMatcher, result: &ToolResultView<'_>) -> bool {
    match matcher {
        StatusMatcher::Any => true,
        StatusMatcher::Success => !result.is_error,
        StatusMatcher::Error => result.is_error,
    }
}

fn content_match(matcher: &ContentMatcher, result: &ToolResultView<'_>) -> bool {
    match matcher {
        ContentMatcher::Text { op, value } => evaluate_op(op, value, result.content),
        ContentMatcher::JsonFields(conditions) => {
            match serde_json::from_str::<serde_json::Value>(result.content) {
                Ok(data) if !data.is_null() => conditions
                    .iter()
                    .all(|cond| evaluate_field_condition(cond, &data)),
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_tool_pattern::PathSegment;

    fn ok(content: &str) -> ToolResultView<'_> {
        ToolResultView::new(false, content)
    }
    fn err(content: &str) -> ToolResultView<'_> {
        ToolResultView::new(true, content)
    }

    #[test]
    fn any_matches() {
        assert!(result_matches(&ResultMatcher::Any, &err("x")));
    }

    #[test]
    fn status_success_and_error() {
        assert!(result_matches(
            &ResultMatcher::Status(StatusMatcher::Success),
            &ok("ok")
        ));
        assert!(!result_matches(
            &ResultMatcher::Status(StatusMatcher::Success),
            &err("boom")
        ));
        assert!(result_matches(
            &ResultMatcher::Status(StatusMatcher::Error),
            &err("boom")
        ));
    }

    #[test]
    fn text_matches_content() {
        let m = ResultMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Glob,
            value: "*permission denied*".into(),
        });
        assert!(result_matches(&m, &err("permission denied for /etc")));
        assert!(!result_matches(&m, &err("ok")));
    }

    #[test]
    fn json_fields_match_content_json() {
        let m = ResultMatcher::Content(ContentMatcher::JsonFields(vec![FieldCondition {
            path: vec![PathSegment::Field("remaining".into())],
            op: MatchOp::Exact,
            value: "0".into(),
        }]));
        assert!(result_matches(&m, &ok(r#"{"remaining": 0}"#)));
        assert!(!result_matches(&m, &ok(r#"{"remaining": 3}"#)));
        assert!(!result_matches(&m, &ok("not json")));
    }

    #[test]
    fn both_requires_status_and_content() {
        let m = ResultMatcher::Both {
            status: StatusMatcher::Success,
            content: ContentMatcher::Text {
                op: MatchOp::Glob,
                value: "*done*".into(),
            },
        };
        assert!(result_matches(&m, &ok("all done")));
        assert!(!result_matches(&m, &err("all done")));
    }
}
