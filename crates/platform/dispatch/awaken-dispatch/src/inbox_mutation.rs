// @formal entrypoint ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit
//! One pending-input CAS plan, applied by each adapter inside its existing critical section.

use crate::CasOutcome;

/// The two revision-fenced commands owned by the pending-input aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum PendingInputMutation {
    Edit,
    Retract,
}

/// Complete mutation plan applied only under the adapter's existing lock/tx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingInputMutationPlan {
    Unchanged(CasOutcome),
    Edit { next_revision: u64 },
    Retract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("pending input revision cannot advance within the backend's exact integer domain")]
pub(crate) struct PendingInputRevisionExhausted;

/// One CAS and nonwrapping-successor law for Memory, SQLite and PostgreSQL.
/// `current` is the exact row read while holding the mutation critical section;
/// the adapter supplies its integer ceiling, never a caller-authored default.
pub(crate) fn plan_pending_input_mutation(
    current: Option<u64>,
    expected: u64,
    mutation: PendingInputMutation,
    ceiling: u64,
) -> Result<PendingInputMutationPlan, PendingInputRevisionExhausted> {
    let Some(current) = current else {
        return Ok(PendingInputMutationPlan::Unchanged(CasOutcome::NotFound));
    };
    if current != expected {
        return Ok(PendingInputMutationPlan::Unchanged(
            CasOutcome::RevisionMismatch,
        ));
    }
    match mutation {
        PendingInputMutation::Retract => Ok(PendingInputMutationPlan::Retract),
        PendingInputMutation::Edit => current
            .checked_add(1)
            .filter(|revision| *revision <= ceiling)
            .map(|next_revision| PendingInputMutationPlan::Edit { next_revision })
            .ok_or(PendingInputRevisionExhausted),
    }
}

#[cfg(test)]
mod pending_mutation_tests {
    use super::*;

    #[test]
    fn cas_plan_prioritizes_presence_identity_and_exact_integer_successor() {
        // Causes: C1 row absent/present; C2 expected revision matches; C3 edit
        // or retract; C4 successor fits the backend ceiling. Effects: R1 !C1
        // =>NotFound; R2 C1+!C2=>Mismatch; R3 C1+C2+retract=>remove even at MAX;
        // R4 C1+C2+edit+C4=>exact successor; R5 !C4=>reject before any mutation.
        use PendingInputMutation::{Edit, Retract};
        use PendingInputMutationPlan as P;
        for operation in [Edit, Retract] {
            assert_eq!(
                plan_pending_input_mutation(None, u64::MAX, operation, 0),
                Ok(P::Unchanged(CasOutcome::NotFound)),
                "R1"
            );
            assert_eq!(
                plan_pending_input_mutation(Some(1), 2, operation, 0),
                Ok(P::Unchanged(CasOutcome::RevisionMismatch)),
                "R2"
            );
        }
        assert_eq!(
            plan_pending_input_mutation(Some(u64::MAX), u64::MAX, Retract, 0),
            Ok(P::Retract),
            "R3"
        );
        for ceiling in [i64::MAX as u64, u64::MAX] {
            assert_eq!(
                plan_pending_input_mutation(Some(ceiling - 1), ceiling - 1, Edit, ceiling),
                Ok(P::Edit {
                    next_revision: ceiling
                }),
                "R4"
            );
            assert!(
                plan_pending_input_mutation(Some(ceiling), ceiling, Edit, ceiling).is_err(),
                "R5"
            );
        }
        assert!(
            plan_pending_input_mutation(Some(1), 1, Edit, 0).is_err(),
            "R5 invalid domain"
        );
    }
}

#[cfg(kani)]
/// @obligation ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit
/// @kani default-features=false
#[kani::proof]
fn pending_input_mutation_plan_is_exact_revision_fenced_and_nonwrapping() {
    // Direct production plan over the full Option/u64/operation domains. SQL
    // row locking, storage honesty and transaction effects are adapter/DB TCB.
    let current: Option<u64> = kani::any();
    let expected: u64 = kani::any();
    let ceiling: u64 = kani::any();
    let mutation: PendingInputMutation = kani::any();
    let plan = plan_pending_input_mutation(current, expected, mutation, ceiling);
    match plan {
        Ok(PendingInputMutationPlan::Edit { next_revision }) => {
            assert_eq!(current, Some(expected));
            assert_eq!(mutation, PendingInputMutation::Edit);
            assert_eq!(expected.checked_add(1), Some(next_revision));
            assert!(next_revision > expected && next_revision <= ceiling);
        }
        Ok(PendingInputMutationPlan::Retract) => {
            assert_eq!(current, Some(expected));
            assert_eq!(mutation, PendingInputMutation::Retract);
        }
        Ok(PendingInputMutationPlan::Unchanged(CasOutcome::NotFound)) => assert!(current.is_none()),
        Ok(PendingInputMutationPlan::Unchanged(CasOutcome::RevisionMismatch)) => {
            assert!(current.is_some_and(|revision| revision != expected))
        }
        Ok(PendingInputMutationPlan::Unchanged(CasOutcome::Applied)) => {
            panic!("unqualified effect plan")
        }
        Err(_) => {
            assert_eq!(current, Some(expected));
            assert_eq!(mutation, PendingInputMutation::Edit);
            assert!(expected.checked_add(1).is_none_or(|next| next > ceiling));
        }
    }
    if current == Some(expected) && mutation == PendingInputMutation::Retract {
        assert_eq!(plan, Ok(PendingInputMutationPlan::Retract));
    }
    if current == Some(expected)
        && mutation == PendingInputMutation::Edit
        && let Some(next_revision) = expected.checked_add(1).filter(|next| *next <= ceiling)
    {
        assert_eq!(plan, Ok(PendingInputMutationPlan::Edit { next_revision }));
    }
}
