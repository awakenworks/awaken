//! Human-approval gating for sensitive sandbox actions (an oversight facet). Neutral
//! and content-blind: it decides *whether* an action needs approval, keyed on the
//! action kind — not its data. The actual pause/HITL mailbox is the permission
//! plane's job (ext-permission's `ask`); the host maps a [`ApprovalDecision::RequireApproval`]
//! onto that existing suspension rather than inventing a second mechanism.

/// A run/environment's approval posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    /// No human gate — the agent acts autonomously.
    #[default]
    Autonomous,
    /// A human must approve a sensitive action before it proceeds.
    HumanApproval,
}

/// A sandbox action that may be gated (content-blind: the kind, never the payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxAction {
    /// Launching a process in the sandbox.
    Spawn,
    /// Egress to the network.
    Egress,
    /// Reading artifacts back out of the sandbox.
    Artifact,
}

/// The gate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Proceed without a human in the loop.
    Allow,
    /// Suspend and route to the human-approval mailbox before proceeding.
    RequireApproval,
}

/// Decide whether `action` needs human approval under `policy`. Under
/// `HumanApproval` the sensitive, outward-facing actions (launching a process,
/// egress) are gated; reading artifacts back is not. Under `Autonomous` everything
/// is allowed.
#[must_use]
pub fn decide(policy: ApprovalPolicy, action: SandboxAction) -> ApprovalDecision {
    match policy {
        ApprovalPolicy::Autonomous => ApprovalDecision::Allow,
        ApprovalPolicy::HumanApproval => match action {
            SandboxAction::Spawn | SandboxAction::Egress => ApprovalDecision::RequireApproval,
            SandboxAction::Artifact => ApprovalDecision::Allow,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomous_allows_every_action() {
        for action in [
            SandboxAction::Spawn,
            SandboxAction::Egress,
            SandboxAction::Artifact,
        ] {
            assert_eq!(
                decide(ApprovalPolicy::Autonomous, action),
                ApprovalDecision::Allow
            );
        }
    }

    #[test]
    fn human_approval_gates_the_outward_facing_actions() {
        assert_eq!(
            decide(ApprovalPolicy::HumanApproval, SandboxAction::Spawn),
            ApprovalDecision::RequireApproval
        );
        assert_eq!(
            decide(ApprovalPolicy::HumanApproval, SandboxAction::Egress),
            ApprovalDecision::RequireApproval
        );
    }

    #[test]
    fn human_approval_does_not_gate_reading_artifacts_back() {
        assert_eq!(
            decide(ApprovalPolicy::HumanApproval, SandboxAction::Artifact),
            ApprovalDecision::Allow
        );
    }

    #[test]
    fn the_default_policy_is_autonomous() {
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Autonomous);
    }
}
