//! Output matching logic for tool execution results.

use awaken_contract::contract::tool::{ToolResult, ToolStatus};
use awaken_contract::tool_pattern::{
    FieldCondition, MatchOp, evaluate_field_condition, evaluate_op, value_to_string,
};

/// Matches tool execution output.
#[derive(Debug, Clone)]
pub enum OutputMatcher {
    /// Match any output (always matches).
    Any,
    /// Match by tool status.
    Status(ToolStatusMatcher),
    /// Match by output content (supports both JSON and plain text).
    Content(ContentMatcher),
    /// Match by status AND content.
    Both {
        status: ToolStatusMatcher,
        content: ContentMatcher,
    },
}

/// Matches tool execution status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatusMatcher {
    Success,
    Error,
    Pending,
    Any,
}

/// Matches tool output content — handles both JSON and plain text.
#[derive(Debug, Clone)]
pub enum ContentMatcher {
    /// Match output text with glob/regex/exact (works for both JSON stringified and plain text).
    Text { op: MatchOp, value: String },
    /// Match JSON fields in the output (only works if output is valid JSON).
    JsonFields(Vec<FieldCondition>),
}

/// Check whether a tool result matches an output matcher.
#[must_use]
pub fn output_matches(matcher: &OutputMatcher, result: &ToolResult) -> bool {
    match matcher {
        OutputMatcher::Any => true,
        OutputMatcher::Status(status_matcher) => status_match(status_matcher, &result.status),
        OutputMatcher::Content(content_matcher) => content_match(content_matcher, result),
        OutputMatcher::Both { status, content } => {
            status_match(status, &result.status) && content_match(content, result)
        }
    }
}

fn status_match(matcher: &ToolStatusMatcher, status: &ToolStatus) -> bool {
    match matcher {
        ToolStatusMatcher::Any => true,
        ToolStatusMatcher::Success => matches!(status, ToolStatus::Success),
        ToolStatusMatcher::Error => matches!(status, ToolStatus::Error),
        ToolStatusMatcher::Pending => matches!(status, ToolStatus::Pending),
    }
}

fn content_match(matcher: &ContentMatcher, result: &ToolResult) -> bool {
    match matcher {
        ContentMatcher::Text { op, value } => {
            let text = data_to_string(&result.data);
            evaluate_op(op, value, &text)
        }
        ContentMatcher::JsonFields(conditions) => {
            if result.data.is_null() {
                return false;
            }
            conditions
                .iter()
                .all(|cond| evaluate_field_condition(cond, &result.data))
        }
    }
}

fn data_to_string(data: &serde_json::Value) -> String {
    if data.is_null() {
        return String::new();
    }
    value_to_string(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::tool_pattern::{FieldCondition, MatchOp, PathSegment};
    use serde_json::json;

    fn success_result(data: serde_json::Value) -> ToolResult {
        ToolResult::success("test", data)
    }

    fn error_result(msg: &str) -> ToolResult {
        ToolResult::error("test", msg)
    }

    // --- Status matching ---

    #[test]
    fn any_output_always_matches() {
        assert!(output_matches(
            &OutputMatcher::Any,
            &success_result(json!("ok"))
        ));
        assert!(output_matches(&OutputMatcher::Any, &error_result("boom")));
    }

    #[test]
    fn status_success_matches() {
        let m = OutputMatcher::Status(ToolStatusMatcher::Success);
        assert!(output_matches(&m, &success_result(json!("ok"))));
        assert!(!output_matches(&m, &error_result("boom")));
    }

    #[test]
    fn status_error_matches() {
        let m = OutputMatcher::Status(ToolStatusMatcher::Error);
        assert!(!output_matches(&m, &success_result(json!("ok"))));
        assert!(output_matches(&m, &error_result("boom")));
    }

    #[test]
    fn status_pending_matches() {
        let m = OutputMatcher::Status(ToolStatusMatcher::Pending);
        let pending = ToolResult::suspended("test", "waiting");
        assert!(output_matches(&m, &pending));
        assert!(!output_matches(&m, &success_result(json!("ok"))));
    }

    #[test]
    fn status_any_matches_all() {
        let m = OutputMatcher::Status(ToolStatusMatcher::Any);
        assert!(output_matches(&m, &success_result(json!("ok"))));
        assert!(output_matches(&m, &error_result("boom")));
    }

    // --- Text content matching ---

    #[test]
    fn text_glob_matches_string_data() {
        let m = OutputMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Glob,
            value: "*permission denied*".into(),
        });
        let result = success_result(json!("Error: permission denied for /etc/passwd"));
        assert!(output_matches(&m, &result));
    }

    #[test]
    fn text_glob_no_match() {
        let m = OutputMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Glob,
            value: "*permission denied*".into(),
        });
        let result = success_result(json!("File created successfully"));
        assert!(!output_matches(&m, &result));
    }

    #[test]
    fn text_exact_matches() {
        let m = OutputMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Exact,
            value: "done".into(),
        });
        assert!(output_matches(&m, &success_result(json!("done"))));
        assert!(!output_matches(&m, &success_result(json!("not done"))));
    }

    #[test]
    fn text_regex_matches() {
        let m = OutputMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Regex,
            value: r"(?i)error|fail".into(),
        });
        assert!(output_matches(
            &m,
            &success_result(json!("FATAL ERROR occurred"))
        ));
        assert!(!output_matches(&m, &success_result(json!("all good"))));
    }

    #[test]
    fn text_matches_null_data_as_empty() {
        let m = OutputMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Exact,
            value: "".into(),
        });
        let result = ToolResult::error("test", "msg");
        // data is Null for error results
        assert!(output_matches(&m, &result));
    }

    #[test]
    fn text_matches_json_stringified() {
        let m = OutputMatcher::Content(ContentMatcher::Text {
            op: MatchOp::Glob,
            value: "*status*".into(),
        });
        let result = success_result(json!({"status": "ok", "count": 42}));
        assert!(output_matches(&m, &result));
    }

    // --- JSON field matching ---

    #[test]
    fn json_fields_match() {
        let m = OutputMatcher::Content(ContentMatcher::JsonFields(vec![FieldCondition {
            path: vec![PathSegment::Field("status".into())],
            op: MatchOp::Exact,
            value: "ok".into(),
        }]));
        let result = success_result(json!({"status": "ok"}));
        assert!(output_matches(&m, &result));
    }

    #[test]
    fn json_fields_no_match() {
        let m = OutputMatcher::Content(ContentMatcher::JsonFields(vec![FieldCondition {
            path: vec![PathSegment::Field("status".into())],
            op: MatchOp::Exact,
            value: "ok".into(),
        }]));
        let result = success_result(json!({"status": "error"}));
        assert!(!output_matches(&m, &result));
    }

    #[test]
    fn json_fields_null_data_no_match() {
        let m = OutputMatcher::Content(ContentMatcher::JsonFields(vec![FieldCondition {
            path: vec![PathSegment::Field("status".into())],
            op: MatchOp::Exact,
            value: "ok".into(),
        }]));
        let result = ToolResult::error("test", "msg");
        assert!(!output_matches(&m, &result));
    }

    #[test]
    fn json_fields_multiple_conditions_and() {
        let m = OutputMatcher::Content(ContentMatcher::JsonFields(vec![
            FieldCondition {
                path: vec![PathSegment::Field("code".into())],
                op: MatchOp::Exact,
                value: "200".into(),
            },
            FieldCondition {
                path: vec![PathSegment::Field("body".into())],
                op: MatchOp::Glob,
                value: "*success*".into(),
            },
        ]));
        let result = success_result(json!({"code": 200, "body": "Operation success"}));
        assert!(output_matches(&m, &result));

        let partial = success_result(json!({"code": 200, "body": "failed"}));
        assert!(!output_matches(&m, &partial));
    }

    // --- Combined status + content ---

    #[test]
    fn both_status_and_content_must_match() {
        let m = OutputMatcher::Both {
            status: ToolStatusMatcher::Error,
            content: ContentMatcher::Text {
                op: MatchOp::Glob,
                value: "*permission denied*".into(),
            },
        };

        let error_with_perm =
            ToolResult::success("test", json!("Error: permission denied for /etc/passwd"));
        // Status doesn't match (Success != Error)
        assert!(!output_matches(&m, &error_with_perm));

        let actual_error = ToolResult::error("test", "permission denied for /etc/passwd");
        // Status matches (Error) but data is Null, message is in `message` not `data`
        assert!(!output_matches(&m, &actual_error));
    }

    #[test]
    fn both_matches_when_all_conditions_met() {
        let m = OutputMatcher::Both {
            status: ToolStatusMatcher::Success,
            content: ContentMatcher::Text {
                op: MatchOp::Glob,
                value: "*deleted*".into(),
            },
        };
        let result = success_result(json!("3 files deleted"));
        assert!(output_matches(&m, &result));
    }
}
