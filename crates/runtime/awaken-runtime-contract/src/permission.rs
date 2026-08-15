//! Permission policy and the tool gate.
//!
//! Permission is the *only* authorization path (permission-policy-axis.md,
//! G21). Visibility, selection, capability compatibility, and health never
//! grant — their result types carry no decision (G9). The loop always calls the
//! gate before executing a tool; a permission-backed gate maps a
//! `ToolPermissionVerdict` onto a `GateOutcome`.

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Store;
use serde::{Deserialize, Serialize};

use crate::tool::ToolOutput;

pub use crate::llm::ToolCall;

/// A policy's verdict for one [`ToolCall`]. This is not the later human
/// decision: `RequireConfirmation` causes the Run to commit a `ResumeTicket`,
/// while a user or parent Run eventually supplies the decision that answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolPermissionVerdict {
    Allow,
    Deny { reason: String },
    RequireConfirmation { correlation_id: String },
}

/// Closed decision vocabulary used by the permission-to-gate projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermissionVerdictKind {
    Allow,
    Deny,
    RequireConfirmation,
}

impl ToolPermissionVerdictKind {
    /// Exact authority projection into the final gate vocabulary.
    #[must_use]
    pub const fn gate_outcome_kind(self) -> GateOutcomeKind {
        match self {
            Self::Allow => GateOutcomeKind::Allow,
            Self::Deny => GateOutcomeKind::Block,
            Self::RequireConfirmation => GateOutcomeKind::RequireConfirmation,
        }
    }
}

impl ToolPermissionVerdict {
    /// Return the payload-free decision kind without changing its authority.
    #[must_use]
    pub const fn kind(&self) -> ToolPermissionVerdictKind {
        match self {
            Self::Allow => ToolPermissionVerdictKind::Allow,
            Self::Deny { .. } => ToolPermissionVerdictKind::Deny,
            Self::RequireConfirmation { .. } => ToolPermissionVerdictKind::RequireConfirmation,
        }
    }

    /// Project a policy verdict onto the runtime gate exactly once.
    ///
    /// Denial reasons and confirmation correlation ids are moved unchanged;
    /// only `Allow` can become the executable gate outcome.
    #[must_use]
    pub fn into_gate_outcome(self) -> GateOutcome {
        let expected_kind = self.kind().gate_outcome_kind();
        let outcome = match self {
            Self::Allow => GateOutcome::Allow,
            Self::Deny { reason } => GateOutcome::Block { reason },
            Self::RequireConfirmation { correlation_id } => {
                GateOutcome::RequireConfirmation { correlation_id }
            }
        };
        debug_assert_eq!(outcome.kind(), expected_kind);
        outcome
    }
}

/// The authorization policy. Async because a real policy may consult an
/// external system; it returns only a decision, never executes the tool.
#[async_trait]
pub trait ToolPermissionPolicy: Send + Sync {
    async fn evaluate(&self, call: &ToolCall) -> ToolPermissionVerdict;
}

/// Serializable, per-Run restriction on the tool authority configured by the
/// selected executor. `Configured` preserves that authority; `DenyAll` removes
/// it. Neither variant can add authority, so executors may safely intersect this
/// value with their platform permission policy after durable recovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapabilityNarrowing {
    #[default]
    Configured,
    DenyAll,
}

impl ToolCapabilityNarrowing {
    #[must_use]
    pub fn is_configured(&self) -> bool {
        *self == Self::Configured
    }

    /// Intersect two per-Run restrictions. `DenyAll` is absorbing, so
    /// composition can preserve or remove configured authority, never add it.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Configured, Self::Configured) => Self::Configured,
            _ => Self::DenyAll,
        }
    }
}

/// A capability narrowing that denies every tool. Because per-Run policies are
/// intersected with the configured host/plugin gates, this can remove authority
/// but can never grant it.
pub struct DenyAllTools {
    reason: String,
}

impl DenyAllTools {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl ToolPermissionPolicy for DenyAllTools {
    async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionVerdict {
        ToolPermissionVerdict::Deny {
            reason: self.reason.clone(),
        }
    }
}

/// What the gate tells the loop to do with one tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum GateOutcome {
    /// Execute the tool.
    Allow,
    /// Skip execution; feed a model-visible block result back instead.
    Block { reason: String },
    /// Skip execution; the gate supplied the result directly.
    SetResult(ToolOutput),
    /// Suspend the Run pending an out-of-band permission decision.
    RequireConfirmation { correlation_id: String },
    /// Defer this call as a committed `ScheduledAction` (ADR-0020): the run awaits
    /// and the action is performed later (in-process or by recovery), not decided
    /// by a human. `correlation_id` keys the committed request and its resume.
    /// `action_kind`, when set, names a plugin-owned scheduled-action kind that
    /// must be present in the resolved environment, else the run fails closed
    /// (ADR-0027); `None` is the ordinary tool-backed scheduled action.
    Schedule {
        correlation_id: String,
        action_kind: Option<String>,
    },
}

impl GateOutcome {
    /// The permission-relevant decision label for this outcome — the single
    /// authoritative vocabulary the audit trail (ADR-0030) records. Kept beside
    /// the type so a new outcome variant forces a label here, not in each caller.
    #[must_use]
    pub fn decision_label(&self) -> &'static str {
        self.kind().audit_decision().as_str()
    }

    /// Payload-free gate kind for exact audit and execution projections.
    #[must_use]
    pub const fn kind(&self) -> GateOutcomeKind {
        match self {
            Self::Allow => GateOutcomeKind::Allow,
            Self::Block { .. } => GateOutcomeKind::Block,
            Self::SetResult(_) => GateOutcomeKind::SetResult,
            Self::RequireConfirmation { .. } => GateOutcomeKind::RequireConfirmation,
            Self::Schedule { .. } => GateOutcomeKind::Schedule,
        }
    }
}

/// Closed set of outcomes emitted by the final tool gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcomeKind {
    Allow,
    Block,
    SetResult,
    RequireConfirmation,
    Schedule,
}

impl GateOutcomeKind {
    #[must_use]
    pub const fn audit_decision(self) -> GateAuditDecision {
        match self {
            Self::Allow => GateAuditDecision::Allow,
            Self::Block => GateAuditDecision::Deny,
            Self::SetResult => GateAuditDecision::SetResult,
            Self::RequireConfirmation => GateAuditDecision::Ask,
            Self::Schedule => GateAuditDecision::Schedule,
        }
    }
}

/// Typed audit decision selected before its stable wire label is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAuditDecision {
    Allow,
    Deny,
    Ask,
    SetResult,
    Schedule,
}

impl GateAuditDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::SetResult => "set_result",
            Self::Schedule => "schedule",
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{
        GateAuditDecision, GateOutcomeKind, ToolCapabilityNarrowing, ToolPermissionVerdictKind,
    };

    #[kani::proof]
    fn tool_capability_intersection_never_widens_configured_authority() {
        let left = if kani::any() {
            ToolCapabilityNarrowing::Configured
        } else {
            ToolCapabilityNarrowing::DenyAll
        };
        let right = if kani::any() {
            ToolCapabilityNarrowing::Configured
        } else {
            ToolCapabilityNarrowing::DenyAll
        };
        let effective = left.intersect(right);

        assert_eq!(
            effective == ToolCapabilityNarrowing::Configured,
            left == ToolCapabilityNarrowing::Configured
                && right == ToolCapabilityNarrowing::Configured
        );
        assert_eq!(effective, right.intersect(left));
    }

    #[kani::proof]
    fn permission_verdict_projects_to_exact_non_widening_gate_outcome() {
        let verdict = match kani::any::<u8>() % 3 {
            0 => ToolPermissionVerdictKind::Allow,
            1 => ToolPermissionVerdictKind::Deny,
            _ => ToolPermissionVerdictKind::RequireConfirmation,
        };
        let outcome = verdict.gate_outcome_kind();
        let expected = match verdict {
            ToolPermissionVerdictKind::Allow => GateOutcomeKind::Allow,
            ToolPermissionVerdictKind::Deny => GateOutcomeKind::Block,
            ToolPermissionVerdictKind::RequireConfirmation => GateOutcomeKind::RequireConfirmation,
        };

        assert_eq!(outcome, expected);
        assert_eq!(
            outcome == GateOutcomeKind::Allow,
            verdict == ToolPermissionVerdictKind::Allow
        );
    }

    #[kani::proof]
    fn every_gate_outcome_has_one_exact_audit_label() {
        let kind = match kani::any::<u8>() % 5 {
            0 => GateOutcomeKind::Allow,
            1 => GateOutcomeKind::Block,
            2 => GateOutcomeKind::SetResult,
            3 => GateOutcomeKind::RequireConfirmation,
            _ => GateOutcomeKind::Schedule,
        };
        let expected = match kind {
            GateOutcomeKind::Allow => GateAuditDecision::Allow,
            GateOutcomeKind::Block => GateAuditDecision::Deny,
            GateOutcomeKind::SetResult => GateAuditDecision::SetResult,
            GateOutcomeKind::RequireConfirmation => GateAuditDecision::Ask,
            GateOutcomeKind::Schedule => GateAuditDecision::Schedule,
        };
        assert_eq!(kind.audit_decision(), expected);
        assert_eq!(
            kind.audit_decision() == GateAuditDecision::Allow,
            matches!(kind, GateOutcomeKind::Allow)
        );
    }
}

/// The final invocation gate. The loop calls this for every tool call; only an
/// `Allow` reaches the executor. The loop consults the host gate first, then any
/// plugin-contributed gates in dependency order; a call runs only if every gate
/// allows it, and a permission `Deny` is absolute — a plugin gate can further
/// restrict but never widen what permission allows (G21).
#[async_trait]
pub trait ToolGateHook: Send + Sync {
    /// Stable id, used to bound a plugin-contributed gate under its
    /// `CapabilityBound` (G30). A host-wired gate that is not a plugin
    /// contribution keeps the default.
    fn id(&self) -> &str {
        "gate"
    }

    /// Decide one tool call against the run's read-only state. Most gates ignore
    /// `state`; a state machine gate reads it to enforce a precondition.
    async fn gate(&self, call: &ToolCall, state: &Store) -> GateOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_projection_preserves_denial_and_confirmation_identity() {
        assert_eq!(
            ToolPermissionVerdict::Deny {
                reason: "bound denial".into(),
            }
            .into_gate_outcome(),
            GateOutcome::Block {
                reason: "bound denial".into(),
            }
        );
        assert_eq!(
            ToolPermissionVerdict::RequireConfirmation {
                correlation_id: "call-bound-ticket".into(),
            }
            .into_gate_outcome(),
            GateOutcome::RequireConfirmation {
                correlation_id: "call-bound-ticket".into(),
            }
        );
    }

    #[test]
    fn decision_label_is_the_authoritative_audit_vocabulary_for_every_outcome() {
        // The single label surface the audit trail records (ADR-0030). Each outcome
        // maps to exactly one stable label; a Block reads as "deny" and a Suspend as
        // "ask" (the gate labels mirror the permission decision they came from), while
        // the gate-only outcomes carry their own labels.
        assert_eq!(GateOutcome::Allow.decision_label(), "allow");
        assert_eq!(
            GateOutcome::Block {
                reason: "no".into()
            }
            .decision_label(),
            "deny"
        );
        assert_eq!(
            GateOutcome::RequireConfirmation {
                correlation_id: "t".into()
            }
            .decision_label(),
            "ask"
        );
        assert_eq!(
            GateOutcome::SetResult(ToolOutput::ok("c1", "done")).decision_label(),
            "set_result"
        );
        assert_eq!(
            GateOutcome::Schedule {
                correlation_id: "x".into(),
                action_kind: None,
            }
            .decision_label(),
            "schedule"
        );
    }
}
