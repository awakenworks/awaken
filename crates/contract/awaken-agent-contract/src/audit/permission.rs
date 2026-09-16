//! The vocabularies a permission audit fact carries beside its decision.
//!
//! A `PermissionDecided` fact records *what* the gate decided (`allow`, `ask`,
//! `deny`, …). These types record *which authored policy* decided it and, for
//! the `auto` policy, the grounds of the judgement. They are the producer side
//! of the Managed Agents `evaluation` object on tool-use events, and the
//! projection reads them back from the committed payload. The policy is a
//! closed vocabulary; the reason code is an open registry, because the SDK
//! declares it one and a reader must carry a code it has no name for.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::toolset::ToolPermissionRequirement;

/// Which authored permission policy produced a gate decision (ADR-0030 D4).
///
/// The first three are the toolset `permission_policy` vocabulary an author
/// states per toolset or per tool. `Rule` names an explicit authored rule
/// outside that vocabulary — a pattern rule in a ruleset, the ruleset's own
/// authored default, or a tool the toolset disabled outright. A decision made
/// by something that is not an authored permission policy at all (a per-Run
/// capability narrowing, a plugin gate, a person answering a ticket) carries no
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    AlwaysAllow,
    AlwaysAsk,
    Auto,
    Rule,
}

impl PermissionPolicy {
    /// The stable audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlwaysAllow => "always_allow",
            Self::AlwaysAsk => "always_ask",
            Self::Auto => "auto",
            Self::Rule => "rule",
        }
    }
}

impl From<ToolPermissionRequirement> for PermissionPolicy {
    /// Every authored requirement names exactly one policy; the projection is
    /// total so a new requirement cannot silently audit as another.
    fn from(requirement: ToolPermissionRequirement) -> Self {
        match requirement {
            ToolPermissionRequirement::AlwaysAllow => Self::AlwaysAllow,
            ToolPermissionRequirement::AlwaysAsk => Self::AlwaysAsk,
            ToolPermissionRequirement::Auto => Self::Auto,
        }
    }
}

/// The grounds of an `auto` judgement that did not allow the call.
///
/// An open registry, as the Managed Agents SDK declares `reason_code`: a
/// `string` whose documented values are not exhaustive, which clients must
/// tolerate. The codes named here are the ones the shipped evaluator produces;
/// a durable row written by a newer producer may carry one this version has no
/// name for, and reading it must keep that code rather than fail or drop it.
/// Equality is by label, so a decoded `"indeterminate"` is
/// [`PermissionReasonCode::INDETERMINATE`]. An `auto` allow carries no reason.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionReasonCode(Cow<'static, str>);

impl PermissionReasonCode {
    /// The evaluator judged the call too risky to run at all; it was refused.
    pub const HIGH_RISK: Self = Self(Cow::Borrowed("high_risk"));
    /// The evaluator reached no conclusion; the call went to a person.
    pub const INDETERMINATE: Self = Self(Cow::Borrowed("indeterminate"));

    /// A code from the open registry that this type does not name.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self(Cow::Owned(code.into()))
    }

    /// The stable audit label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A person's answer to a call the gate routed to them.
///
/// Recorded as its own `PermissionConfirmed` fact rather than as another
/// `PermissionDecided` row: the decision states what the gate concluded and on
/// which authored basis, and a person answering the wait is a different actor
/// making a different kind of statement. Folding the answer into the decision
/// stream made a reader that counts or projects decisions see a second,
/// policy-less "decision" for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionConfirmation {
    /// The person allowed the call; it proceeds to execution.
    Allow,
    /// The person refused the call; it completes blocked, never executed.
    Deny,
}

impl PermissionConfirmation {
    /// The stable audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

impl From<&crate::agent::awaiting::PermissionDecision> for PermissionConfirmation {
    /// Every answer a permission ticket accepts is exactly one confirmation;
    /// the note or reason text stays with the tool result, not the audit label.
    fn from(decision: &crate::agent::awaiting::PermissionDecision) -> Self {
        match decision {
            crate::agent::awaiting::PermissionDecision::Allow { .. } => Self::Allow,
            crate::agent::awaiting::PermissionDecision::Deny { .. } => Self::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_and_reason_serializes_as_its_stable_label() {
        // Decision table (one label each):
        // | Value | wire |
        // | AlwaysAllow | "always_allow" |
        // | AlwaysAsk | "always_ask" |
        // | Auto | "auto" |
        // | Rule | "rule" |
        // | HIGH_RISK | "high_risk" |
        // | INDETERMINATE | "indeterminate" |
        for policy in [
            PermissionPolicy::AlwaysAllow,
            PermissionPolicy::AlwaysAsk,
            PermissionPolicy::Auto,
            PermissionPolicy::Rule,
        ] {
            let wire = serde_json::to_value(policy).unwrap();
            assert_eq!(wire, serde_json::Value::String(policy.as_str().into()));
            assert_eq!(
                serde_json::from_value::<PermissionPolicy>(wire).unwrap(),
                policy
            );
        }
        for reason in [
            PermissionReasonCode::HIGH_RISK,
            PermissionReasonCode::INDETERMINATE,
        ] {
            let wire = serde_json::to_value(&reason).unwrap();
            assert_eq!(wire, serde_json::Value::String(reason.as_str().into()));
            assert_eq!(
                serde_json::from_value::<PermissionReasonCode>(wire).unwrap(),
                reason
            );
        }
        assert!(serde_json::from_value::<PermissionPolicy>(serde_json::json!("bypass")).is_err());
    }

    #[test]
    fn every_authored_requirement_names_exactly_one_policy() {
        assert_eq!(
            PermissionPolicy::from(ToolPermissionRequirement::AlwaysAllow),
            PermissionPolicy::AlwaysAllow
        );
        assert_eq!(
            PermissionPolicy::from(ToolPermissionRequirement::AlwaysAsk),
            PermissionPolicy::AlwaysAsk
        );
        assert_eq!(
            PermissionPolicy::from(ToolPermissionRequirement::Auto),
            PermissionPolicy::Auto
        );
    }

    #[test]
    fn a_reason_code_outside_the_named_set_decodes_and_round_trips() {
        // Causes: C1 the label is one this version names; C2 the label is a
        // registry code this version has never heard of; C3 the value is not a
        // string at all. Effects: E1 it decodes; E2 it re-encodes to the exact
        // label; E3 it equals the named constant exactly when the label does;
        // E4 decoding fails.
        //
        // | Rule | input | E1 | E2 | E3 | E4 |
        // |---|---|---|---|---|---|
        // | O1 (C1) | "indeterminate" | T | T | == INDETERMINATE | F |
        // | O2 (C2) | "exfiltration_risk" | T | T | != any named | F |
        // | O3 (C3) | 7 | F | - | - | T |
        let known: PermissionReasonCode =
            serde_json::from_value(serde_json::json!("indeterminate")).expect("O1/E1");
        assert_eq!(known, PermissionReasonCode::INDETERMINATE, "O1/E3");
        assert_eq!(
            serde_json::to_value(&known).unwrap(),
            serde_json::json!("indeterminate"),
            "O1/E2"
        );

        let unknown: PermissionReasonCode =
            serde_json::from_value(serde_json::json!("exfiltration_risk")).expect("O2/E1");
        assert_eq!(unknown.as_str(), "exfiltration_risk");
        assert_eq!(
            unknown,
            PermissionReasonCode::new("exfiltration_risk"),
            "O2/E3"
        );
        assert_ne!(unknown, PermissionReasonCode::HIGH_RISK, "O2/E3");
        assert_ne!(unknown, PermissionReasonCode::INDETERMINATE, "O2/E3");
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            serde_json::json!("exfiltration_risk"),
            "O2/E2"
        );

        assert!(
            serde_json::from_value::<PermissionReasonCode>(serde_json::json!(7)).is_err(),
            "O3/E4"
        );
    }

    #[test]
    fn a_person_answer_is_exactly_one_confirmation_label() {
        // | Rule | ticket answer | confirmation | wire |
        // |---|---|---|---|
        // | C1 | Allow { note } | Allow | "allow" |
        // | C2 | Deny { reason } | Deny | "deny" |
        // | C3 | any other label | decode error | - |
        use crate::agent::awaiting::PermissionDecision;
        for (rule, answer, expected) in [
            (
                "C1",
                PermissionDecision::Allow {
                    note: Some("ok".into()),
                },
                PermissionConfirmation::Allow,
            ),
            (
                "C2",
                PermissionDecision::Deny {
                    reason: Some("no".into()),
                },
                PermissionConfirmation::Deny,
            ),
        ] {
            let confirmation = PermissionConfirmation::from(&answer);
            assert_eq!(confirmation, expected, "{rule}");
            let wire = serde_json::to_value(confirmation).unwrap();
            assert_eq!(wire, serde_json::json!(expected.as_str()), "{rule}");
            assert_eq!(
                serde_json::from_value::<PermissionConfirmation>(wire).unwrap(),
                expected,
                "{rule}"
            );
        }
        assert!(
            serde_json::from_value::<PermissionConfirmation>(serde_json::json!("approved"))
                .is_err(),
            "C3"
        );
    }
}
