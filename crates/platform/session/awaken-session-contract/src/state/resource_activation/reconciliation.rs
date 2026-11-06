// @formal entrypoint session_resources.terminal_session_never_reactivates
//! The Session Resource reconciliation decision.
//!
//! [`next_resource_reconciliation`] is the one pure decision over the Resource
//! sub-state, beside the Environment's `transition_environment`: given the
//! aggregate's [`SessionResourceReconciliationFacts`] it returns the closed
//! [`SessionResourceReconciliationInstruction`] the Session application carries
//! out. It reads no clock and performs no I/O. The application's stack-saving
//! early return and its complete activation/retirement path consume the same
//! value, so the early return cannot select a branch the full path would not.

use super::ActivationState;
use crate::state::{PersistedSession, SessionExecutionState};

/// Every aggregate fact the decision reads. Only [`Self::project`] builds it
/// outside this module, so no caller can invent a Resource or lifecycle fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionResourceReconciliationFacts {
    /// [`PersistedSession::is_terminal`].
    pub(crate) terminal: bool,
    /// [`PersistedSession::requires_external_environment_owner`]: a claimed
    /// Worker, not this process, realizes the Environment and its Resources.
    pub(crate) external_environment: bool,
    /// Execution is Idle: no Run owns the Session's Resources.
    pub(crate) execution_idle: bool,
    /// A desired replacement generation is pending.
    pub(crate) pending: bool,
    /// Removed Repository inputs still await participant retirement.
    pub(crate) repository_retirements: bool,
    /// An activation record is Releasing.
    pub(crate) releasing_activations: bool,
    /// The installed (active) manifest names at least one input.
    pub(crate) active_inputs: bool,
    /// At least one activation record exists.
    pub(crate) activation_records: bool,
}

impl SessionResourceReconciliationFacts {
    /// Fold the durable root into the facts the decision reads.
    #[must_use]
    pub(crate) fn project(session: &PersistedSession) -> Self {
        let resources = &session.resources;
        Self {
            terminal: session.is_terminal(),
            external_environment: session.requires_external_environment_owner(),
            execution_idle: session.execution == SessionExecutionState::Idle,
            pending: resources.pending.is_some(),
            repository_retirements: resources.has_repository_retirements(),
            releasing_activations: resources
                .activations
                .iter()
                .any(|activation| activation.state == ActivationState::Releasing),
            active_inputs: !resources.active.inputs().is_empty(),
            activation_records: !resources.activations.is_empty(),
        }
    }
}

/// What Resource reconciliation must do next for one Session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionResourceReconciliationInstruction {
    /// No Resource effect is due: a Run owns execution (Resource changes stay
    /// deferred), a pending generation waits for its external Worker, or
    /// nothing is installed and nothing awaits retirement. The root is
    /// returned unchanged.
    Hold,
    /// Retire the Repository participants a detached generation left behind.
    /// An external Worker realizes this live Session, so no local Runtime
    /// effect runs.
    RetireDetachedGeneration,
    /// Ordinary Runtime apply on a live, Idle, Coordinator-realized Session.
    /// Every step requires this process's realization-effects role.
    ApplyLocally(SessionResourceLocalApply),
    /// A terminal Session: its Resources belong to terminal cleanup and never
    /// reenter ordinary activation.
    ReleaseTerminal,
}

/// The ordinary local Runtime apply step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionResourceLocalApply {
    /// Install the pending generation, settle it, then retire what it detached.
    InstallPending,
    /// Re-apply the installed generation, then retire. `adopt_legacy`: the
    /// installed manifest has no activation records and is adopted once it has
    /// been re-realized.
    ReapplyActive { adopt_legacy: bool },
    /// Nothing is installed; only Repository retirements remain.
    RetireDetachedGeneration,
    /// Releasing activation records without a pending manifest: an
    /// inconsistent root the apply refuses.
    RefuseReleasingWithoutPending,
}

/// Decide the next Resource reconciliation step. Pure, total and exhaustively
/// tested.
#[must_use]
pub(crate) const fn next_resource_reconciliation(
    facts: SessionResourceReconciliationFacts,
) -> SessionResourceReconciliationInstruction {
    use SessionResourceLocalApply as Local;
    use SessionResourceReconciliationInstruction as I;
    if facts.terminal {
        return I::ReleaseTerminal;
    }
    if facts.external_environment {
        return if !facts.pending && facts.repository_retirements {
            I::RetireDetachedGeneration
        } else {
            I::Hold
        };
    }
    if !facts.execution_idle {
        return I::Hold;
    }
    let step = if facts.pending {
        Local::InstallPending
    } else if facts.releasing_activations {
        Local::RefuseReleasingWithoutPending
    } else if facts.active_inputs || facts.activation_records {
        Local::ReapplyActive {
            adopt_legacy: !facts.activation_records,
        }
    } else if facts.repository_retirements {
        Local::RetireDetachedGeneration
    } else {
        return I::Hold;
    };
    I::ApplyLocally(step)
}

impl SessionResourceReconciliationInstruction {
    /// Decide the next Resource reconciliation step for this root.
    #[must_use]
    pub fn decide(session: &PersistedSession) -> Self {
        next_resource_reconciliation(SessionResourceReconciliationFacts::project(session))
    }
}

#[cfg(kani)]
fn any_facts() -> SessionResourceReconciliationFacts {
    SessionResourceReconciliationFacts {
        terminal: kani::any(),
        external_environment: kani::any(),
        execution_idle: kani::any(),
        pending: kani::any(),
        repository_retirements: kani::any(),
        releasing_activations: kani::any(),
        active_inputs: kani::any(),
        activation_records: kani::any(),
    }
}

#[cfg(kani)]
/// @obligation session_resources.terminal_session_never_reactivates
#[kani::proof]
fn terminal_session_never_enters_ordinary_resource_reconciliation() {
    // Over every fact combination: a terminal root always reaches terminal
    // cleanup and never an ordinary apply; an ordinary apply is reached only
    // by a live, Idle, Coordinator-realized root; an external live root never
    // runs a local Runtime effect.
    let facts = any_facts();
    let instruction = next_resource_reconciliation(facts);
    let local = matches!(
        instruction,
        SessionResourceReconciliationInstruction::ApplyLocally(_)
    );
    if facts.terminal {
        assert!(matches!(
            instruction,
            SessionResourceReconciliationInstruction::ReleaseTerminal
        ));
    }
    if local {
        assert!(!facts.terminal && !facts.external_environment && facts.execution_idle);
    }
    if !facts.terminal {
        assert!(!matches!(
            instruction,
            SessionResourceReconciliationInstruction::ReleaseTerminal
        ));
    }
}

#[cfg(test)]
mod tests;
