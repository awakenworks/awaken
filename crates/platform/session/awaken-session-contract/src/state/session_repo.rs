// @formal entrypoint session.delete_intent_is_hidden_terminal_and_idempotent
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal entrypoint session.delete_tombstone_requires_hidden_terminal_verified_cleanup
// @formal entrypoint session.owner_saved_atomically
// @formal entrypoint session_event_batch.initial_plan_and_wake_activity_are_atomic_with_root
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
//! Persistence for the adapter-side Managed session aggregate.
//!
//! The runtime's committed transcript (ADR-0039) is durable, but the wire
//! `Session` object carries configuration that is NOT in the transcript — the
//! bound agent, the resolved model, the title/metadata, and the accepted MCP
//! servers. Without persisting it, a session rehydrated after a restart (or first
//! seen by another process sharing the store) reports placeholder defaults
//! (`agent = "assistant"`, empty `mcp_servers`, no title). This port stores that
//! aggregate so rehydration restores the real values.
//!
//! Secrets never cross this port. MCP and Repository entries may persist an
//! exact secret-free credential access/holder pin, but never credential material;
//! realization consumes that pin through the common exact resolver without
//! selecting another source or revision.

use crate::state::{SessionRealizationLease, SessionRevision};
use std::collections::{BTreeMap, BTreeSet};

mod activity_state;
pub use activity_state::{
    SessionActivitySettlementPlan, SessionRunActivityAdmissionRecord,
    session_activity_settlement_plan, validate_complete_run_activity,
};
mod disposition;
#[cfg(test)]
mod event_history_tests;
mod mutation;
mod persisted_session;
mod placement;
#[cfg(test)]
mod private_binding_tests;
mod recovery;
mod repository_publication;
mod runtime_intervals;

use disposition::{SessionDeleteDispositionClass, session_delete_request_plan};
pub use disposition::{SessionDisposition, SessionDispositionTransitionError};
use mutation::session_tombstone_is_admitted;
pub use mutation::{
    IdempotencyRecord, ScopedPersistedSession, SessionCommandReceiptKind, SessionCreateResult,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionMutationValidationError, SessionRepositoryConflict, SessionRepositoryError,
    SessionTombstone,
};
pub use persisted_session::{
    PersistedSession, SessionEventDispatchSettlementEffect,
    session_can_adopt_mcp_credential_lifecycle, session_is_terminal,
    session_is_unattempted_profiled_preparation,
};
pub use placement::{
    SessionPlacementClass, SessionPublicStatus, session_budget_pause_absorbs_interrupt,
    session_budget_pause_may_absorb_interrupt, session_is_budget_paused,
};
pub use recovery::{
    SessionRepositoryRecoveryAction, SessionScan, SessionScanCursor, SessionScanQuery,
};
pub use repository_publication::{
    SessionArchiveWithRepositoryPublicationError, SessionWorkerObligation,
};

#[cfg(test)]
use crate::state::ManagedLifecycleFact;

mod execution_state;
pub use execution_state::{
    SessionExecutionState, SessionExecutionStateError, SessionExecutionTransitionError,
};

#[cfg(test)]
mod mutation_tests;

#[cfg(kani)]
mod verification {
    use super::{SessionDeleteDispositionClass, session_delete_request_plan};

    /// @obligation session.delete_intent_is_hidden_terminal_and_idempotent
    /// @obligation session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
    #[kani::proof]
    fn session_delete_request_plan_is_exact_hidden_terminal_and_idempotent() {
        // Cause/effect table: C1 disposition is Active, Archived, Deleting, or
        // Deleted; C2 execution is terminal/nonterminal; C3 request time is any
        // i64. Effects: E1 Active/Archived select one Commit carrying the exact
        // terminalize decision, canonical cleanup identity, and one canonical
        // lifecycle fact; E2 Deleting/Deleted select Replay, which carries none
        // of those effects. Kani covers all 4x2 disposition/execution rows with
        // arbitrary C3. Repository CAS, fact insertion, delivery ACK, and async
        // cleanup remain outside this aggregate-plan proof.
        let disposition_code = kani::any::<u8>();
        kani::assume(disposition_code < 4);
        let disposition = match disposition_code {
            0 => SessionDeleteDispositionClass::Active,
            1 => SessionDeleteDispositionClass::Archived,
            2 => SessionDeleteDispositionClass::Deleting,
            3 => SessionDeleteDispositionClass::Deleted,
            _ => unreachable!(),
        };
        let execution_terminal = kani::any::<bool>();
        let requested_at_unix_s = kani::any::<i64>();
        let session_id = crate::state::SessionId::from("session-1");
        let owner_scope = "workspace-1";
        let plan = session_delete_request_plan(
            disposition,
            execution_terminal,
            &session_id,
            owner_scope,
            requested_at_unix_s,
        );
        let first_request = disposition_code < 2;
        assert_eq!(plan.transitioned(), first_request);
        match plan.commit_parts() {
            Some((terminalize_execution, cleanup_identity)) => {
                assert_eq!(terminalize_execution, !execution_terminal);
                assert!(std::ptr::eq(cleanup_identity.session_id(), &session_id));
                let lifecycle_fact = plan.lifecycle_fact_plan().unwrap();
                assert!(std::ptr::eq(lifecycle_fact.session_id, &session_id));
                assert!(std::ptr::eq(lifecycle_fact.owner_scope, owner_scope));
                assert_eq!(lifecycle_fact.requested_at_unix_s, requested_at_unix_s);
            }
            None => {
                assert!(!first_request);
                assert!(plan.lifecycle_fact_plan().is_none());
            }
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::execution_state::exhaustive::EVERY_EXECUTION_STATE;
    use super::{SessionExecutionState, session_tombstone_is_admitted};

    const BOOLS: [bool; 2] = [false, true];

    /// @exhaustive {ActivationFailed,Terminated}(2) x SessionExecutionState(7) = 14
    /// @obligation session_execution.terminal_state_never_reopens
    #[test]
    fn terminal_execution_never_reopens() {
        // Causes: the durable execution state is either terminal variant and
        // the requested successor is any of the seven production variants.
        // Effect: an admitted edge is necessarily an idempotent replay of the
        // same terminal state. T1=same terminal state -> admit without
        // reopening; T2=other terminal or nonterminal state -> reject.
        let mut cases = 0;
        for terminal in [
            SessionExecutionState::ActivationFailed,
            SessionExecutionState::Terminated,
        ] {
            for next in EVERY_EXECUTION_STATE {
                if terminal.can_transition_to(next) {
                    assert_eq!(terminal, next);
                }
                cases += 1;
            }
        }
        assert_eq!(cases, 14);
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) x bool(2) x bool(2) = 64
    /// @obligation session.delete_tombstone_requires_hidden_terminal_verified_cleanup
    #[test]
    fn session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup() {
        let mut cases = 0;
        for disposition_hidden in BOOLS {
            for execution_terminal in BOOLS {
                for cleanup_completed in BOOLS {
                    for event_batches_complete in BOOLS {
                        for session_identity_exact in BOOLS {
                            for next_revision_exact in BOOLS {
                                assert_eq!(
                                    session_tombstone_is_admitted(
                                        disposition_hidden,
                                        execution_terminal,
                                        cleanup_completed,
                                        event_batches_complete,
                                        session_identity_exact,
                                        next_revision_exact,
                                    ),
                                    disposition_hidden
                                        && execution_terminal
                                        && cleanup_completed
                                        && event_batches_complete
                                        && session_identity_exact
                                        && next_revision_exact
                                );
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 64);
    }
}

pub(crate) use mutation::validate_aggregate_state;

#[cfg(test)]
mod assignment_begin_tests;
#[cfg(test)]
mod controlled_aggregate_tests;
#[cfg(test)]
mod environment_facts_tests;
