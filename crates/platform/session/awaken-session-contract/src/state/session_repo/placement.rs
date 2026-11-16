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

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
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

/// The continuation Work law: a Worker-placed root whose Environment fences
/// Worker effects with Session Work (self-hosted) needs that Work to run a
/// continuation effect its registered owner executes (suspend, restore, or
/// expired-checkpoint deletion). Its last Run's settlement stopped the Work,
/// and only a durable root obligation may wake stopped Work: an admitted
/// event, or this Idle root's own committed continuation, which no Run will
/// drive. A Resident or Unmaterialized root, a driving Run (whose admission
/// already woke the Work), another placement and a terminal root wake
/// nothing. Callers ask only when the aggregate decided such an effect.
#[must_use]
const fn continuation_wakes_work(
    placement: Option<SessionPlacementClass>,
    terminal: bool,
    idle_without_activity: bool,
    continuation_step: bool,
) -> bool {
    match placement {
        Some(placement) => {
            placement.requires_external_environment_owner()
                && placement.requires_work_projection()
                && !terminal
                && idle_without_activity
                && continuation_step
        }
        None => false,
    }
}

/// The idle-source lease law. A self-hosted Worker placement renews its
/// realization lease under Session Work, and a settled Run stops that Work, so
/// the first renewal after it would retire the Worker's local source authority
/// and the later suspend could never capture that exact source generation.
/// While a checkpoint-and-release root still holds its source Sandbox
/// (a generated Resident binding, or any Suspending phase before Hibernated),
/// renewing the lease is not an Environment effect: it only keeps that one
/// generation alive for its own checkpoint. Every Environment effect still
/// requires the Work.
#[must_use]
const fn idle_source_lease_outlives_work(
    placement: Option<SessionPlacementClass>,
    terminal: bool,
    checkpoint_and_release: bool,
    source_retained: bool,
) -> bool {
    match placement {
        Some(placement) => {
            placement.requires_external_environment_owner()
                && placement.requires_work_projection()
                && !terminal
                && checkpoint_and_release
                && source_retained
        }
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

/// The budget-pause interrupt law (ADR-0094 D4): an Interrupt has nothing to
/// stop, and is absorbed without an Event, only when the root is budget paused,
/// no driving activity is open, and the committed root and child Thread
/// snapshots prove no live Run and stayed current through admission. Unknown
/// or moving Thread sources are not that proof.
#[must_use]
const fn budget_pause_absorbs_interrupt(
    budget_paused: bool,
    activity_open: bool,
    no_live_run_proven: bool,
) -> bool {
    budget_paused && !activity_open && no_live_run_proven
}

/// Whether an Interrupt to this root is absorbed by its budget pause.
/// `no_live_run_proven` is the Event application's Thread evidence: the
/// committed root and child snapshots show no live Run and did not move while
/// they were read.
#[must_use]
pub fn session_budget_pause_absorbs_interrupt(
    session: &PersistedSession,
    no_live_run_proven: bool,
) -> bool {
    budget_pause_absorbs_interrupt(
        session_is_budget_paused(session),
        !session.active_activity_epochs.is_empty(),
        no_live_run_proven,
    )
}

/// Whether any Thread evidence could still make an Interrupt to this root
/// absorbable. When false the application reads no Thread snapshots; the
/// repository rechecks it before recording an Event no-op.
#[must_use]
pub fn session_budget_pause_may_absorb_interrupt(session: &PersistedSession) -> bool {
    session_budget_pause_absorbs_interrupt(session, true)
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

/// Whether a decided continuation effect of this idle root wakes its stopped
/// Session Work, so its registered Worker may claim the continuation under the
/// Environment's Work fence ([`continuation_wakes_work`]).
#[must_use]
pub fn continuation_wakes_session_work(session: &PersistedSession) -> bool {
    continuation_wakes_work(
        session.placement_class(),
        session.is_terminal(),
        session.execution == SessionExecutionState::Idle && !session.has_active_activities(),
        !session.environment.admits_realization_effects(),
    )
}

/// Whether `session`'s realization lease may be renewed without its Session
/// Work ([`idle_source_lease_outlives_work`]): its source Sandbox must survive
/// the idle window to be checkpointed under that exact lease generation.
#[must_use]
pub fn idle_source_lease_outlives_session_work(session: &PersistedSession) -> bool {
    idle_source_lease_outlives_work(
        session.placement_class(),
        session.is_terminal(),
        session.frozen_baseline().is_some_and(|baseline| {
            baseline.environment.idle_retention.mode
                == crate::state::SandboxIdleRetentionMode::CheckpointAndRelease
        }),
        matches!(
            &session.environment,
            crate::state::SessionEnvironmentState::Resident {
                effect_id: Some(_),
                generation: Some(_),
                ..
            } | crate::state::SessionEnvironmentState::Suspending { .. }
        ),
    )
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
    fn budget_pause_absorbs_an_interrupt_only_with_an_inactive_root_and_no_live_run() {
        // Causes: C1 the shared budget admits no model request; C2 execution
        // is Idle; C3 a driving activity epoch is open; C4 the committed Thread
        // snapshots prove no live Run and stayed current. E1 the Interrupt is
        // absorbed (no Event); otherwise it is retained and reaches Runtime.
        // A pending tool reply implies an Awaiting Run, so it is a C4 = no row.
        // | Rule | C1 | C2 | C3 | C4 | Absorbed |
        // |---|---|---|---|---|---|
        // | A1 | yes | yes | no | yes | yes |
        // | A2 | yes | yes | no | no (live, Awaiting, unknown or moving) | no |
        // | A3 | yes | yes | yes (invalid root, defence) | any | no |
        // | A4 | yes | no (every other state) | any | any | no |
        // | A5 | no | any | any | any | no |
        let executions = [
            SessionExecutionState::Preparing,
            SessionExecutionState::Activating,
            SessionExecutionState::ActivationFailed,
            SessionExecutionState::Running,
            SessionExecutionState::Rescheduling,
            SessionExecutionState::Idle,
            SessionExecutionState::Terminated,
        ];
        for budget_admits in [false, true] {
            for execution in executions {
                for activity_open in [false, true] {
                    for proof in [false, true] {
                        let expected = !budget_admits
                            && execution == SessionExecutionState::Idle
                            && !activity_open
                            && proof;
                        assert_eq!(
                            budget_pause_absorbs_interrupt(
                                budget_paused(budget_admits, execution),
                                activity_open,
                                proof,
                            ),
                            expected,
                            "admits={budget_admits} {execution:?} open={activity_open} proof={proof}"
                        );
                    }
                }
            }
        }
        assert!(budget_pause_absorbs_interrupt(true, false, true), "A1");
        assert!(!budget_pause_absorbs_interrupt(true, false, false), "A2");
        assert!(!budget_pause_absorbs_interrupt(true, true, true), "A3");
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
    fn continuation_wakes_work_only_for_an_idle_continuing_self_hosted_worker_root() {
        // Causes: C1 frozen placement (absent, Local/Worker x cloud/self-hosted);
        // C2 terminal; C3 Idle with no open activity; C4 step is a continuation step
        // (Suspending, Hibernated, Restoring).
        // Effect E1: wake the stopped Session Work for the continuation.
        // | Rule | C1 | C2 | C3 | C4 | E1 |
        // | K1 | Worker, self-hosted | no | yes | yes | yes |
        // | K2 | Worker, self-hosted | yes | any | any | no (terminal) |
        // | K3 | Worker, self-hosted | no | no | any | no (Run's wake) |
        // | K4 | Worker, self-hosted | no | yes | no | no (no obligation) |
        // | K5 | Worker cloud / Local / absent | any | any | any | no (no Work) |
        let mut placements = vec![None];
        for runtime in [
            SessionRuntimePlacement::Local,
            SessionRuntimePlacement::Worker,
        ] {
            for self_hosted in [false, true] {
                placements.push(Some(SessionPlacementClass::new(runtime, self_hosted)));
            }
        }
        let mut cases = 0;
        for placement in placements {
            let fenced_worker = placement
                == Some(SessionPlacementClass::new(
                    SessionRuntimePlacement::Worker,
                    true,
                ));
            for terminal in [false, true] {
                for idle in [false, true] {
                    for continuing in [false, true] {
                        let rule = match (fenced_worker, terminal, idle, continuing) {
                            (false, ..) => "K5",
                            (true, true, ..) => "K2",
                            (true, false, false, _) => "K3",
                            (true, false, true, false) => "K4",
                            (true, false, true, true) => "K1",
                        };
                        assert_eq!(
                            continuation_wakes_work(placement, terminal, idle, continuing),
                            rule == "K1",
                            "{rule} {placement:?} terminal={terminal} idle={idle} continuing={continuing}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 40);
    }

    #[test]
    fn only_a_retained_idle_source_lease_outlives_its_session_work() {
        // Causes: C1 frozen placement (absent, Local/Worker x cloud/self-hosted);
        // C2 terminal; C3 frozen checkpoint-and-release retention; C4 the source
        // Sandbox is retained (generated Resident or Suspending).
        // Effect E1: the realization lease is renewed without Session Work.
        // | Rule | C1 | C2 | C3 | C4 | E1 |
        // | L1 | Worker, self-hosted | no | yes | yes | yes |
        // | L2 | Worker, self-hosted | yes | any | any | no (terminal) |
        // | L3 | Worker, self-hosted | no | no (Resident policy) | any | no |
        // | L4 | Worker, self-hosted | no | yes | no (Hibernated, Unmaterialized) | no |
        // | L5 | Worker cloud / Local / absent | any | any | any | no (no Work gate) |
        let mut placements = vec![None];
        for runtime in [
            SessionRuntimePlacement::Local,
            SessionRuntimePlacement::Worker,
        ] {
            for self_hosted in [false, true] {
                placements.push(Some(SessionPlacementClass::new(runtime, self_hosted)));
            }
        }
        let mut cases = 0;
        for placement in placements {
            let fenced_worker = placement
                == Some(SessionPlacementClass::new(
                    SessionRuntimePlacement::Worker,
                    true,
                ));
            for terminal in [false, true] {
                for policy in [false, true] {
                    for retained in [false, true] {
                        let rule = match (fenced_worker, terminal, policy, retained) {
                            (false, ..) => "L5",
                            (true, true, ..) => "L2",
                            (true, false, false, _) => "L3",
                            (true, false, true, false) => "L4",
                            (true, false, true, true) => "L1",
                        };
                        assert_eq!(
                            idle_source_lease_outlives_work(placement, terminal, policy, retained),
                            rule == "L1",
                            "{rule} {placement:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 40);
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

#[cfg(test)]
mod exhaustive {
    use super::super::execution_state::exhaustive::EVERY_EXECUTION_STATE;
    use super::*;

    const EVERY_RUNTIME: [SessionRuntimePlacement; 2] = [
        SessionRuntimePlacement::Local,
        SessionRuntimePlacement::Worker,
    ];

    const fn listed_runtime(runtime: SessionRuntimePlacement) -> usize {
        match runtime {
            SessionRuntimePlacement::Local => 0,
            SessionRuntimePlacement::Worker => 1,
        }
    }

    #[test]
    fn every_runtime_placement_is_listed() {
        for (index, runtime) in EVERY_RUNTIME.into_iter().enumerate() {
            assert_eq!(listed_runtime(runtime), index);
        }
    }

    /// @exhaustive Option<SessionPlacementClass(SessionRuntimePlacement(2) x bool(2))>(5) x SessionExecutionState(7) x SessionDisposition{Active,Deleted}(2) = 70
    /// @obligation session_creation.work_projection_is_disposable_repairable_and_non_authoritative
    /// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
    #[test]
    fn work_projection_is_derived_only_from_durable_session_truth() {
        // Cause/effect table: C1 root has no frozen baseline / local / external
        // Environment; C2 root execution and retention truth. E1 enqueue iff
        // self-hosted + nonterminal; E2 every other row stutters. Terminality is
        // derived by the production aggregate law, not taken as a free bit.
        let mut placements = vec![None];
        for runtime in EVERY_RUNTIME {
            for self_hosted in [false, true] {
                placements.push(Some(SessionPlacementClass::new(runtime, self_hosted)));
            }
        }
        let mut cases = 0;
        for &placement in &placements {
            for execution in EVERY_EXECUTION_STATE {
                for disposition in [SessionDisposition::Active, SessionDisposition::Deleted] {
                    let terminal = session_is_terminal(execution, &disposition);
                    assert_eq!(
                        session_work_projection_required(placement, terminal),
                        placement.is_some_and(SessionPlacementClass::requires_work_projection)
                            && !terminal
                    );
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 70);
    }
}
