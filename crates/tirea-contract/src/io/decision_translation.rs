//! Shared decision-translation helpers for converting [`SuspensionResponse`]
//! values into [`ToolCallDecision`] commands.
//!
//! Both the AG-UI and AI-SDK protocol adapters delegate to these functions so
//! that approval/denial semantics are defined in exactly one place.

use serde_json::Value;

use crate::io::decision::ToolCallDecision;
use crate::runtime::tool_call::lifecycle::{ResumeDecisionAction, ToolCallResume};
use crate::runtime::tool_call::suspension::SuspensionResponse;

/// Convert a [`SuspensionResponse`] into a routable [`ToolCallDecision`].
pub fn suspension_response_to_decision(response: SuspensionResponse) -> ToolCallDecision {
    let action = decision_action_from_result(&response.result);
    let reason = if matches!(action, ResumeDecisionAction::Cancel) {
        decision_reason_from_result(&response.result)
    } else {
        None
    };
    ToolCallDecision {
        target_id: response.target_id.clone(),
        resume: ToolCallResume {
            decision_id: format!("decision_{}", response.target_id),
            action,
            result: response.result,
            reason,
            updated_at: current_unix_millis(),
        },
    }
}

/// Determine whether a result value represents approval or denial.
///
/// - `Bool(true)` → Resume, `Bool(false)` → Cancel
/// - `String` → Resume unless it is a denial token (e.g. "deny", "cancel")
/// - `Object` → Cancel when `approved: false`, a boolean denial flag is set,
///   or a status/decision/action field contains a denial token
/// - **All other types** (`Null`, `Array`, `Number`) → **Cancel** (safe default)
pub fn decision_action_from_result(result: &Value) -> ResumeDecisionAction {
    match result {
        Value::Bool(approved) => {
            if *approved {
                ResumeDecisionAction::Resume
            } else {
                ResumeDecisionAction::Cancel
            }
        }
        Value::String(value) => {
            if is_denied_token(value) {
                ResumeDecisionAction::Cancel
            } else {
                ResumeDecisionAction::Resume
            }
        }
        Value::Object(obj) => {
            if obj
                .get("approved")
                .and_then(Value::as_bool)
                .map(|approved| !approved)
                .unwrap_or(false)
            {
                return ResumeDecisionAction::Cancel;
            }
            if [
                "denied",
                "reject",
                "rejected",
                "cancel",
                "canceled",
                "cancelled",
                "abort",
                "aborted",
            ]
            .iter()
            .any(|key| obj.get(*key).and_then(Value::as_bool).unwrap_or(false))
            {
                return ResumeDecisionAction::Cancel;
            }
            if ["status", "decision", "action"].iter().any(|key| {
                obj.get(*key)
                    .and_then(Value::as_str)
                    .map(is_denied_token)
                    .unwrap_or(false)
            }) {
                return ResumeDecisionAction::Cancel;
            }
            ResumeDecisionAction::Resume
        }
        // Null, Array, Number → default to Cancel (safe: reject unknown types).
        _ => ResumeDecisionAction::Cancel,
    }
}

/// Extract a human-readable reason string from a denial result.
pub fn decision_reason_from_result(result: &Value) -> Option<String> {
    match result {
        Value::String(text) => {
            if text.trim().is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Object(obj) => obj
            .get("reason")
            .and_then(Value::as_str)
            .or_else(|| obj.get("message").and_then(Value::as_str))
            .or_else(|| obj.get("error").and_then(Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

/// Return `true` if a string value represents a denial intent.
pub fn is_denied_token(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false"
            | "no"
            | "denied"
            | "deny"
            | "reject"
            | "rejected"
            | "cancel"
            | "canceled"
            | "cancelled"
            | "abort"
            | "aborted"
    )
}

/// Current time as milliseconds since the Unix epoch.
pub fn current_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bool_true_resumes() {
        assert!(matches!(
            decision_action_from_result(&json!(true)),
            ResumeDecisionAction::Resume
        ));
    }

    #[test]
    fn bool_false_cancels() {
        assert!(matches!(
            decision_action_from_result(&json!(false)),
            ResumeDecisionAction::Cancel
        ));
    }

    #[test]
    fn denied_tokens_cancel() {
        for token in &[
            "deny",
            "denied",
            "reject",
            "rejected",
            "cancel",
            "canceled",
            "cancelled",
            "abort",
            "aborted",
            "no",
            "false",
        ] {
            assert!(
                matches!(
                    decision_action_from_result(&json!(token)),
                    ResumeDecisionAction::Cancel
                ),
                "expected Cancel for token: {token}"
            );
        }
    }

    #[test]
    fn approval_string_resumes() {
        assert!(matches!(
            decision_action_from_result(&json!("yes")),
            ResumeDecisionAction::Resume
        ));
    }

    #[test]
    fn null_array_number_cancel() {
        for value in &[json!(null), json!([1, 2]), json!(42)] {
            assert!(
                matches!(
                    decision_action_from_result(value),
                    ResumeDecisionAction::Cancel
                ),
                "expected Cancel for value: {value}"
            );
        }
    }

    #[test]
    fn object_approved_false_cancels() {
        assert!(matches!(
            decision_action_from_result(&json!({"approved": false})),
            ResumeDecisionAction::Cancel
        ));
    }

    #[test]
    fn object_boolean_denial_flags() {
        for key in [
            "denied",
            "reject",
            "rejected",
            "cancel",
            "canceled",
            "cancelled",
            "abort",
            "aborted",
        ] {
            let val = json!({ key: true });
            assert!(
                matches!(
                    decision_action_from_result(&val),
                    ResumeDecisionAction::Cancel
                ),
                "expected Cancel for key: {key}"
            );
        }
    }

    #[test]
    fn object_status_field_denied() {
        assert!(matches!(
            decision_action_from_result(&json!({"status": "cancelled"})),
            ResumeDecisionAction::Cancel
        ));
    }

    #[test]
    fn reason_extraction() {
        assert_eq!(
            decision_reason_from_result(&json!("not allowed")),
            Some("not allowed".to_string())
        );
        assert_eq!(
            decision_reason_from_result(&json!({"reason": "policy violation"})),
            Some("policy violation".to_string())
        );
        assert_eq!(decision_reason_from_result(&json!(null)), None);
    }

    #[test]
    fn suspension_response_converts_to_decision() {
        let response = SuspensionResponse::new("fc_1", json!(true));
        let decision = suspension_response_to_decision(response);
        assert_eq!(decision.target_id, "fc_1");
        assert!(matches!(
            decision.resume.action,
            ResumeDecisionAction::Resume
        ));
    }
}
