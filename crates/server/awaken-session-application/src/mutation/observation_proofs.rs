//! Direct production-kernel proofs; repository transactions remain a separate boundary.
use super::*;

fn check_observation(previous: Option<u64>, now: u64, later: u64) {
    let mut observed = previous;
    observe_root_update(&mut observed, now);
    let value = observed.expect("every applied candidate has an observation");
    assert!(value >= now);
    if let Some(old) = previous {
        assert!(value >= old);
        assert!(value == old || value == now);
    } else {
        assert_eq!(value, now);
    }
    // A repeated sample is idempotent. A second arbitrary clock, including a
    // backwards clock, cannot undo the first observation.
    observe_root_update(&mut observed, now);
    assert_eq!(observed, Some(value));
    observe_root_update(&mut observed, later);
    let final_value = observed.unwrap();
    assert!(final_value >= value && final_value >= later);
    assert!(final_value == value || final_value == later);
}

fn check_receipt(kind: u8, revision: u64) {
    let revision = SessionRevision(revision);
    let receipt = match kind {
        0 => SessionMutationResult::Applied {
            new_revision: revision,
        },
        1 => SessionMutationResult::Replayed {
            new_revision: revision,
        },
        2 => SessionMutationResult::Conflict {
            current_revision: revision,
        },
        3 => SessionMutationResult::IdempotencyMismatch,
        _ => unreachable!(),
    };
    let actual = snapshot_commit_receipt(receipt);
    // Independent closed decision table: accepting the candidate requires an
    // Applied receipt; replay retains its receipt revision without authorizing
    // that candidate or substituting a later aggregate revision.
    match kind {
        0 => assert_eq!(actual, Ok((revision, true))),
        1 => assert_eq!(actual, Ok((revision, false))),
        2 => assert_eq!(actual, Err(SessionMutationError::Conflict)),
        3 => assert_eq!(actual, Err(SessionMutationError::IdempotencyMismatch)),
        _ => unreachable!(),
    }
}

#[cfg(kani)]
/// @obligation session_time.root_observations_are_revision_bound_and_replay_stable
#[kani::proof]
fn session_root_observation_is_total_monotonic_and_idempotent() {
    // Causes C1 previous unknown/known; C2 clock lower/equal/higher; C3 retry or
    // subsequent arbitrary clock. Effects E1 unknown becomes exact now; E2 a
    // known value is the least upper bound; E3 repeat is inert, later never
    // regresses. R1=!C1=>E1; R2=C1+C2=>E2; R3=C3=>E3. All Option/u64 values,
    // including zero and MAX, are symbolic; no time bound or arithmetic stub.
    check_observation(kani::any(), kani::any(), kani::any());
}

#[cfg(kani)]
/// @obligation session_time.root_observations_are_revision_bound_and_replay_stable
#[kani::proof]
fn session_root_receipt_selects_candidate_only_for_applied_revision() {
    // C1 actual repository receipt variant; C2 arbitrary revision. Effects:
    // R1 Applied=>publish that exact revision; R2 Replayed=>retain the original
    // receipt revision and reload durable root, without accepting a candidate;
    // R3 Conflict=>typed conflict; R4 mismatch=>typed mismatch. C2 cannot turn
    // R2-R4 into candidate acceptance. SQL/async wiring is tested separately.
    let kind: u8 = kani::any();
    kani::assume(kind < 4);
    check_receipt(kind, kani::any());
}

#[cfg(test)]
#[test]
fn root_observation_and_receipt_decision_tables() {
    // Same R1-R3 causes/effects as the symbolic observation proof, instantiated
    // at None/0/1/MAX-1/MAX to cross-check native Option and boundary behavior.
    // The Cartesian product covers both clock orderings, equality and retries.
    for old in [None, Some(0), Some(1), Some(u64::MAX - 1), Some(u64::MAX)] {
        for now in [0, 1, u64::MAX - 1, u64::MAX] {
            for later in [0, 1, u64::MAX - 1, u64::MAX] {
                check_observation(old, now, later);
            }
        }
    }
    // Receipt R1-R4 crossed with zero/nonzero/exhausted revision. This is
    // interpretation, not admission: the repository owns revision validity.
    for kind in 0..4 {
        for revision in [0, 1, u64::MAX] {
            check_receipt(kind, revision);
        }
    }
}
