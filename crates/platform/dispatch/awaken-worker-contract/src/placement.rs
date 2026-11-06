// @formal entrypoint worker.placement_extension_cannot_widen_eligibility
use std::collections::{BTreeMap, HashSet};

use thiserror::Error;

use crate::registry::{WorkerAssignment, WorkerIdentity, WorkerSnapshot, can_assign};
use crate::requirements::PlacementRequirements;

/// Placement-context key for a soft, exact Environment capacity preference.
pub const PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE: &str = "environment_shape";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementContext {
    pub run_id: String,
    pub workspace_id: String,
    pub requirements: PlacementRequirements,
    pub recovered: bool,
    pub previous_worker: Option<WorkerIdentity>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedWorker {
    pub identity: WorkerIdentity,
    pub score: i64,
    pub reason: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlacementError {
    #[error("no eligible worker")]
    NoEligibleWorker,
    #[error("placement policy failed: {0}")]
    Policy(String),
    #[error("placement policy returned an ineligible worker: {0}")]
    IneligibleResult(String),
    #[error("placement policy returned a worker more than once: {0}")]
    DuplicateResult(String),
}

/// Closed admission result for one identity returned by a replaceable ranking
/// extension. The extension may reorder or omit candidates, but this kernel is
/// the only path by which one of its results enters selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RankedCandidateAdmission {
    Eligible,
    Ineligible,
    Duplicate,
}

#[must_use]
const fn ranked_candidate_admission(
    is_allowed: bool,
    was_already_seen: bool,
) -> RankedCandidateAdmission {
    if !is_allowed {
        RankedCandidateAdmission::Ineligible
    } else if was_already_seen {
        RankedCandidateAdmission::Duplicate
    } else {
        RankedCandidateAdmission::Eligible
    }
}

/// Replaceable preference only. It receives an already-filtered candidate list.
pub trait PlacementPolicy: Send + Sync {
    fn id(&self) -> &str;

    fn rank(
        &self,
        context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LeastLoadedPolicy;

impl PlacementPolicy for LeastLoadedPolicy {
    fn id(&self) -> &str {
        "least-loaded"
    }

    fn rank(
        &self,
        context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError> {
        let mut workers = eligible.to_vec();
        let preferred = context
            .attributes
            .get(PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE);
        workers.sort_by(|left, right| {
            let left_warm = preferred
                .is_some_and(|shape| left.warm_environment_shapes.contains(shape.as_str()));
            let right_warm = preferred
                .is_some_and(|shape| right.warm_environment_shapes.contains(shape.as_str()));
            right_warm.cmp(&left_warm).then_with(|| {
                left.in_flight
                    .cmp(&right.in_flight)
                    .then_with(|| left.identity.cmp(&right.identity))
            })
        });
        Ok(workers
            .into_iter()
            .map(|worker| {
                let warm = preferred
                    .is_some_and(|shape| worker.warm_environment_shapes.contains(shape.as_str()));
                RankedWorker {
                    identity: worker.identity,
                    score: if warm { 1_000_000 } else { 0 } - i64::from(worker.in_flight),
                    reason: if warm {
                        "ready Environment shape, then least in-flight work"
                    } else {
                        "least in-flight work"
                    }
                    .to_string(),
                }
            })
            .collect())
    }
}

/// Filter through the immutable kernel, invoke the extension, then validate its
/// output again. A buggy or malicious extension can fail placement but cannot
/// widen authority.
pub fn place(
    policy: &dyn PlacementPolicy,
    context: &PlacementContext,
    workers: &[WorkerSnapshot],
    now_ms: u64,
) -> Result<RankedWorker, PlacementError> {
    let eligible = workers
        .iter()
        .filter(|worker| worker.accepts(&context.requirements, now_ms))
        .cloned()
        .collect::<Vec<_>>();
    rank_eligible(policy, context, eligible)
}

/// Placement for a concrete dispatch assignment. Unlike [`place`], this also
/// applies replacement and sandbox-continuity constraints from the durable
/// prior assignment. The extension still receives only eligible candidates.
pub fn place_assignment(
    policy: &dyn PlacementPolicy,
    context: &PlacementContext,
    workers: &[WorkerSnapshot],
    previous: Option<&WorkerAssignment>,
    sandbox_bound: bool,
    now_ms: u64,
) -> Result<RankedWorker, PlacementError> {
    let eligible = workers
        .iter()
        .filter(|worker| {
            can_assign(
                worker,
                &context.requirements,
                previous,
                sandbox_bound,
                now_ms,
            )
            .is_ok()
        })
        .cloned()
        .collect::<Vec<_>>();
    rank_eligible(policy, context, eligible)
}

fn rank_eligible(
    policy: &dyn PlacementPolicy,
    context: &PlacementContext,
    eligible: Vec<WorkerSnapshot>,
) -> Result<RankedWorker, PlacementError> {
    if eligible.is_empty() {
        return Err(PlacementError::NoEligibleWorker);
    }
    let allowed = eligible
        .iter()
        .map(|worker| worker.identity.clone())
        .collect::<HashSet<_>>();
    let ranked = policy.rank(context, &eligible)?;
    let mut seen = HashSet::new();
    let mut selected = None;
    for candidate in ranked {
        match ranked_candidate_admission(
            allowed.contains(&candidate.identity),
            seen.contains(&candidate.identity),
        ) {
            RankedCandidateAdmission::Ineligible => {
                return Err(PlacementError::IneligibleResult(
                    candidate.identity.worker_id,
                ));
            }
            RankedCandidateAdmission::Duplicate => {
                return Err(PlacementError::DuplicateResult(
                    candidate.identity.worker_id,
                ));
            }
            RankedCandidateAdmission::Eligible => {
                let inserted = seen.insert(candidate.identity.clone());
                debug_assert!(inserted, "admission checked that the identity was unseen");
            }
        }
        if selected.is_none() {
            selected = Some(candidate);
        }
    }
    selected.ok_or(PlacementError::NoEligibleWorker)
}

#[cfg(test)]
mod tests {
    use super::{RankedCandidateAdmission, ranked_candidate_admission};

    #[test]
    fn ranking_extension_result_requires_allowed_unique_identity() {
        // Causes: C1 the returned identity belongs to the prefiltered allowed
        // set; C2 the same identity was already returned by this ranking call.
        // Effects: E1 only allowed+unseen proceeds to selection; E2 an outside
        // identity fails as Ineligible; E3 a repeated allowed identity fails as
        // Duplicate. Constraint: an outside identity is never reclassified as
        // a duplicate. Decision table: R1=10=>E1, R2=00/01=>E2, R3=11=>E3.
        for (allowed, seen, expected) in [
            (true, false, RankedCandidateAdmission::Eligible),
            (false, false, RankedCandidateAdmission::Ineligible),
            (false, true, RankedCandidateAdmission::Ineligible),
            (true, true, RankedCandidateAdmission::Duplicate),
        ] {
            assert_eq!(ranked_candidate_admission(allowed, seen), expected);
        }
    }
}

#[cfg(kani)]
/// @obligation worker.placement_extension_cannot_widen_eligibility
/// @proof-input-debt free_symbol ranked_candidate_admission is_allowed
/// @proof-input-debt free_symbol ranked_candidate_admission was_already_seen
#[kani::proof]
fn placement_extension_admits_only_allowed_unique_identity() {
    // Symbolic form of the unit decision table: all four C1/C2 combinations
    // are covered, and E1 is equivalent to allowed AND not previously seen.
    let is_allowed = kani::any::<bool>();
    let was_already_seen = kani::any::<bool>();
    let admission = ranked_candidate_admission(is_allowed, was_already_seen);
    assert_eq!(
        admission == RankedCandidateAdmission::Eligible,
        is_allowed && !was_already_seen
    );
    if !is_allowed {
        assert_eq!(admission, RankedCandidateAdmission::Ineligible);
    } else if was_already_seen {
        assert_eq!(admission, RankedCandidateAdmission::Duplicate);
    }
}
