//! The closed vocabularies a permission audit fact carries beside its decision.
//!
//! A `PermissionDecided` fact records *what* the gate decided (`allow`, `ask`,
//! `deny`, …). These two enums record *which authored policy* decided it and,
//! for the `auto` policy, the grounds of the judgement. They are the producer
//! side of the Managed Agents `evaluation` object on tool-use events; the
//! projection reads them back from the committed payload, so both stay closed
//! and serialize as stable snake_case labels.

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
/// Closed: the evaluator port has exactly two non-allow conclusions, and each
/// maps to one code. An `auto` allow carries no reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionReasonCode {
    /// The evaluator judged the call too risky to run at all; it was refused.
    JudgedHighRisk,
    /// The evaluator reached no conclusion; the call went to a person.
    Indeterminate,
}

impl PermissionReasonCode {
    /// The stable audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JudgedHighRisk => "judged_high_risk",
            Self::Indeterminate => "indeterminate",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_and_reason_serializes_as_its_stable_label() {
        // Decision table (closed vocabularies, one label each):
        // | Value | wire |
        // | AlwaysAllow | "always_allow" |
        // | AlwaysAsk | "always_ask" |
        // | Auto | "auto" |
        // | Rule | "rule" |
        // | JudgedHighRisk | "judged_high_risk" |
        // | Indeterminate | "indeterminate" |
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
            PermissionReasonCode::JudgedHighRisk,
            PermissionReasonCode::Indeterminate,
        ] {
            let wire = serde_json::to_value(reason).unwrap();
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
}
