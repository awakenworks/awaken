//! Secret-free fleet readiness projection for a hosted product plane.
//!
//! A product surface asks one question before it offers work to an AI
//! workforce: can any Worker run the capabilities this work requires, right
//! now? Placement already answers that question in
//! [`WorkerSnapshot::accepts`]. This projection asks the placement kernel
//! itself instead of re-deriving eligibility, so a fleet that reports ready
//! cannot be a fleet that placement would then refuse.
//!
//! The projection separates two different reasons for "not ready". A
//! capability no registered Worker offers is a *fleet shape* fact and is named
//! in `missing_capabilities`. A fleet that offers every required capability but
//! has no spare concurrency is a *capacity* fact: `ready_workers` is zero while
//! `missing_capabilities` stays empty. Reporting saturation as a missing
//! capability would send an operator to install a Worker they already have.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::registry::RegisteredWorker;
use crate::requirements::PlacementRequirements;

/// What a product plane may learn about the Worker fleet without receiving any
/// Worker identity, credential, or address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FleetReadiness {
    /// At least one Worker can accept this work now.
    pub ready: bool,
    /// Workers that would be admitted for the required capabilities now.
    pub ready_workers: usize,
    /// Every registered incarnation, including drained and expired ones.
    pub total_workers: usize,
    /// Required capabilities that no live Worker offers, sorted and unique.
    pub missing_capabilities: Vec<String>,
}

/// Project the fleet against the capabilities a product plane requires.
///
/// `required` is the caller's raw capability list; duplicates and ordering do
/// not change the answer. `now_ms` is the reader's clock: a Worker whose
/// registration lease has lapsed is neither ready nor a source of capability,
/// because a lapsed lease is exactly the evidence that the Worker is gone.
#[must_use]
pub fn fleet_readiness(
    workers: &[RegisteredWorker],
    required: &[String],
    now_ms: u64,
) -> FleetReadiness {
    let required: BTreeSet<String> = required.iter().cloned().collect();
    // `remote_preferred` is the current-contract constructor: `Default` is the
    // legacy v0 decoder posture, and asking with it would refuse every Worker
    // that pins a modern protocol range, reporting a healthy fleet as absent.
    let requirements = PlacementRequirements {
        required_capabilities: required.clone(),
        ..PlacementRequirements::remote_preferred()
    };

    let ready_workers = workers
        .iter()
        .filter(|worker| worker.snapshot.accepts(&requirements, now_ms))
        .count();

    // Capability supply is read from live Workers, not from ready ones: a
    // saturated Worker still proves its capability exists in this fleet.
    let offered: BTreeSet<&str> = workers
        .iter()
        .filter(|worker| is_live(worker, now_ms))
        .flat_map(|worker| {
            worker
                .snapshot
                .manifest
                .capabilities
                .iter()
                .map(String::as_str)
        })
        .collect();
    let missing_capabilities = required
        .into_iter()
        .filter(|capability| !offered.contains(capability.as_str()))
        .collect();

    FleetReadiness {
        ready: ready_workers > 0,
        ready_workers,
        total_workers: workers.len(),
        missing_capabilities,
    }
}

/// A Worker whose registration lease still holds and whose state has not left
/// service. Liveness is weaker than readiness on purpose: it admits a Worker
/// that is merely busy.
fn is_live(worker: &RegisteredWorker, now_ms: u64) -> bool {
    worker.snapshot.state.accepts_work() && worker.snapshot.expires_at_ms > now_ms
}

#[cfg(test)]
mod tests;
