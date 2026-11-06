//! Decision table of the Resource reconciliation decision.
//!
//! Causes: C1 terminal root; C2 external Worker realizes the Environment;
//! C3 execution Idle; C4 pending generation; C5 Repository retirements;
//! C6 Releasing activation record; C7 installed manifest has inputs;
//! C8 activation records exist. Table R fixes one row per branch; table S
//! sweeps all 256 fact combinations against the safety laws the Kani harness
//! proves.

use super::*;
use SessionResourceLocalApply as L;
use SessionResourceReconciliationInstruction as I;

#[derive(Clone, Copy)]
struct Row {
    terminal: bool,
    external: bool,
    idle: bool,
    pending: bool,
    retirements: bool,
    releasing: bool,
    active_inputs: bool,
    records: bool,
}

const LIVE_IDLE_LOCAL: Row = Row {
    terminal: false,
    external: false,
    idle: true,
    pending: false,
    retirements: false,
    releasing: false,
    active_inputs: false,
    records: false,
};

const fn facts(row: Row) -> SessionResourceReconciliationFacts {
    SessionResourceReconciliationFacts {
        terminal: row.terminal,
        external_environment: row.external,
        execution_idle: row.idle,
        pending: row.pending,
        repository_retirements: row.retirements,
        releasing_activations: row.releasing,
        active_inputs: row.active_inputs,
        activation_records: row.records,
    }
}

#[test]
fn every_branch_of_the_resource_reconciliation_decision() {
    let base = LIVE_IDLE_LOCAL;
    let rows: [(&str, Row, I); 17] = [
        // Terminal roots go to terminal cleanup whatever else holds.
        (
            "R1",
            Row {
                terminal: true,
                ..base
            },
            I::ReleaseTerminal,
        ),
        (
            "R2",
            Row {
                terminal: true,
                external: true,
                pending: true,
                ..base
            },
            I::ReleaseTerminal,
        ),
        (
            "R3",
            Row {
                terminal: true,
                idle: false,
                active_inputs: true,
                records: true,
                ..base
            },
            I::ReleaseTerminal,
        ),
        // A live external root only retires detached participants.
        (
            "R4",
            Row {
                external: true,
                ..base
            },
            I::Hold,
        ),
        (
            "R5",
            Row {
                external: true,
                retirements: true,
                ..base
            },
            I::RetireDetachedGeneration,
        ),
        (
            "R6",
            Row {
                external: true,
                pending: true,
                retirements: true,
                ..base
            },
            I::Hold,
        ),
        (
            "R7",
            Row {
                external: true,
                idle: false,
                retirements: true,
                ..base
            },
            I::RetireDetachedGeneration,
        ),
        // A live local root defers every change while a Run owns execution.
        (
            "R8",
            Row {
                idle: false,
                pending: true,
                releasing: true,
                active_inputs: true,
                records: true,
                retirements: true,
                ..base
            },
            I::Hold,
        ),
        // Live, Idle, Coordinator-realized: the ordinary apply.
        ("R9", base, I::Hold),
        (
            "R10",
            Row {
                pending: true,
                releasing: true,
                records: true,
                ..base
            },
            I::ApplyLocally(L::InstallPending),
        ),
        (
            "R11",
            Row {
                releasing: true,
                records: true,
                active_inputs: true,
                ..base
            },
            I::ApplyLocally(L::RefuseReleasingWithoutPending),
        ),
        (
            "R12",
            Row {
                active_inputs: true,
                ..base
            },
            I::ApplyLocally(L::ReapplyActive { adopt_legacy: true }),
        ),
        (
            "R13",
            Row {
                active_inputs: true,
                records: true,
                ..base
            },
            I::ApplyLocally(L::ReapplyActive {
                adopt_legacy: false,
            }),
        ),
        (
            "R14",
            Row {
                records: true,
                ..base
            },
            I::ApplyLocally(L::ReapplyActive {
                adopt_legacy: false,
            }),
        ),
        (
            "R15",
            Row {
                retirements: true,
                ..base
            },
            I::ApplyLocally(L::RetireDetachedGeneration),
        ),
        (
            "R16",
            Row {
                retirements: true,
                active_inputs: true,
                records: true,
                ..base
            },
            I::ApplyLocally(L::ReapplyActive {
                adopt_legacy: false,
            }),
        ),
        (
            "R17",
            Row {
                pending: true,
                retirements: true,
                ..base
            },
            I::ApplyLocally(L::InstallPending),
        ),
    ];
    for (rule, row, expected) in rows {
        assert_eq!(next_resource_reconciliation(facts(row)), expected, "{rule}");
    }
}

#[test]
fn resource_reconciliation_sweep_holds_the_safety_laws() {
    for bits in 0_u16..256 {
        let bit = |index: u16| bits & (1 << index) != 0;
        let facts = facts(Row {
            terminal: bit(0),
            external: bit(1),
            idle: bit(2),
            pending: bit(3),
            retirements: bit(4),
            releasing: bit(5),
            active_inputs: bit(6),
            records: bit(7),
        });
        let decided = next_resource_reconciliation(facts);
        // S1 terminal roots always and only reach terminal cleanup.
        assert_eq!(
            decided == I::ReleaseTerminal,
            facts.terminal,
            "S1 {facts:?}"
        );
        // S2 a local Runtime effect needs a live, Idle, Coordinator-realized root.
        if matches!(decided, I::ApplyLocally(_)) {
            assert!(
                !facts.terminal && !facts.external_environment && facts.execution_idle,
                "S2 {facts:?}"
            );
        }
        // S3 a pending generation is never retired around nor left for the
        // detached-generation path.
        if facts.pending && decided == I::RetireDetachedGeneration {
            panic!("S3 {facts:?}");
        }
        // S4 Hold is an exact no-op: nothing local to install, re-apply or
        // retire is ever skipped on a live, Idle, Coordinator-realized root.
        if decided == I::Hold && !facts.terminal && !facts.external_environment {
            assert!(
                !facts.execution_idle
                    || !(facts.pending
                        || facts.repository_retirements
                        || facts.releasing_activations
                        || facts.active_inputs
                        || facts.activation_records),
                "S4 {facts:?}"
            );
        }
    }
}
