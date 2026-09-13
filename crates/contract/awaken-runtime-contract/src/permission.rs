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
pub use awaken_agent_contract::audit::permission::{PermissionPolicy, PermissionReasonCode};

/// The authored policy a permission verdict came from, with the grounds of an
/// `auto` judgement (ADR-0030 D4). This is what the audit fact records beside
/// the decision and what the Managed `evaluation` object is projected from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionBasis {
    pub policy: PermissionPolicy,
    /// Present only for an `auto` judgement that did not allow.
    pub reason_code: Option<PermissionReasonCode>,
}

impl PermissionBasis {
    /// A fixed authored answer: the policy stated it, no judgement was made.
    #[must_use]
    pub const fn fixed(policy: PermissionPolicy) -> Self {
        Self {
            policy,
            reason_code: None,
        }
    }

    /// An `auto` judgement; `reason_code` is `None` exactly when it allowed.
    #[must_use]
    pub const fn auto(reason_code: Option<PermissionReasonCode>) -> Self {
        Self {
            policy: PermissionPolicy::Auto,
            reason_code,
        }
    }
}

/// A policy's verdict together with the basis it was reached on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPermissionRuling {
    pub verdict: ToolPermissionVerdict,
    /// `None` when no authored permission policy decided (see
    /// [`PermissionPolicy`]).
    pub basis: Option<PermissionBasis>,
}

impl ToolPermissionRuling {
    /// A verdict reached outside the authored policy vocabulary.
    #[must_use]
    pub const fn without_basis(verdict: ToolPermissionVerdict) -> Self {
        Self {
            verdict,
            basis: None,
        }
    }

    /// Project onto the gate exactly once, carrying the basis unchanged.
    #[must_use]
    pub fn into_gate_decision(self) -> GateDecision {
        GateDecision {
            outcome: self.verdict.into_gate_outcome(),
            basis: self.basis,
        }
    }
}

/// What the gate chain decided for one call and on what basis. The outcome
/// alone drives execution; the basis rides into the audit fact.
#[derive(Debug, Clone, PartialEq)]
pub struct GateDecision {
    pub outcome: GateOutcome,
    /// The authored policy that produced `outcome`, when one did.
    pub basis: Option<PermissionBasis>,
}

impl GateDecision {
    /// An outcome reached outside the authored policy vocabulary (a per-Run
    /// narrowing, a plugin gate, an absent gate).
    #[must_use]
    pub const fn without_basis(outcome: GateOutcome) -> Self {
        Self {
            outcome,
            basis: None,
        }
    }
}

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

    /// The verdict together with the authored policy that produced it.
    ///
    /// The default states no basis: a policy that does not decide from the
    /// authored vocabulary (a per-Run narrowing, a fixed host policy) has none
    /// to name, and inventing one here would put a policy label on a decision
    /// nobody authored. A policy that does decide from that vocabulary
    /// overrides this and must return the verdict `evaluate` would.
    async fn evaluate_with_basis(&self, call: &ToolCall) -> ToolPermissionRuling {
        ToolPermissionRuling::without_basis(self.evaluate(call).await)
    }
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

    /// The outcome together with the authored policy that produced it. The
    /// default states no basis, which is exact for every gate that is not
    /// backed by an authored permission policy (a plugin precondition gate
    /// decides on state, not on policy). The permission-backed gate overrides
    /// this and must return the outcome `gate` would.
    async fn gate_with_basis(&self, call: &ToolCall, state: &Store) -> GateDecision {
        GateDecision::without_basis(self.gate(call, state).await)
    }
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

    #[test]
    fn a_ruling_carries_its_basis_onto_the_gate_decision_unchanged() {
        // | Rule | verdict | basis | gate outcome | basis after |
        // |---|---|---|---|---|
        // | B1 | Allow | always_allow | Allow | always_allow |
        // | B2 | Ask | auto/indeterminate | RequireConfirmation | auto/indeterminate |
        // | B3 | Deny | none | Block | none |
        let b1 = ToolPermissionRuling {
            verdict: ToolPermissionVerdict::Allow,
            basis: Some(PermissionBasis::fixed(PermissionPolicy::AlwaysAllow)),
        }
        .into_gate_decision();
        assert_eq!(b1.outcome, GateOutcome::Allow, "B1");
        assert_eq!(
            b1.basis,
            Some(PermissionBasis::fixed(PermissionPolicy::AlwaysAllow)),
            "B1"
        );

        let b2 = ToolPermissionRuling {
            verdict: ToolPermissionVerdict::RequireConfirmation {
                correlation_id: "perm-1".into(),
            },
            basis: Some(PermissionBasis::auto(Some(
                PermissionReasonCode::Indeterminate,
            ))),
        }
        .into_gate_decision();
        assert_eq!(
            b2.outcome,
            GateOutcome::RequireConfirmation {
                correlation_id: "perm-1".into()
            },
            "B2"
        );
        assert_eq!(
            b2.basis,
            Some(PermissionBasis {
                policy: PermissionPolicy::Auto,
                reason_code: Some(PermissionReasonCode::Indeterminate),
            }),
            "B2"
        );

        let b3 = ToolPermissionRuling::without_basis(ToolPermissionVerdict::Deny {
            reason: "no".into(),
        })
        .into_gate_decision();
        assert_eq!(
            b3.outcome,
            GateOutcome::Block {
                reason: "no".into()
            },
            "B3"
        );
        assert_eq!(b3.basis, None, "B3");
    }

    #[tokio::test]
    async fn a_policy_that_states_no_basis_still_reaches_the_gate_with_its_verdict() {
        // The trait defaults are exact, not lossy: a policy or gate that only
        // implements the verdict surface reports its verdict and no basis.
        struct FixedDeny;
        #[async_trait]
        impl ToolPermissionPolicy for FixedDeny {
            async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionVerdict {
                ToolPermissionVerdict::Deny {
                    reason: "fixed".into(),
                }
            }
        }
        struct StateGate;
        #[async_trait]
        impl ToolGateHook for StateGate {
            async fn gate(&self, _call: &ToolCall, _state: &Store) -> GateOutcome {
                GateOutcome::SetResult(ToolOutput::ok("c1", "cached"))
            }
        }
        let call = ToolCall {
            tool_id: "t".into(),
            call_id: "c1".into(),
            arguments: serde_json::json!({}),
        };
        let ruling = FixedDeny.evaluate_with_basis(&call).await;
        assert_eq!(
            ruling,
            ToolPermissionRuling::without_basis(ToolPermissionVerdict::Deny {
                reason: "fixed".into()
            })
        );
        let decision = StateGate.gate_with_basis(&call, &Store::default()).await;
        assert_eq!(decision.basis, None);
        assert_eq!(decision.outcome.decision_label(), "set_result");
    }
}
