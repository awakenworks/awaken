use std::collections::BTreeSet;

use super::*;
use crate::manifest::{
    CURRENT_RUNTIME_PROTOCOL_VERSION, VersionRange, WorkerCapacity, WorkerManifest,
};
use crate::registry::{RegisteredWorker, WorkerIdentity, WorkerSnapshot, WorkerState};

const NOW: u64 = 1_000;

fn worker(
    id: &str,
    state: WorkerState,
    capabilities: &[&str],
    in_flight: u32,
    max_concurrent: u32,
    expires_at_ms: u64,
) -> RegisteredWorker {
    let manifest = WorkerManifest {
        capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        capacity: WorkerCapacity {
            max_concurrent,
            ..WorkerCapacity::default()
        },
        // A registered Worker always pins an explicit protocol range; the
        // `ANY` default stands for a manifest authored before the range
        // existed, which `can_claim` refuses. Fixtures must therefore look
        // like a real registration, not like that legacy shape.
        runtime_protocol: VersionRange {
            min: 1,
            max: CURRENT_RUNTIME_PROTOCOL_VERSION,
        },
        ..WorkerManifest::default()
    };
    let capability_fingerprint = manifest
        .fingerprint()
        .expect("a test manifest is always fingerprintable");
    RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: WorkerIdentity {
                worker_id: id.to_owned(),
                incarnation_id: format!("{id}-1"),
                generation: 1,
            },
            state,
            manifest,
            capability_fingerprint,
            in_flight,
            warm_environment_shapes: BTreeSet::new(),
            credential_observations: BTreeSet::new(),
            acp_capability_observations: Vec::new(),
            expires_at_ms,
        },
        heartbeat_sequence: 1,
        observation_sequence: 1,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
        process_exit: None,
        create_ledger: None,
        source_retention: Default::default(),
    }
}

/// A Worker that is live, idle, and offers `coding`.
fn healthy(id: &str) -> RegisteredWorker {
    worker(id, WorkerState::Ready, &["coding"], 0, 4, NOW + 1)
}

#[test]
fn readiness_is_granted_only_by_a_worker_placement_would_also_admit() {
    // Causes, one per row: C1 the Worker state accepts work; C2 the
    // registration lease still holds; C3 spare concurrency exists; C4 the
    // required capability is offered. A Worker grants readiness only when all
    // four hold, which is exactly the conjunction `WorkerSnapshot::accepts`
    // evaluates for placement. Every row is asserted so no single cause can be
    // dropped without failing here.
    let required = vec!["coding".to_owned()];
    for state_ok in [false, true] {
        for lease_ok in [false, true] {
            for capacity_ok in [false, true] {
                for capability_ok in [false, true] {
                    let fleet = [worker(
                        "w1",
                        if state_ok {
                            WorkerState::Ready
                        } else {
                            WorkerState::Draining
                        },
                        if capability_ok {
                            &["coding"]
                        } else {
                            &["docs"]
                        },
                        if capacity_ok { 0 } else { 4 },
                        4,
                        if lease_ok { NOW + 1 } else { NOW },
                    )];
                    let projection = fleet_readiness(&fleet, &required, NOW);
                    let admitted = state_ok && lease_ok && capacity_ok && capability_ok;
                    assert_eq!(
                        projection.ready, admitted,
                        "state={state_ok} lease={lease_ok} capacity={capacity_ok} \
                         capability={capability_ok}"
                    );
                    assert_eq!(projection.ready_workers, usize::from(admitted));
                    assert_eq!(projection.total_workers, 1, "registration is not readiness");
                }
            }
        }
    }
}

#[test]
fn a_saturated_fleet_reports_capacity_not_a_missing_capability() {
    // The distinction this projection exists to make: every required
    // capability is present, so an operator must not be told to install a
    // Worker they already run. Only `ready_workers` reports the shortage.
    let fleet = [worker(
        "busy",
        WorkerState::Ready,
        &["coding"],
        4,
        4,
        NOW + 1,
    )];
    let projection = fleet_readiness(&fleet, &["coding".to_owned()], NOW);
    assert!(!projection.ready);
    assert_eq!(projection.ready_workers, 0);
    assert_eq!(projection.total_workers, 1);
    assert!(
        projection.missing_capabilities.is_empty(),
        "a busy Worker still proves its capability exists in this fleet"
    );
}

#[test]
fn a_capability_no_live_worker_offers_is_named() {
    let fleet = [healthy("w1")];
    let projection = fleet_readiness(&fleet, &["coding".to_owned(), "gpu".to_owned()], NOW);
    assert!(!projection.ready, "an unmet requirement admits no Worker");
    assert_eq!(projection.missing_capabilities, vec!["gpu".to_owned()]);
}

#[test]
fn a_lapsed_lease_supplies_no_capability() {
    // Liveness gates capability supply as well as readiness: a Worker whose
    // lease has lapsed is gone, and reporting its capabilities as present
    // would hide the real reason the fleet cannot run the work.
    let fleet = [worker(
        "expired",
        WorkerState::Ready,
        &["coding"],
        0,
        4,
        NOW, // expires_at_ms is not strictly after now
    )];
    let projection = fleet_readiness(&fleet, &["coding".to_owned()], NOW);
    assert_eq!(projection.ready_workers, 0);
    assert_eq!(projection.total_workers, 1);
    assert_eq!(projection.missing_capabilities, vec!["coding".to_owned()]);
}

#[test]
fn an_empty_requirement_asks_only_whether_any_worker_can_take_work() {
    let fleet = [
        healthy("w1"),
        worker("d", WorkerState::Draining, &[], 0, 4, NOW + 1),
    ];
    let projection = fleet_readiness(&fleet, &[], NOW);
    assert!(projection.ready);
    assert_eq!(projection.ready_workers, 1);
    assert_eq!(projection.total_workers, 2);
    assert!(projection.missing_capabilities.is_empty());
}

#[test]
fn the_answer_does_not_depend_on_how_the_caller_spelled_its_list() {
    // The wire form is a comma-joined string, so duplicates and ordering reach
    // this kernel routinely and must not change the projection.
    let fleet = [healthy("w1")];
    let canonical = fleet_readiness(&fleet, &["coding".to_owned(), "gpu".to_owned()], NOW);
    let noisy = fleet_readiness(
        &fleet,
        &[
            "gpu".to_owned(),
            "coding".to_owned(),
            "gpu".to_owned(),
            "coding".to_owned(),
        ],
        NOW,
    );
    assert_eq!(canonical, noisy);
}

#[test]
fn readiness_counts_every_admitted_worker_not_just_the_first() {
    let fleet = [
        healthy("w1"),
        healthy("w2"),
        worker("busy", WorkerState::Ready, &["coding"], 4, 4, NOW + 1),
    ];
    let projection = fleet_readiness(&fleet, &["coding".to_owned()], NOW);
    assert!(projection.ready);
    assert_eq!(projection.ready_workers, 2);
    assert_eq!(projection.total_workers, 3);
}

#[test]
fn an_empty_fleet_is_not_ready_and_names_every_requirement() {
    let projection = fleet_readiness(&[], &["coding".to_owned()], NOW);
    assert_eq!(
        projection,
        FleetReadiness {
            ready: false,
            ready_workers: 0,
            total_workers: 0,
            missing_capabilities: vec!["coding".to_owned()],
        }
    );
}
