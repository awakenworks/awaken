//! Shared encoder utilities for protocol transcoders.
//!
//! Contains common logic extracted from AG-UI, AI SDK v6, and ACP encoders
//! to reduce duplication across protocol implementations.

use serde_json::Value;

/// Guards against emitting events after a terminal event (RunFinish / Error).
///
/// All protocol encoders must suppress output once the stream has finished.
/// This struct encapsulates the finished-flag check pattern.
#[derive(Debug, Default)]
pub(crate) struct TerminalGuard {
    finished: bool,
}

impl TerminalGuard {
    pub fn new() -> Self {
        Self { finished: false }
    }

    /// Returns `true` if a terminal event has already been processed.
    /// Callers should return an empty vec when this is true.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Marks the stream as finished. Subsequent calls to `is_finished` return `true`.
    pub fn mark_finished(&mut self) {
        self.finished = true;
    }
}

/// Classification of a `ToolCallResumed` result payload.
///
/// The AI SDK v6 and ACP encoders both inspect the JSON result from a resumed
/// tool call to determine whether it represents an error, a denial, or a
/// successful completion. This enum captures that three-way classification.
#[derive(Debug)]
pub(crate) enum ResumedOutcome<'a> {
    /// The result contains an `"error"` field with a string message.
    Error { message: &'a str },
    /// The result contains `"approved": false`, indicating the user denied the tool call.
    Denied,
    /// The result is a normal completion (neither error nor denied).
    Success,
}

/// Classify a `ToolCallResumed` result payload.
///
/// Checks for `"error"` string field first, then `"approved": false`, otherwise
/// treats it as a successful completion.
pub(crate) fn classify_resumed_result(result: &Value) -> ResumedOutcome<'_> {
    if let Some(error_msg) = result.get("error").and_then(Value::as_str) {
        ResumedOutcome::Error { message: error_msg }
    } else if result.get("approved") == Some(&serde_json::json!(false)) {
        ResumedOutcome::Denied
    } else {
        ResumedOutcome::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── TerminalGuard tests ──────────────────────────────────────────

    #[test]
    fn terminal_guard_starts_unfinished() {
        let guard = TerminalGuard::new();
        assert!(!guard.is_finished());
    }

    #[test]
    fn terminal_guard_default_starts_unfinished() {
        let guard = TerminalGuard::default();
        assert!(!guard.is_finished());
    }

    #[test]
    fn terminal_guard_mark_finished_sets_flag() {
        let mut guard = TerminalGuard::new();
        guard.mark_finished();
        assert!(guard.is_finished());
    }

    #[test]
    fn terminal_guard_mark_finished_is_idempotent() {
        let mut guard = TerminalGuard::new();
        guard.mark_finished();
        guard.mark_finished();
        assert!(guard.is_finished());
    }

    // ── classify_resumed_result tests ────────────────────────────────

    #[test]
    fn resumed_error_with_message() {
        let result = json!({"error": "something went wrong"});
        match classify_resumed_result(&result) {
            ResumedOutcome::Error { message } => {
                assert_eq!(message, "something went wrong");
            }
            other => panic!("expected Error, got: {other:?}"),
        }
    }

    #[test]
    fn resumed_error_non_string_treated_as_success() {
        // "error" field exists but is not a string — not classified as error
        let result = json!({"error": 42});
        assert!(matches!(
            classify_resumed_result(&result),
            ResumedOutcome::Success
        ));
    }

    #[test]
    fn resumed_denied() {
        let result = json!({"approved": false});
        assert!(matches!(
            classify_resumed_result(&result),
            ResumedOutcome::Denied
        ));
    }

    #[test]
    fn resumed_approved_true_is_success() {
        let result = json!({"approved": true});
        assert!(matches!(
            classify_resumed_result(&result),
            ResumedOutcome::Success
        ));
    }

    #[test]
    fn resumed_no_special_fields_is_success() {
        let result = json!({"data": "hello"});
        assert!(matches!(
            classify_resumed_result(&result),
            ResumedOutcome::Success
        ));
    }

    #[test]
    fn resumed_empty_object_is_success() {
        let result = json!({});
        assert!(matches!(
            classify_resumed_result(&result),
            ResumedOutcome::Success
        ));
    }

    #[test]
    fn resumed_error_takes_precedence_over_denied() {
        // If both "error" and "approved: false" are present, error wins
        let result = json!({"error": "fail", "approved": false});
        assert!(matches!(
            classify_resumed_result(&result),
            ResumedOutcome::Error { .. }
        ));
    }
}
