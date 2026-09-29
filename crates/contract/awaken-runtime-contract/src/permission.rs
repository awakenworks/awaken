// @formal entrypoint permission.runtime_capability_and_verdict_projection_never_widens_authority
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
pub use awaken_agent_contract::audit::permission::{
    PermissionDecider, PermissionPolicy, PermissionReasonCode,
};
use awaken_agent_contract::audit::run_event::RunEvent;

/// The authored policy a permission verdict came from, with the grounds of an
/// `auto` judgement (ADR-0030 D4). This is what the audit fact records beside
/// the decision and what the Managed `evaluation` object is projected from.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Who reached a [`ToolPermissionRuling`]: the authored policy, or a per-Run
/// narrowing of the configured authority. A [`ToolPermissionPolicy`] is one of
/// these two and nothing else, so a ruling cannot claim to be a plugin gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulingAuthority {
    /// The authored permission policy decided, on this basis.
    Policy(PermissionBasis),
    /// A per-Run narrowing decided (e.g. [`DenyAllTools`]).
    RunNarrowing,
}

impl From<RulingAuthority> for GateAuthority {
    fn from(authority: RulingAuthority) -> Self {
        match authority {
            RulingAuthority::Policy(basis) => Self::Policy(basis),
            RulingAuthority::RunNarrowing => Self::RunNarrowing,
        }
    }
}

/// A policy's verdict together with who reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPermissionRuling {
    pub verdict: ToolPermissionVerdict,
    pub authority: RulingAuthority,
}

impl ToolPermissionRuling {
    /// A verdict the authored policy reached on `basis`.
    #[must_use]
    pub const fn policy(verdict: ToolPermissionVerdict, basis: PermissionBasis) -> Self {
        Self {
            verdict,
            authority: RulingAuthority::Policy(basis),
        }
    }

    /// A verdict a per-Run narrowing reached.
    #[must_use]
    pub const fn run_narrowing(verdict: ToolPermissionVerdict) -> Self {
        Self {
            verdict,
            authority: RulingAuthority::RunNarrowing,
        }
    }

    /// Project onto the gate exactly once, carrying the authority unchanged.
    #[must_use]
    pub fn into_gate_decision(self) -> GateDecision {
        GateDecision {
            outcome: self.verdict.into_gate_outcome(),
            authority: self.authority.into(),
        }
    }
}

/// Who decided a gate outcome (ADR-0030 D4). Closed: every gate decision names
/// exactly one, so the audit fact can always tell an authored policy, a per-Run
/// narrowing and a plugin gate apart instead of folding the latter two into an
/// absent policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAuthority {
    /// The authored permission policy decided, on this basis. A disabled tool
    /// or a matched deny rule is still the authored `rule` policy.
    Policy(PermissionBasis),
    /// A per-Run narrowing of the configured tool authority decided.
    RunNarrowing,
    /// A [`ToolGateHook`] that is not an authored permission policy decided
    /// (a plugin precondition, the skill `allowed_tools` intersection).
    PluginGate,
}

impl GateAuthority {
    /// The closed audit label naming this decider.
    #[must_use]
    pub const fn decider(&self) -> PermissionDecider {
        match self {
            Self::Policy(_) => PermissionDecider::Policy,
            Self::RunNarrowing => PermissionDecider::RunNarrowing,
            Self::PluginGate => PermissionDecider::PluginGate,
        }
    }

    /// The authored basis, present exactly for a policy decision.
    #[must_use]
    pub const fn basis(&self) -> Option<&PermissionBasis> {
        match self {
            Self::Policy(basis) => Some(basis),
            Self::RunNarrowing | Self::PluginGate => None,
        }
    }

    /// The one `PermissionDecided` audit fact for `call`, decided as
    /// `decision` by this authority. Every producer (the native gate and the
    /// ACP permission bridge) lowers through here, so `decided_by`, `policy`
    /// and `reason_code` are derived from one value and cannot disagree.
    #[must_use]
    pub fn permission_decided(&self, call: &ToolCall, decision: GateAuditDecision) -> RunEvent {
        let basis = self.basis();
        RunEvent::PermissionDecided {
            tool_id: call.tool_id.clone(),
            call_id: call.call_id.clone(),
            decision: decision.as_str().to_string(),
            decided_by: self.decider(),
            policy: basis.map(|basis| basis.policy),
            reason_code: basis.and_then(|basis| basis.reason_code.clone()),
        }
    }
}

/// What the gate chain decided for one call and who decided it. The outcome
/// alone drives execution; the authority rides into the audit fact.
#[derive(Debug, Clone, PartialEq)]
pub struct GateDecision {
    pub outcome: GateOutcome,
    pub authority: GateAuthority,
}

impl GateDecision {
    /// An outcome the authored policy reached on `basis`.
    #[must_use]
    pub const fn policy(outcome: GateOutcome, basis: PermissionBasis) -> Self {
        Self {
            outcome,
            authority: GateAuthority::Policy(basis),
        }
    }

    /// An outcome a per-Run narrowing reached.
    #[must_use]
    pub const fn run_narrowing(outcome: GateOutcome) -> Self {
        Self {
            outcome,
            authority: GateAuthority::RunNarrowing,
        }
    }

    /// An outcome a gate that is not an authored permission policy reached.
    #[must_use]
    pub const fn plugin_gate(outcome: GateOutcome) -> Self {
        Self {
            outcome,
            authority: GateAuthority::PluginGate,
        }
    }

    /// The `PermissionDecided` audit fact for this decision on `call`.
    #[must_use]
    pub fn audit_fact(&self, call: &ToolCall) -> RunEvent {
        self.authority
            .permission_decided(call, self.outcome.kind().audit_decision())
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
    /// Trusted implementation containment. Default/custom policies may perform
    /// external effects; a fixed local policy may explicitly close this scope.
    fn process_bound(&self) -> bool {
        false
    }
    /// The verdict together with the authored policy that produced it.
    ///
    /// One method, so a verdict and who reached it cannot be answered
    /// separately and disagree. There is no unattributed ruling: an authored
    /// policy names its basis with [`ToolPermissionRuling::policy`], and a
    /// per-Run narrowing says so by name with
    /// [`ToolPermissionRuling::run_narrowing`].
    async fn evaluate(&self, call: &ToolCall) -> ToolPermissionRuling;
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
    /// Configured authority further constrained by the activation's exact
    /// inherited extension inputs. Older decoders reject this variant rather
    /// than silently ignore those inputs on an unregistered local executor.
    ConfiguredWithInheritedToolsets,
    DenyAll,
}

impl ToolCapabilityNarrowing {
    #[must_use]
    pub fn is_configured(&self) -> bool {
        matches!(
            self,
            Self::Configured | Self::ConfiguredWithInheritedToolsets
        )
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::Configured
    }

    /// Intersect two per-Run restrictions. `DenyAll` is absorbing, so
    /// composition can preserve or remove configured authority, never add it.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::DenyAll, _) | (_, Self::DenyAll) => Self::DenyAll,
            (Self::Configured, Self::Configured) => Self::Configured,
            _ => Self::ConfiguredWithInheritedToolsets,
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
    fn process_bound(&self) -> bool {
        true
    }
    async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionRuling {
        ToolPermissionRuling::run_narrowing(ToolPermissionVerdict::Deny {
            reason: self.reason.clone(),
        })
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

    /// @obligation permission.runtime_capability_and_verdict_projection_never_widens_authority
    #[kani::proof]
    fn tool_capability_intersection_never_widens_configured_authority() {
        // Cause/effect table: configured, inherited configured, and deny-all
        // intersect in all nine combinations. Deny is absorbing; inherited
        // format survives either configured operand; no combination grants
        // authority removed by either operand.
        let restriction = |tag: u8| match tag % 3 {
            0 => ToolCapabilityNarrowing::Configured,
            1 => ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets,
            _ => ToolCapabilityNarrowing::DenyAll,
        };
        let left = restriction(kani::any());
        let right = restriction(kani::any());
        let effective = left.intersect(right);

        assert_eq!(
            effective.is_configured(),
            left.is_configured() && right.is_configured()
        );
        assert_eq!(effective, right.intersect(left));
        if effective.is_configured()
            && (left == ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets
                || right == ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets)
        {
            assert_eq!(
                effective,
                ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets
            );
        }
    }

    /// @obligation permission.runtime_capability_and_verdict_projection_never_widens_authority
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

    /// @obligation permission.runtime_capability_and_verdict_projection_never_widens_authority
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
    fn process_bound(&self) -> bool {
        false
    }
    /// Stable id, used to bound a plugin-contributed gate under its
    /// `CapabilityBound` (G30). A host-wired gate that is not a plugin
    /// contribution keeps the default.
    fn id(&self) -> &str {
        "gate"
    }

    /// Decide one tool call against the run's read-only state, together with the
    /// authored policy that produced the outcome. Most gates ignore `state`; a
    /// state machine gate reads it to enforce a precondition.
    ///
    /// One method, so an outcome and who decided it cannot be answered
    /// separately and disagree. A gate that is not backed by an authored
    /// permission policy — a plugin precondition gate decides on state, not on
    /// policy — says so by name with [`GateDecision::plugin_gate`].
    async fn gate(&self, call: &ToolCall, state: &Store) -> GateDecision;
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
    fn a_ruling_carries_its_authority_onto_the_gate_decision_unchanged() {
        // | Rule | verdict | ruling authority | gate outcome | gate authority |
        // |---|---|---|---|---|
        // | B1 | Allow | policy always_allow | Allow | Policy(always_allow) |
        // | B2 | Ask | policy auto/indeterminate | RequireConfirmation | Policy(auto/indeterminate) |
        // | B3 | Deny | run narrowing | Block | RunNarrowing |
        let b1 = ToolPermissionRuling::policy(
            ToolPermissionVerdict::Allow,
            PermissionBasis::fixed(PermissionPolicy::AlwaysAllow),
        )
        .into_gate_decision();
        assert_eq!(b1.outcome, GateOutcome::Allow, "B1");
        assert_eq!(
            b1.authority,
            GateAuthority::Policy(PermissionBasis::fixed(PermissionPolicy::AlwaysAllow)),
            "B1"
        );

        let b2 = ToolPermissionRuling::policy(
            ToolPermissionVerdict::RequireConfirmation {
                correlation_id: "perm-1".into(),
            },
            PermissionBasis::auto(Some(PermissionReasonCode::INDETERMINATE)),
        )
        .into_gate_decision();
        assert_eq!(
            b2.outcome,
            GateOutcome::RequireConfirmation {
                correlation_id: "perm-1".into()
            },
            "B2"
        );
        assert_eq!(
            b2.authority,
            GateAuthority::Policy(PermissionBasis {
                policy: PermissionPolicy::Auto,
                reason_code: Some(PermissionReasonCode::INDETERMINATE),
            }),
            "B2"
        );

        let b3 = ToolPermissionRuling::run_narrowing(ToolPermissionVerdict::Deny {
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
        assert_eq!(b3.authority, GateAuthority::RunNarrowing, "B3");
    }

    #[test]
    fn every_gate_authority_lowers_to_one_exact_audit_fact() {
        // Cause/effect graph: C1 who decided (policy / run narrowing / plugin
        // gate); C2 the policy basis carries an `auto` reason. Effects: E1
        // `decided_by` names C1 exactly; E2 `policy` is present iff C1=policy;
        // E3 `reason_code` is present iff C1=policy and C2.
        //
        // | Rule | authority | decision | decided_by | policy | reason_code |
        // |---|---|---|---|---|---|
        // | A1 | Policy(always_allow) | allow | policy | always_allow | - |
        // | A2 | Policy(auto/high_risk) | deny | policy | auto | high_risk |
        // | A3 | Policy(rule) (disabled tool / deny rule) | deny | policy | rule | - |
        // | A4 | RunNarrowing | deny | run_narrowing | - | - |
        // | A5 | PluginGate | set_result | plugin_gate | - | - |
        let call = ToolCall {
            tool_id: "bash".into(),
            call_id: "c1".into(),
            arguments: serde_json::json!({}),
        };
        let block = || GateOutcome::Block {
            reason: "no".into(),
        };
        let rows = [
            (
                "A1",
                GateDecision::policy(
                    GateOutcome::Allow,
                    PermissionBasis::fixed(PermissionPolicy::AlwaysAllow),
                ),
                serde_json::json!({"decision": "allow", "decided_by": "policy", "policy": "always_allow"}),
            ),
            (
                "A2",
                GateDecision::policy(
                    block(),
                    PermissionBasis::auto(Some(PermissionReasonCode::HIGH_RISK)),
                ),
                serde_json::json!({"decision": "deny", "decided_by": "policy", "policy": "auto", "reason_code": "high_risk"}),
            ),
            (
                "A3",
                GateDecision::policy(block(), PermissionBasis::fixed(PermissionPolicy::Rule)),
                serde_json::json!({"decision": "deny", "decided_by": "policy", "policy": "rule"}),
            ),
            (
                "A4",
                GateDecision::run_narrowing(block()),
                serde_json::json!({"decision": "deny", "decided_by": "run_narrowing"}),
            ),
            (
                "A5",
                GateDecision::plugin_gate(GateOutcome::SetResult(ToolOutput::ok("c1", "cached"))),
                serde_json::json!({"decision": "set_result", "decided_by": "plugin_gate"}),
            ),
        ];
        for (rule, decision, expected) in rows {
            let draft =
                awaken_agent_contract::audit::draft::Draft::from(decision.audit_fact(&call));
            let mut expected = expected;
            expected["tool_id"] = "bash".into();
            expected["call_id"] = "c1".into();
            assert_eq!(draft.payload, expected, "{rule}");
        }
    }

    #[tokio::test]
    async fn a_narrowing_and_a_plugin_gate_name_themselves_through_the_ports() {
        // The ports carry the authority the implementation stated, unchanged.
        struct FixedDeny;
        #[async_trait]
        impl ToolPermissionPolicy for FixedDeny {
            async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionRuling {
                ToolPermissionRuling::run_narrowing(ToolPermissionVerdict::Deny {
                    reason: "fixed".into(),
                })
            }
        }
        struct StateGate;
        #[async_trait]
        impl ToolGateHook for StateGate {
            async fn gate(&self, _call: &ToolCall, _state: &Store) -> GateDecision {
                GateDecision::plugin_gate(GateOutcome::SetResult(ToolOutput::ok("c1", "cached")))
            }
        }
        let call = ToolCall {
            tool_id: "t".into(),
            call_id: "c1".into(),
            arguments: serde_json::json!({}),
        };
        let ruling = FixedDeny.evaluate(&call).await;
        assert_eq!(
            ruling,
            ToolPermissionRuling::run_narrowing(ToolPermissionVerdict::Deny {
                reason: "fixed".into()
            })
        );
        let decision = StateGate.gate(&call, &Store::default()).await;
        assert_eq!(decision.authority, GateAuthority::PluginGate);
        assert_eq!(decision.outcome.decision_label(), "set_result");
        assert_eq!(
            DenyAllTools::new("r").evaluate(&call).await.authority,
            RulingAuthority::RunNarrowing
        );
    }
}
