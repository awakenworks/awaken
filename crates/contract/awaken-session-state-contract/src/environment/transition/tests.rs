//! Decision tables of the Environment transition function.
//!
//! Table T1 fixes every step × event cell under the ready facts (an active,
//! fenced, idle Session whose suspend is due, whose evidence, generation and
//! checkpoint are all valid, whose topology supports continuation and whose
//! Memory evidence admits a suspend). Tables T2–T7 and T9 vary one qualifier at
//! a time and cover every fail-closed row. Table T8 sweeps the complete finite
//! domain against the three safety laws that the Kani harnesses prove.

use super::laws::{
    EVENT_COUNT, STEPS, effects_require_current_lease, event, steps_follow_declared_edges,
    terminal_never_reenters,
};
use super::*;
use MaterializationKind as K;
use SessionEnvironmentEvent as E;
use SessionEnvironmentInstruction as I;
use SessionEnvironmentRejection as X;
use SessionEnvironmentStep as S;

const U: S = S::Unmaterialized;
const R: S = S::Resident;
const SQ: S = S::Suspending(SuspendPhase::Quiescing);
const SU: S = S::Suspending(SuspendPhase::Uploading);
const SR: S = S::Suspending(SuspendPhase::ReadyToDispose);
const SD: S = S::Suspending(SuspendPhase::Disposing);
const H: S = S::Hibernated;
const RS: S = S::Restoring;

fn ready(step: S) -> SessionEnvironmentFacts {
    SessionEnvironmentFacts {
        step,
        terminality: SessionTerminality::Active,
        execution_idle: true,
        activities_open: false,
        lease: RealizationLeaseStanding::Fenced,
        resident_generated: true,
        suspend_due: true,
        suspend_evidence_consistent: true,
        generation_live: true,
        checkpoint_live: true,
        continuation_topology_supported: true,
        suspend_memory_admitted: true,
    }
}

/// Expected cell: `Go(next)`, `Do(next, instruction)`, `Stay` (next is the
/// current step, no instruction) or `No(rejection)`.
#[derive(Clone, Copy, Debug)]
enum Cell {
    Stay,
    Go(S),
    Do(S, I),
    DoHere(I),
    No(X),
}
use Cell::{Do, DoHere, Go, No, Stay};

fn expected(step: S, cell: Cell) -> Outcome {
    match cell {
        Stay => Ok(SessionEnvironmentDecision {
            next: step,
            instruction: None,
        }),
        Go(next) => Ok(SessionEnvironmentDecision {
            next,
            instruction: None,
        }),
        Do(next, instruction) => Ok(SessionEnvironmentDecision {
            next,
            instruction: Some(instruction),
        }),
        DoHere(instruction) => Ok(SessionEnvironmentDecision {
            next: step,
            instruction: Some(instruction),
        }),
        No(rejection) => Err(rejection),
    }
}

fn check_row(rule: &str, facts: SessionEnvironmentFacts, event: E, cell: Cell) {
    assert_eq!(
        transition_environment(facts, event),
        expected(facts.step, cell),
        "{rule}: {:?} on {:?}",
        event,
        facts
    );
}

const CONT: Cell = No(X::ContinuationInProgress);
const WS: Cell = No(X::WrongStep);

fn admit(kind: K, already_applied: bool) -> E {
    E::AdmitMaterialization {
        kind,
        already_applied,
    }
}

fn materialized(kind: K, already_applied: bool) -> E {
    E::Materialized {
        kind,
        already_applied,
    }
}

#[test]
fn t1_every_step_and_event_under_ready_facts() {
    // Columns: U, R, Suspending{Quiescing, Uploading, ReadyToDispose,
    // Disposing}, Hibernated, Restoring. Sources per row are named in the
    // transition module; this table is the complete closed cell set.
    let rows: [(&str, E, [Cell; 8]); 31] = [
        (
            "begin",
            E::BeginRealization,
            [Stay, Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "renew",
            E::RenewRealization,
            [Stay, Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "admit create",
            admit(K::Create, false),
            [
                DoHere(I::Materialize(K::Create)),
                No(X::InvalidTransition),
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "admit adopt",
            admit(K::Adopt, false),
            [
                DoHere(I::Materialize(K::Adopt)),
                DoHere(I::Materialize(K::Adopt)),
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "admit rebuild",
            admit(K::Rebuild, false),
            [
                No(X::RequiresResident),
                No(X::MaterializationPending),
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "admit reserve",
            admit(K::ReserveResources, false),
            [
                No(X::RequiresResident),
                DoHere(I::Materialize(K::ReserveResources)),
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "admit create replay",
            admit(K::Create, true),
            [Stay, Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "admit adopt replay",
            admit(K::Adopt, true),
            [Stay, Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "admit rebuild replay",
            admit(K::Rebuild, true),
            [
                No(X::RequiresResident),
                No(X::MaterializationPending),
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "admit reserve replay",
            admit(K::ReserveResources, true),
            [
                No(X::RequiresResident),
                Stay,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "advance",
            E::AdvanceContinuation,
            [
                Stay,
                Do(SQ, I::Quiesce),
                DoHere(I::Quiesce),
                DoHere(I::Checkpoint),
                DoHere(I::PrepareSourceRelease),
                DoHere(I::DisposeSource),
                Stay,
                Stay,
            ],
        ),
        (
            "require resident",
            E::RequireResident,
            [
                Stay,
                Stay,
                DoHere(I::Quiesce),
                DoHere(I::Checkpoint),
                DoHere(I::PrepareSourceRelease),
                DoHere(I::DisposeSource),
                Do(RS, I::Restore),
                DoHere(I::Restore),
            ],
        ),
        ("retire", E::RetireForCleanup, [No(X::NotTerminal); 8]),
        (
            "materialized create",
            materialized(K::Create, false),
            [
                Go(R),
                No(X::InvalidTransition),
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "materialized adopt",
            materialized(K::Adopt, false),
            [Go(R), Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "materialized rebuild",
            materialized(K::Rebuild, false),
            [
                No(X::RequiresResident),
                Stay,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "materialized reserve",
            materialized(K::ReserveResources, false),
            [
                No(X::RequiresResident),
                Stay,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "materialized create replay",
            materialized(K::Create, true),
            [Go(R), Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "materialized adopt replay",
            materialized(K::Adopt, true),
            [Go(R), Stay, CONT, CONT, CONT, CONT, CONT, CONT],
        ),
        (
            "materialized rebuild replay",
            materialized(K::Rebuild, true),
            [
                No(X::RequiresResident),
                Stay,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "materialized reserve replay",
            materialized(K::ReserveResources, true),
            [
                No(X::RequiresResident),
                Stay,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
                CONT,
            ],
        ),
        (
            "legacy binding",
            E::LegacyBindingDelivered,
            [Go(R), Stay, Stay, Stay, Stay, Stay, Stay, Stay],
        ),
        (
            "quiesced",
            E::Quiesced,
            [WS, WS, Go(SU), Stay, Stay, Stay, WS, WS],
        ),
        (
            "checkpointed",
            E::Checkpointed,
            [WS, WS, WS, Go(SR), Stay, Stay, WS, WS],
        ),
        (
            "prepared",
            E::SourceReleasePrepared,
            [WS, WS, WS, WS, Go(SD), Stay, WS, WS],
        ),
        (
            "released",
            E::SourceReleased,
            [WS, WS, WS, WS, WS, Go(H), Stay, WS],
        ),
        ("restored", E::Restored, [WS, WS, WS, WS, WS, WS, WS, Go(R)]),
        (
            "expired checkpoint deleted (live)",
            E::ExpiredCheckpointDeleted,
            [WS, WS, WS, WS, WS, WS, No(X::CheckpointLive), WS],
        ),
        (
            "cleaned up",
            E::CleanedUp {
                already_recorded: false,
            },
            [No(X::NotTerminal); 8],
        ),
        (
            "cleaned up replay",
            E::CleanedUp {
                already_recorded: true,
            },
            [No(X::NotTerminal); 8],
        ),
        ("effect failed", E::EffectFailed, [Stay; 8]),
    ];
    // Every event value appears exactly once.
    for index in 0..EVENT_COUNT {
        assert_eq!(
            rows.iter()
                .filter(|(_, event_, _)| *event_ == event(index))
                .count(),
            1,
            "T1 covers {:?}",
            event(index)
        );
    }
    for (rule, event, cells) in rows {
        for (step, cell) in STEPS.into_iter().zip(cells) {
            check_row(rule, ready(step), event, cell);
        }
    }
}

#[test]
fn t2_terminal_session_retires_and_joins_only() {
    // Terminal (cleanup not Fenced) and CleanupJoining (Fenced join) under a
    // current lease. Renewal and retirement keep working in every step; every
    // continuation and admission command is refused; only the join admits an
    // in-flight materialization receipt; disposal retires to Unmaterialized.
    for terminality in [
        SessionTerminality::Terminal,
        SessionTerminality::CleanupJoining,
    ] {
        let joining = terminality == SessionTerminality::CleanupJoining;
        for step in STEPS {
            let facts = SessionEnvironmentFacts {
                terminality,
                ..ready(step)
            };
            let terminal = No(X::Terminal);
            check_row("T2 begin", facts, E::BeginRealization, terminal);
            check_row("T2 renew", facts, E::RenewRealization, Stay);
            check_row("T2 admit", facts, admit(K::Adopt, false), terminal);
            check_row("T2 advance", facts, E::AdvanceContinuation, terminal);
            check_row("T2 require", facts, E::RequireResident, terminal);
            check_row("T2 retire", facts, E::RetireForCleanup, DoHere(I::Retire));
            for observation in [
                E::Quiesced,
                E::Checkpointed,
                E::SourceReleasePrepared,
                E::SourceReleased,
                E::Restored,
                E::ExpiredCheckpointDeleted,
            ] {
                check_row("T2 continuation observation", facts, observation, terminal);
            }
            let cleaned = E::CleanedUp {
                already_recorded: false,
            };
            check_row("T2 cleaned up", facts, cleaned, Go(U));
            let replay = E::CleanedUp {
                already_recorded: true,
            };
            check_row("T2 cleaned up replay", facts, replay, Stay);
            check_row("T2 effect failed", facts, E::EffectFailed, Stay);
            let receipt = materialized(K::Create, false);
            let legacy = E::LegacyBindingDelivered;
            match (joining, step) {
                (false, _) => {
                    check_row("T2 receipt after join", facts, receipt, terminal);
                    check_row("T2 legacy after join", facts, legacy, terminal);
                }
                (true, U) => {
                    check_row("T2 receipt in join", facts, receipt, Go(R));
                    check_row("T2 legacy in join", facts, legacy, terminal);
                }
                (true, R) => {
                    check_row(
                        "T2 receipt in join",
                        facts,
                        receipt,
                        No(X::InvalidTransition),
                    );
                    check_row("T2 legacy in join", facts, legacy, terminal);
                }
                (true, _) => {
                    check_row("T2 receipt in join", facts, receipt, CONT);
                    check_row("T2 legacy in join", facts, legacy, terminal);
                }
            }
        }
    }
}

#[test]
fn t3_lease_standing_fences_every_effect() {
    // F = Fenced, N = Unfenced (legacy/local, no lease anywhere), S = Stale.
    // L1 S refuses every command that may instruct an effect and every
    // receipt; L2 N still admits legacy adoption, quiescence, checkpoint,
    // restore and receipts; L3 N refuses Create, source release, renewal and
    // every terminal step that needs the lease.
    let with = |step, lease| SessionEnvironmentFacts {
        lease,
        ..ready(step)
    };
    let terminal = |step, lease| SessionEnvironmentFacts {
        terminality: SessionTerminality::Terminal,
        lease,
        ..ready(step)
    };
    let lease_lost = No(X::LeaseNotCurrent);
    for lease in [
        RealizationLeaseStanding::Unfenced,
        RealizationLeaseStanding::Stale,
    ] {
        let stale = lease == RealizationLeaseStanding::Stale;
        check_row("L renew", with(R, lease), E::RenewRealization, lease_lost);
        check_row(
            "L admit create",
            with(U, lease),
            admit(K::Create, false),
            lease_lost,
        );
        check_row(
            "L admit adopt",
            with(U, lease),
            admit(K::Adopt, false),
            if stale {
                lease_lost
            } else {
                DoHere(I::Materialize(K::Adopt))
            },
        );
        check_row(
            "L receipt",
            with(U, lease),
            materialized(K::Create, false),
            if stale { lease_lost } else { Go(R) },
        );
        check_row(
            "L advance suspend",
            with(R, lease),
            E::AdvanceContinuation,
            if stale {
                lease_lost
            } else {
                Do(SQ, I::Quiesce)
            },
        );
        check_row(
            "L advance quiesce",
            with(SQ, lease),
            E::AdvanceContinuation,
            if stale {
                lease_lost
            } else {
                DoHere(I::Quiesce)
            },
        );
        check_row(
            "L advance checkpoint",
            with(SU, lease),
            E::AdvanceContinuation,
            if stale {
                lease_lost
            } else {
                DoHere(I::Checkpoint)
            },
        );
        check_row(
            "L advance prepare",
            with(SR, lease),
            E::AdvanceContinuation,
            lease_lost,
        );
        check_row(
            "L advance dispose",
            with(SD, lease),
            E::AdvanceContinuation,
            lease_lost,
        );
        check_row(
            "L require restore",
            with(H, lease),
            E::RequireResident,
            if stale {
                lease_lost
            } else {
                Do(RS, I::Restore)
            },
        );
        check_row(
            "L prepared",
            with(SR, lease),
            E::SourceReleasePrepared,
            lease_lost,
        );
        check_row(
            "L retire",
            terminal(R, lease),
            E::RetireForCleanup,
            lease_lost,
        );
        let cleaned = E::CleanedUp {
            already_recorded: false,
        };
        check_row("L cleaned up", terminal(R, lease), cleaned, lease_lost);
        let replay = E::CleanedUp {
            already_recorded: true,
        };
        check_row("L cleaned up replay", terminal(R, lease), replay, Stay);
        check_row(
            "L renew terminal",
            terminal(SQ, lease),
            E::RenewRealization,
            lease_lost,
        );
        check_row("L begin", with(U, lease), E::BeginRealization, Stay);
    }
}

#[test]
fn t4_suspend_due_requires_idle_execution_no_activity_and_a_generation() {
    // D1 Resident: suspend begins only when due AND execution Idle; then open
    // activities and an ungenerated binding refuse, exactly as the aggregate's
    // begin_suspend_at wrapper does after the continuation guard.
    let resident = |suspend_due, execution_idle, activities_open, resident_generated| {
        SessionEnvironmentFacts {
            suspend_due,
            execution_idle,
            activities_open,
            resident_generated,
            ..ready(R)
        }
    };
    for (rule, facts, cell) in [
        ("D1 not due", resident(false, true, false, true), Stay),
        ("D1 busy", resident(true, false, false, true), Stay),
        ("D1 not due busy", resident(false, false, true, false), Stay),
        (
            "D1 activities",
            resident(true, true, true, true),
            No(X::ActivitiesOpen),
        ),
        (
            "D1 ungenerated",
            resident(true, true, false, false),
            No(X::NotGenerated),
        ),
        (
            "D1 due",
            resident(true, true, false, true),
            Do(SQ, I::Quiesce),
        ),
    ] {
        check_row(rule, facts, E::AdvanceContinuation, cell);
        check_row(rule, facts, E::RequireResident, Stay);
    }
}

#[test]
fn t5_inconsistent_suspend_evidence_fails_closed() {
    // Every suspend sub-phase whose checkpoint/preparation evidence disagrees
    // with it refuses both drivers and every continuation observation.
    for step in [SQ, SU, SR, SD] {
        let facts = SessionEnvironmentFacts {
            suspend_evidence_consistent: false,
            ..ready(step)
        };
        let refused = No(X::InconsistentEvidence);
        for event in [
            E::AdvanceContinuation,
            E::RequireResident,
            E::Quiesced,
            E::Checkpointed,
            E::SourceReleasePrepared,
            E::SourceReleased,
        ] {
            check_row("E1 inconsistent", facts, event, refused);
        }
    }
}

#[test]
fn t6_expiry_decides_checkpoint_and_restore() {
    // X1 Uploading with an expired generation refuses its checkpoint; X2 a
    // Hibernated checkpoint or generation past expiry is deleted instead of
    // restored, and its deletion receipt retires to Unmaterialized; X3 a live
    // Hibernated Environment with open activities cannot restore.
    let expired_upload = SessionEnvironmentFacts {
        generation_live: false,
        ..ready(SU)
    };
    for event in [E::AdvanceContinuation, E::RequireResident] {
        check_row("X1", expired_upload, event, No(X::GenerationExpired));
    }
    for (checkpoint_live, generation_live) in [(false, true), (true, false), (false, false)] {
        let facts = SessionEnvironmentFacts {
            checkpoint_live,
            generation_live,
            ..ready(H)
        };
        check_row(
            "X2 delete",
            facts,
            E::RequireResident,
            DoHere(I::DeleteExpiredCheckpoint),
        );
        check_row("X2 deleted", facts, E::ExpiredCheckpointDeleted, Go(U));
        check_row("X2 no restore loop", facts, E::AdvanceContinuation, Stay);
    }
    let busy = SessionEnvironmentFacts {
        activities_open: true,
        ..ready(H)
    };
    check_row("X3", busy, E::RequireResident, No(X::ActivitiesOpen));
}

#[test]
fn t7_realization_gate_is_the_continuation_complement() {
    // G1 the shared gate admits exactly Unmaterialized and Resident.
    for step in STEPS {
        assert_eq!(
            step.admits_realization_effects(),
            matches!(step, U | R),
            "G1 {step:?}"
        );
    }
}

#[test]
fn t8_complete_domain_satisfies_the_safety_laws() {
    let terminalities = [
        SessionTerminality::Active,
        SessionTerminality::CleanupJoining,
        SessionTerminality::Terminal,
    ];
    let leases = [
        RealizationLeaseStanding::Fenced,
        RealizationLeaseStanding::Unfenced,
        RealizationLeaseStanding::Stale,
    ];
    let mut cases = 0_usize;
    for step in STEPS {
        for terminality in terminalities {
            for lease in leases {
                for bits in 0_u16..512 {
                    let facts = SessionEnvironmentFacts {
                        step,
                        terminality,
                        lease,
                        execution_idle: bits & 1 != 0,
                        activities_open: bits & 2 != 0,
                        resident_generated: bits & 4 != 0,
                        suspend_due: bits & 8 != 0,
                        suspend_evidence_consistent: bits & 16 != 0,
                        generation_live: bits & 32 != 0,
                        checkpoint_live: bits & 64 != 0,
                        continuation_topology_supported: bits & 128 != 0,
                        suspend_memory_admitted: bits & 256 != 0,
                    };
                    for index in 0..EVENT_COUNT {
                        let event = event(index);
                        let outcome = transition_environment(facts, event);
                        assert!(
                            terminal_never_reenters(facts, event, outcome),
                            "L1 {facts:?} {event:?}"
                        );
                        assert!(
                            steps_follow_declared_edges(facts, event, outcome),
                            "L2 {facts:?} {event:?}"
                        );
                        assert!(
                            effects_require_current_lease(facts, event, outcome),
                            "L3 {facts:?} {event:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 8 * 3 * 3 * 512 * EVENT_COUNT);
}

#[test]
fn t9_frozen_topology_and_memory_evidence_guard_the_continuation() {
    // P1 a frozen topology without a continuation transport refuses both
    // continuation commands in every step, after terminality and before the
    // lease; P2 it leaves realization, receipts and terminal cleanup alone.
    // M1 a due Resident whose Memory evidence does not admit a suspend
    // refuses the suspend intent before activities and generation; M2 the
    // Memory fact is read only by the suspend intent (every other step and
    // a not-due Resident are unaffected).
    for step in STEPS {
        let unsupported = SessionEnvironmentFacts {
            continuation_topology_supported: false,
            ..ready(step)
        };
        let refused = No(X::UnsupportedContinuationTopology);
        check_row("P1 advance", unsupported, E::AdvanceContinuation, refused);
        check_row("P1 require", unsupported, E::RequireResident, refused);
        let stale = SessionEnvironmentFacts {
            lease: RealizationLeaseStanding::Stale,
            ..unsupported
        };
        check_row("P1 before lease", stale, E::AdvanceContinuation, refused);
        let terminal = SessionEnvironmentFacts {
            terminality: SessionTerminality::Terminal,
            ..unsupported
        };
        check_row(
            "P1 after terminality",
            terminal,
            E::RequireResident,
            No(X::Terminal),
        );
        for event in [
            E::BeginRealization,
            E::RenewRealization,
            materialized(K::Adopt, false),
            E::Quiesced,
            E::EffectFailed,
        ] {
            assert_eq!(
                transition_environment(unsupported, event),
                transition_environment(ready(step), event),
                "P2 {step:?} {event:?}"
            );
        }
        check_row(
            "P2 retire",
            terminal,
            E::RetireForCleanup,
            DoHere(I::Retire),
        );
    }
    let unproven = |suspend_due, activities_open, resident_generated| SessionEnvironmentFacts {
        suspend_memory_admitted: false,
        suspend_due,
        activities_open,
        resident_generated,
        ..ready(R)
    };
    let refused = No(X::MemoryEvidenceUnproven);
    check_row(
        "M1 due",
        unproven(true, false, true),
        E::AdvanceContinuation,
        refused,
    );
    check_row(
        "M1 first",
        unproven(true, true, false),
        E::AdvanceContinuation,
        refused,
    );
    check_row(
        "M2 not due",
        unproven(false, false, true),
        E::AdvanceContinuation,
        Stay,
    );
    check_row(
        "M2 require",
        unproven(true, false, true),
        E::RequireResident,
        Stay,
    );
    for step in [SQ, SU, SR, SD, H, RS] {
        let facts = SessionEnvironmentFacts {
            suspend_memory_admitted: false,
            ..ready(step)
        };
        for event in [E::AdvanceContinuation, E::RequireResident] {
            assert_eq!(
                transition_environment(facts, event),
                transition_environment(ready(step), event),
                "M2 {step:?} {event:?}"
            );
        }
    }
}

#[test]
fn t10_the_count_phase_is_the_steps_projection() {
    // G2 every step projects onto exactly the durable phase whose
    // SessionEnvironmentState variant it refines; the four suspend sub-phases
    // share Suspending.
    use super::super::SessionEnvironmentPhase as P;
    for (step, phase) in [
        (U, P::Unmaterialized),
        (R, P::Resident),
        (SQ, P::Suspending),
        (SU, P::Suspending),
        (SR, P::Suspending),
        (SD, P::Suspending),
        (H, P::Hibernated),
        (RS, P::Restoring),
    ] {
        assert_eq!(step.phase(), phase, "G2 {step:?}");
    }
}
