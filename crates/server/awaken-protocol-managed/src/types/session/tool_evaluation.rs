//! The SDK's per-call `evaluation` on Agent tool-use events, projected from
//! the committed permission audit.
//!
//! The runtime records which authored policy decided a gated call and, for
//! `auto`, the grounds of the judgement (ADR-0030 D4). This module is the one
//! place that vocabulary becomes the Managed wire shape
//! (`BetaManagedAgentsAgentToolEvaluation`), so the flat `evaluated_permission`
//! and the structured `evaluation` can never disagree: both are derived here
//! from the same recorded row.

use awaken_agent_contract::audit::permission::{PermissionPolicy, PermissionReasonCode};
use serde::Serialize;

use super::EvaluatedPermission;

/// The SDK's per-call `evaluation` on Agent tool-use events
/// (`BetaManagedAgentsAgentToolEvaluation`): which resolved `permission_policy`
/// produced `evaluated_permission`, and for `auto` the judgement itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolEvaluation {
    AlwaysAllow,
    AlwaysAsk,
    Auto {
        evaluated_permission: AutoEvaluatedPermission,
    },
}

/// The server's per-invocation judgement under the `auto` policy
/// (`BetaManagedAgentsAgentAutoEvaluatedPermission`). Its `type` always equals
/// the event's flat `evaluated_permission`; the reason code is the audit's
/// open registry code, rendered as the SDK's registry string unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutoEvaluatedPermission {
    Allow,
    Ask { reason_code: PermissionReasonCode },
    Deny { reason_code: PermissionReasonCode },
}

/// What the committed audit recorded for one gated call: the decision, and the
/// SDK `evaluation` that decision projects to when the recorded policy is in
/// the SDK vocabulary.
/// SDK: none — the adapter's own record of what the gate decided for one
/// call, read back from the audit trail. Its two halves reach the wire as the
/// event's `evaluated_permission` and `evaluation`; the pair itself is not a
/// shape the SDK declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPermission {
    pub decision: EvaluatedPermission,
    pub evaluation: Option<ToolEvaluation>,
}

impl RecordedPermission {
    /// Project one recorded audit row. Exact and fail-closed: `evaluation` is
    /// absent when no policy was recorded (a historical row, a narrowing, a
    /// plugin gate), when the policy is outside the SDK's `permission_policy`
    /// vocabulary (an authored `rule`), or when the row is internally
    /// inconsistent (a fixed policy with a decision it cannot produce, an
    /// `auto` ask/deny without the grounds the SDK requires). The flat
    /// `evaluated_permission` is reported in every case.
    #[must_use]
    pub fn from_audit(
        decision: EvaluatedPermission,
        policy: Option<PermissionPolicy>,
        reason_code: Option<PermissionReasonCode>,
    ) -> Self {
        let evaluation = match (policy, decision, reason_code) {
            (Some(PermissionPolicy::AlwaysAllow), EvaluatedPermission::Allow, _) => {
                Some(ToolEvaluation::AlwaysAllow)
            }
            (Some(PermissionPolicy::AlwaysAsk), EvaluatedPermission::Ask, _) => {
                Some(ToolEvaluation::AlwaysAsk)
            }
            (Some(PermissionPolicy::Auto), EvaluatedPermission::Allow, _) => {
                Some(ToolEvaluation::Auto {
                    evaluated_permission: AutoEvaluatedPermission::Allow,
                })
            }
            (Some(PermissionPolicy::Auto), EvaluatedPermission::Ask, Some(reason_code)) => {
                Some(ToolEvaluation::Auto {
                    evaluated_permission: AutoEvaluatedPermission::Ask { reason_code },
                })
            }
            (Some(PermissionPolicy::Auto), EvaluatedPermission::Deny, Some(reason_code)) => {
                Some(ToolEvaluation::Auto {
                    evaluated_permission: AutoEvaluatedPermission::Deny { reason_code },
                })
            }
            _ => None,
        };
        Self {
            decision,
            evaluation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(evaluation: ToolEvaluation) -> serde_json::Value {
        serde_json::to_value(evaluation).unwrap()
    }

    #[test]
    fn evaluation_serializes_exactly_as_the_sdk_declares() {
        // The SDK shapes, pinned literally (events.d.ts, 0.125.0):
        // | Variant | wire |
        // |---|---|
        // | always_allow | {"type":"always_allow"} |
        // | always_ask | {"type":"always_ask"} |
        // | auto/allow | {"type":"auto","evaluated_permission":{"type":"allow"}} |
        // | auto/ask | {"type":"auto","evaluated_permission":{"type":"ask","reason_code":"indeterminate"}} |
        // | auto/deny | {"type":"auto","evaluated_permission":{"type":"deny","reason_code":"high_risk"}} |
        assert_eq!(
            wire(ToolEvaluation::AlwaysAllow),
            serde_json::json!({"type":"always_allow"})
        );
        assert_eq!(
            wire(ToolEvaluation::AlwaysAsk),
            serde_json::json!({"type":"always_ask"})
        );
        assert_eq!(
            wire(ToolEvaluation::Auto {
                evaluated_permission: AutoEvaluatedPermission::Allow
            }),
            serde_json::json!({"type":"auto","evaluated_permission":{"type":"allow"}})
        );
        assert_eq!(
            wire(ToolEvaluation::Auto {
                evaluated_permission: AutoEvaluatedPermission::Ask {
                    reason_code: PermissionReasonCode::INDETERMINATE
                }
            }),
            serde_json::json!({
                "type":"auto",
                "evaluated_permission":{"type":"ask","reason_code":"indeterminate"}
            })
        );
        assert_eq!(
            wire(ToolEvaluation::Auto {
                evaluated_permission: AutoEvaluatedPermission::Deny {
                    reason_code: PermissionReasonCode::HIGH_RISK
                }
            }),
            serde_json::json!({
                "type":"auto",
                "evaluated_permission":{"type":"deny","reason_code":"high_risk"}
            })
        );
    }

    #[test]
    fn a_recorded_row_projects_its_evaluation_exactly_or_not_at_all() {
        // Cause/effect graph: C1 the recorded policy is in the SDK vocabulary;
        // C2 the decision is one that policy can produce; C3 an `auto` ask/deny
        // carries its grounds; C4 no policy was recorded (historical row,
        // narrowing, plugin gate, human answer); C5 the policy is an authored
        // `rule` outside the SDK vocabulary. Effects: E1 `evaluation` names the
        // policy (and the auto judgement); E2 `evaluation` is absent while the
        // flat decision is still reported.
        //
        // | Rule | policy | decision | reason | Effect |
        // |---|---|---|---|---|
        // | R1 | always_allow | allow | - | E1 always_allow |
        // | R2 | always_ask | ask | - | E1 always_ask |
        // | R3 | auto | allow | - | E1 auto/allow |
        // | R4 | auto | ask | indeterminate | E1 auto/ask |
        // | R5 | auto | deny | high_risk | E1 auto/deny |
        // | R6 | none (C4) | any | - | E2 |
        // | R7 | rule (C5) | any | - | E2 |
        // | R8 | always_allow | deny (not C2) | - | E2 |
        // | R9 | auto | ask, no reason (not C3) | - | E2 |
        use EvaluatedPermission as D;
        use PermissionPolicy as P;
        use PermissionReasonCode as C;
        /// `(rule, recorded policy, decision, reason_code, expected evaluation)`.
        type Row = (
            &'static str,
            Option<P>,
            D,
            Option<C>,
            Option<ToolEvaluation>,
        );
        let rows: [Row; 11] = [
            (
                "R1",
                Some(P::AlwaysAllow),
                D::Allow,
                None,
                Some(ToolEvaluation::AlwaysAllow),
            ),
            (
                "R2",
                Some(P::AlwaysAsk),
                D::Ask,
                None,
                Some(ToolEvaluation::AlwaysAsk),
            ),
            (
                "R3",
                Some(P::Auto),
                D::Allow,
                None,
                Some(ToolEvaluation::Auto {
                    evaluated_permission: AutoEvaluatedPermission::Allow,
                }),
            ),
            (
                "R4",
                Some(P::Auto),
                D::Ask,
                Some(C::INDETERMINATE),
                Some(ToolEvaluation::Auto {
                    evaluated_permission: AutoEvaluatedPermission::Ask {
                        reason_code: C::INDETERMINATE,
                    },
                }),
            ),
            (
                "R5",
                Some(P::Auto),
                D::Deny,
                Some(C::HIGH_RISK),
                Some(ToolEvaluation::Auto {
                    evaluated_permission: AutoEvaluatedPermission::Deny {
                        reason_code: C::HIGH_RISK,
                    },
                }),
            ),
            ("R6a", None, D::Allow, None, None),
            ("R6b", None, D::Ask, None, None),
            ("R7", Some(P::Rule), D::Allow, None, None),
            ("R8", Some(P::AlwaysAllow), D::Deny, None, None),
            ("R9a", Some(P::Auto), D::Ask, None, None),
            ("R9b", Some(P::Auto), D::Deny, None, None),
        ];
        for (rule, policy, decision, reason_code, expected) in rows {
            let recorded = RecordedPermission::from_audit(decision, policy, reason_code);
            assert_eq!(
                recorded.decision, decision,
                "{rule}: the flat decision is always kept"
            );
            assert_eq!(recorded.evaluation, expected, "{rule}");
            if let Some(ToolEvaluation::Auto {
                evaluated_permission,
            }) = recorded.evaluation
            {
                let judged = match evaluated_permission {
                    AutoEvaluatedPermission::Allow => D::Allow,
                    AutoEvaluatedPermission::Ask { .. } => D::Ask,
                    AutoEvaluatedPermission::Deny { .. } => D::Deny,
                };
                assert_eq!(
                    judged, decision,
                    "{rule}: the auto judgement type equals the flat decision"
                );
            }
        }
    }
}
