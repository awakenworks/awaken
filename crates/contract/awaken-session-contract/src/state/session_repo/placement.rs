// @formal entrypoint runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal entrypoint session_creation.work_projection_is_disposable_repairable_and_non_authoritative
// @formal entrypoint session_environment.owner_topology_is_exact_and_frozen
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
//! The one placement classification of a frozen Session and the public status
//! it implies.
//!
//! Consumers: the Managed public status projection, Managed create-time layout
//! checks, and the Session application's Environment-realization owner and
//! WorkQueue projection decisions. Each used to classify the raw
//! `(runtime_placement, environment.self_hosted)` pair itself, with different
//! inputs (one ignored `self_hosted`, one ignored the runtime placement), so a
//! new placement could be read three different ways.

use super::*;
use awaken_session_types::SessionRuntimePlacement;

/// Who executes one frozen Session and who realizes its Environment, derived
/// once from the two frozen facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionPlacementClass {
    runtime: SessionRuntimePlacement,
    self_hosted_environment: bool,
}

impl SessionPlacementClass {
    #[must_use]
    pub const fn new(runtime: SessionRuntimePlacement, self_hosted_environment: bool) -> Self {
        Self {
            runtime,
            self_hosted_environment,
        }
    }

    /// Environment realization belongs to an external claimed Worker rather
    /// than this Coordinator process.
    #[must_use]
    pub const fn requires_external_environment_owner(self) -> bool {
        matches!(self.runtime, SessionRuntimePlacement::Worker)
    }

    /// A self-hosted Environment is served through the disposable WorkQueue
    /// projection, independent of where the Session Runtime runs.
    #[must_use]
    pub const fn requires_work_projection(self) -> bool {
        self.self_hosted_environment
    }

    /// The Coordinator itself realizes a cloud Environment, so the durable root
    /// is complete at acceptance and its activity epochs, not Sandbox
    /// readiness, decide the logical status while it prepares.
    #[must_use]
    pub const fn realized_by_coordinator(self) -> bool {
        matches!(self.runtime, SessionRuntimePlacement::Local) && !self.self_hosted_environment
    }

    /// Whether this placement can honour an idle-retention policy. An
    /// external Worker has one realization/terminal-cleanup claim path but no
    /// claim-fenced nonterminal quiesce, checkpoint, disposal or restore
    /// transport, so checkpoint-and-release retention would require
    /// Coordinator-local continuation effects against a Worker-owned
    /// Environment.
    #[must_use]
    pub fn supports_idle_retention(
        self,
        idle_retention: &crate::state::EnvironmentIdleRetentionPolicy,
    ) -> bool {
        !(self.requires_external_environment_owner()
            && idle_retention.mode
                == crate::state::EnvironmentIdleRetentionMode::CheckpointAndRelease)
    }
}

/// Neutral public execution status of one Session root. Protocol adapters map
/// it onto their wire vocabulary; none re-derives it from the raw root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPublicStatus {
    Idle,
    Running,
    Rescheduling,
    Terminated,
}

/// The status law over already-classified facts. A preparing Session reports a
/// logical Idle/Running only when the Coordinator realizes it; every other
/// placement keeps the conservative pre-realization Rescheduling boundary.
#[must_use]
const fn session_public_status(
    execution: SessionExecutionState,
    realized_by_coordinator: bool,
    has_active_activity: bool,
) -> SessionPublicStatus {
    match execution {
        SessionExecutionState::Preparing | SessionExecutionState::Activating => {
            if !realized_by_coordinator {
                SessionPublicStatus::Rescheduling
            } else if has_active_activity {
                SessionPublicStatus::Running
            } else {
                SessionPublicStatus::Idle
            }
        }
        SessionExecutionState::ActivationFailed | SessionExecutionState::Terminated => {
            SessionPublicStatus::Terminated
        }
        SessionExecutionState::Running => SessionPublicStatus::Running,
        SessionExecutionState::Rescheduling => SessionPublicStatus::Rescheduling,
        SessionExecutionState::Idle => SessionPublicStatus::Idle,
    }
}

/// Whether the disposable WorkQueue projection must be present. Absence of a
/// frozen baseline, Coordinator-realized Environments, and terminal cleanup all
/// stutter; only a frozen nonterminal self-hosted Session is projected.
#[must_use]
const fn session_work_projection_required(
    placement: Option<SessionPlacementClass>,
    terminal: bool,
) -> bool {
    match placement {
        Some(placement) => placement.requires_work_projection() && !terminal,
        None => false,
    }
}

/// The budget-pause law: the shared cap admits no model request and nothing
/// executes.
const fn budget_paused(
    budget_admits_model_request: bool,
    execution: SessionExecutionState,
) -> bool {
    !budget_admits_model_request && matches!(execution, SessionExecutionState::Idle)
}

/// The shared budget cap is reached while no execution is in flight: the
/// Session is paused by its budget, so a pure interrupt has nothing to stop.
#[must_use]
pub fn session_is_budget_paused(session: &PersistedSession) -> bool {
    budget_paused(session.budget.can_admit_model_request(), session.execution)
}

impl PersistedSession {
    /// The frozen placement class; `None` while the baseline is still being
    /// prepared.
    #[must_use]
    pub fn placement_class(&self) -> Option<SessionPlacementClass> {
        self.frozen_baseline().map(|baseline| {
            SessionPlacementClass::new(baseline.runtime_placement, baseline.environment.self_hosted)
        })
    }

    /// The one public status of this root.
    #[must_use]
    pub fn public_status(&self) -> SessionPublicStatus {
        session_public_status(
            self.execution,
            self.placement_class()
                .is_some_and(SessionPlacementClass::realized_by_coordinator),
            !self.active_activity_epochs.is_empty(),
        )
    }

    /// Whether this root requires its disposable WorkQueue projection.
    #[must_use]
    pub fn requires_work_projection(&self) -> bool {
        session_work_projection_required(self.placement_class(), self.is_terminal())
    }

    /// Whether an external claimed Worker realizes this root's Environment.
    #[must_use]
    pub fn requires_external_environment_owner(&self) -> bool {
        self.placement_class()
            .is_some_and(SessionPlacementClass::requires_external_environment_owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_pause_requires_a_reached_cap_and_idle_execution() {
        // Cause/effect graph: C1 the shared budget admits no model request;
        // C2 execution is Idle. E1 paused only when C1 and C2 hold.
        // | Rule | C1 | C2 | Paused |
        // |---|---|---|---|
        // | B1 | yes | yes | yes |
        // | B2 | yes | no (every other state) | no |
        // | B3 | no | any | no |
        for execution in [
            SessionExecutionState::Preparing,
            SessionExecutionState::Activating,
            SessionExecutionState::ActivationFailed,
            SessionExecutionState::Running,
            SessionExecutionState::Rescheduling,
            SessionExecutionState::Idle,
            SessionExecutionState::Terminated,
        ] {
            assert_eq!(
                budget_paused(false, execution),
                execution == SessionExecutionState::Idle,
                "B1/B2 {execution:?}"
            );
            assert!(!budget_paused(true, execution), "B3 {execution:?}");
        }
    }

    #[test]
    fn placement_class_answers_every_consumer_from_the_same_frozen_pair() {
        // Causes: C1 runtime placement Local/Worker; C2 self-hosted Environment.
        // Effects: E1 external Environment owner; E2 WorkQueue projection;
        // E3 Coordinator-realized (logical status while preparing).
        // | Rule | C1 | C2 | E1 | E2 | E3 |
        // |---|---|---|---|---|---|
        // | P1 | Local | no | no | no | yes |
        // | P2 | Local | yes | no | yes | no |
        // | P3 | Worker | no | yes | no | no |
        // | P4 | Worker | yes | yes | yes | no |
        for (rule, runtime, self_hosted, external, work, coordinator) in [
            (
                "P1",
                SessionRuntimePlacement::Local,
                false,
                false,
                false,
                true,
            ),
            (
                "P2",
                SessionRuntimePlacement::Local,
                true,
                false,
                true,
                false,
            ),
            (
                "P3",
                SessionRuntimePlacement::Worker,
                false,
                true,
                false,
                false,
            ),
            (
                "P4",
                SessionRuntimePlacement::Worker,
                true,
                true,
                true,
                false,
            ),
        ] {
            let class = SessionPlacementClass::new(runtime, self_hosted);
            assert_eq!(
                class.requires_external_environment_owner(),
                external,
                "{rule}/E1"
            );
            assert_eq!(class.requires_work_projection(), work, "{rule}/E2");
            assert_eq!(class.realized_by_coordinator(), coordinator, "{rule}/E3");
        }
    }

    #[test]
    fn public_status_is_one_law_over_execution_placement_and_activity() {
        // Causes: C1 durable execution state; C2 Coordinator-realized placement;
        // C3 an admitted activity epoch is open. Effects: the public status.
        // | Rule | C1 | C2 | C3 | Status |
        // |---|---|---|---|---|
        // | S1 | Preparing/Activating | no | any | Rescheduling |
        // | S2 | Preparing/Activating | yes | no | Idle |
        // | S3 | Preparing/Activating | yes | yes | Running |
        // | S4 | Running/Rescheduling/Idle | any | any | same state |
        // | S5 | ActivationFailed/Terminated | any | any | Terminated |
        use SessionExecutionState as E;
        use SessionPublicStatus as P;
        for coordinator in [false, true] {
            for active in [false, true] {
                for execution in [E::Preparing, E::Activating] {
                    let expected = match (coordinator, active) {
                        (false, _) => P::Rescheduling,
                        (true, false) => P::Idle,
                        (true, true) => P::Running,
                    };
                    assert_eq!(
                        session_public_status(execution, coordinator, active),
                        expected,
                        "S1-S3 {execution:?} coordinator={coordinator} active={active}"
                    );
                }
                for (execution, expected) in [
                    (E::Running, P::Running),
                    (E::Rescheduling, P::Rescheduling),
                    (E::Idle, P::Idle),
                    (E::ActivationFailed, P::Terminated),
                    (E::Terminated, P::Terminated),
                ] {
                    assert_eq!(
                        session_public_status(execution, coordinator, active),
                        expected,
                        "S4-S5 {execution:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn work_projection_requires_a_frozen_nonterminal_self_hosted_root() {
        // | Rule | Placement | Terminal | Projected |
        // |---|---|---|---|
        // | W1 | not frozen | any | no |
        // | W2 | frozen, not self-hosted | any | no |
        // | W3 | frozen, self-hosted | no | yes |
        // | W4 | frozen, self-hosted | yes | no |
        for terminal in [false, true] {
            assert!(!session_work_projection_required(None, terminal), "W1");
            for runtime in [
                SessionRuntimePlacement::Local,
                SessionRuntimePlacement::Worker,
            ] {
                assert!(
                    !session_work_projection_required(
                        Some(SessionPlacementClass::new(runtime, false)),
                        terminal
                    ),
                    "W2"
                );
                assert_eq!(
                    session_work_projection_required(
                        Some(SessionPlacementClass::new(runtime, true)),
                        terminal
                    ),
                    !terminal,
                    "W3/W4"
                );
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn any_execution() -> SessionExecutionState {
        match kani::any::<u8>() % 7 {
            0 => SessionExecutionState::Preparing,
            1 => SessionExecutionState::Activating,
            2 => SessionExecutionState::ActivationFailed,
            3 => SessionExecutionState::Running,
            4 => SessionExecutionState::Rescheduling,
            5 => SessionExecutionState::Idle,
            _ => SessionExecutionState::Terminated,
        }
    }

    fn any_runtime() -> SessionRuntimePlacement {
        if kani::any() {
            SessionRuntimePlacement::Worker
        } else {
            SessionRuntimePlacement::Local
        }
    }

    /// @obligation session_creation.work_projection_is_disposable_repairable_and_non_authoritative
    /// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
    #[kani::proof]
    fn work_projection_is_derived_only_from_durable_session_truth() {
        // Cause/effect table: C1 root has no frozen baseline / local / external
        // Environment; C2 root execution and retention truth. E1 enqueue iff
        // self-hosted + nonterminal; E2 every other row stutters. Terminality is
        // derived by the production aggregate law, not taken as a free bit.
        let placement = match kani::any::<u8>() % 3 {
            0 => None,
            1 => Some(SessionPlacementClass::new(any_runtime(), false)),
            _ => Some(SessionPlacementClass::new(any_runtime(), true)),
        };
        let execution = any_execution();
        let disposition = if kani::any() {
            SessionDisposition::Active
        } else {
            SessionDisposition::Deleted
        };
        let terminal = session_is_terminal(execution, &disposition);
        assert_eq!(
            session_work_projection_required(placement, terminal),
            placement.is_some_and(SessionPlacementClass::requires_work_projection) && !terminal
        );
    }
}
