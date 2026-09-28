//! Prefix-stable entrypoint for the warm/cold committed projection.

use super::*;
use crate::state::constants::COMMITTED_PREFIX_READ_CONFLICT;

const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

pub(super) const fn logical_deletion_is_proven(
    root_present: bool,
    id_matches: bool,
    owner_matches: bool,
    hidden: bool,
    terminal: bool,
    scoped_tombstone_present: bool,
) -> bool {
    if root_present {
        id_matches && owner_matches && hidden && terminal
    } else {
        scoped_tombstone_present
    }
}

#[cfg(kani)]
/// @obligation session.runtime_projection_logical_deletion_requires_positive_exact_scoped_evidence
/// @kani profile=solver-kissat-v1
#[kani::proof]
fn logical_deletion_observation_requires_positive_exact_scoped_evidence() {
    // Direct production read-decision over all 64 Boolean rows. Repository
    // decoding/owner-partition honesty and async consistency are explicit TCB.
    let present: bool = kani::any();
    let id: bool = kani::any();
    let owner: bool = kani::any();
    let hidden: bool = kani::any();
    let terminal: bool = kani::any();
    let tombstone: bool = kani::any();
    let proven = logical_deletion_is_proven(present, id, owner, hidden, terminal, tombstone);
    assert_eq!(
        proven,
        (present && id && owner && hidden && terminal) || (!present && tombstone)
    );
    if present && (!id || !owner || !hidden || !terminal) {
        assert!(!proven);
    }
    if !present && !tombstone {
        assert!(!proven);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectionObservationDecision {
    UseRefreshed,
    ServePrior,
    Retry,
}

pub(super) const fn projection_observation_decision(
    prior_complete: bool,
    refresh_complete: bool,
) -> ProjectionObservationDecision {
    if refresh_complete {
        ProjectionObservationDecision::UseRefreshed
    } else if prior_complete {
        ProjectionObservationDecision::ServePrior
    } else {
        ProjectionObservationDecision::Retry
    }
}

impl ManagedState {
    /// An open stream retains its already-authorized owner. Hidden terminal
    /// roots and owner-scoped durable tombstones are positive logical deletion
    /// evidence; a missing row, wrong owner, error or timeout is not evidence.
    pub(crate) async fn observe_committed_session_deletion(
        &self,
        session_id: &str,
        owner_scope: &str,
    ) -> Result<Option<Event>, StateError> {
        let read = async {
            let repository = self.application.session_repository_handle();
            let id = awaken_session_types::SessionId::from(session_id);
            let deleted = match repository.get(&id).await {
                Ok(root) => {
                    let id_matches = root.session_id == session_id;
                    let hidden = root.is_hidden();
                    let terminal = root.is_terminal();
                    let owner_matches = if id_matches && hidden && terminal {
                        repository.owner(&id).await.map_err(|error| {
                            StateError::Run(RunError::unavailable(error.to_string()))
                        })? == owner_scope
                    } else {
                        false
                    };
                    logical_deletion_is_proven(
                        true,
                        id_matches,
                        owner_matches,
                        hidden,
                        terminal,
                        false,
                    )
                }
                Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => {
                    let tombstone = repository
                        .deleted_event_admissions(owner_scope, &id)
                        .await
                        .map_err(|error| StateError::Run(RunError::unavailable(error.to_string())))?
                        .is_some();
                    logical_deletion_is_proven(false, false, false, false, false, tombstone)
                }
                Err(error) => {
                    return Err(StateError::Run(RunError::unavailable(error.to_string())));
                }
            };
            Ok(deleted.then(|| session_deletion_event(owner_scope, session_id)))
        };
        tokio::time::timeout(READ_BUDGET, read)
            .await
            .map_err(|_| Self::committed_prefix_read_conflict(session_id, 1))?
    }

    /// Refresh this process's disposable event projection from the Runtime's one
    /// durable transcript and Run lifecycle feed. A Session cache hit is not proof
    /// that it contains commits accepted through another protocol or Coordinator.
    pub(crate) async fn refresh_committed_events(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        self.ensure_session_record(session_id).await?;
        self.refresh_committed_projection(session_id).await
    }

    /// Refresh one read-only Event/Thread observation without requiring a quiet
    /// writer interval. Once this process has published one complete committed
    /// prefix, a racing append may make the new private candidate retry while
    /// the prior published prefix remains coherent and safe to return. Cold
    /// reads still use the strict bounded retry loop; source errors and stalled
    /// reads remain failures rather than being hidden as stale success. A page
    /// cursor is also a minimum observation: a prior prefix that lacks it cannot
    /// decide that the cursor is invalid, since another replica may have issued it.
    pub(crate) async fn refresh_committed_events_for_observation(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<ProjectionObservationDecision, StateError> {
        self.ensure_session_record(session_id).await?;
        let (cursor, _) = awaken_agent_contract::page::normalize_history_page_request(cursor, None);
        let prior_complete = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|record| {
                record.committed_projection_ready
                    && cursor.is_none_or(|cursor| record.event_positions(cursor).is_some())
            });
        if !prior_complete {
            return self
                .refresh_committed_projection(session_id)
                .await
                .map(|()| ProjectionObservationDecision::UseRefreshed);
        }

        let refresh = tokio::time::timeout(
            READ_BUDGET,
            self.refresh_committed_projection_once(session_id),
        )
        .await;
        let refresh_complete = match refresh {
            Ok(result) => result?,
            Err(_) => return Err(Self::committed_prefix_read_conflict(session_id, 1)),
        };
        match projection_observation_decision(prior_complete, refresh_complete) {
            ProjectionObservationDecision::UseRefreshed => {
                Ok(ProjectionObservationDecision::UseRefreshed)
            }
            ProjectionObservationDecision::ServePrior => {
                tracing::debug!(
                    session_id,
                    "Managed observation served the prior committed projection"
                );
                Ok(ProjectionObservationDecision::ServePrior)
            }
            ProjectionObservationDecision::Retry => {
                Err(Self::committed_prefix_read_conflict(session_id, 1))
            }
        }
    }

    /// Rehydration and ordinary reads share this seqlock-style retry. A changed
    /// Session or committed Thread prefix discards the mixed read instead of
    /// publishing a projection assembled from incomparable source versions.
    pub(in crate::state) async fn refresh_committed_projection(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        // One ordinary turn may commit more than four source versions while a
        // reader is in flight. Bound elapsed read work, not the number of valid
        // concurrent commits. Every retry still rebuilds and validates the full
        // candidate; the delay gives the writer time without a database hot loop.
        let mut attempts = 0_u32;
        let refreshed = tokio::time::timeout(READ_BUDGET, async {
            loop {
                attempts += 1;
                if self.refresh_committed_projection_once(session_id).await? {
                    return Ok(());
                }
                tokio::time::sleep(RETRY_DELAY).await;
            }
        })
        .await;
        if let Ok(result) = refreshed {
            return result;
        }
        Err(Self::committed_prefix_read_conflict(session_id, attempts))
    }

    fn committed_prefix_read_conflict(session_id: &str, attempts: u32) -> StateError {
        tracing::warn!(
            session_id,
            attempts,
            "Managed projection read budget exhausted"
        );
        StateError::Run(RunError::unavailable(COMMITTED_PREFIX_READ_CONFLICT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deletion_observation_requires_positive_owned_truth_not_bare_absence() {
        // Causes: D7 row absent with no tombstone; D8 current visible root;
        // D9 exact hidden terminal root after the real delete-intent CAS;
        // D10 only the asserted owner changes. Effects: D7/D8/D10 => no
        // deletion frame; D9 => the same canonical producer frame, no invented
        // timestamp. The retained root deliberately stays before physical
        // cleanup: this proves logical deletion, never remote quiescence.
        let state = ManagedState::new(crate::state::test_support::RehydrateFake::default())
            .with_session_repo(Arc::new(
                crate::state::test_support::ephemeral_session_repo(),
            ));
        let owner = awaken_tenancy::DEFAULT_WORKSPACE_ID;
        assert!(
            state
                .observe_committed_session_deletion("never-created", owner)
                .await
                .unwrap()
                .is_none(),
            "D7"
        );
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                owner,
            )
            .await
            .unwrap();
        assert!(
            state
                .observe_committed_session_deletion(&session.id, owner)
                .await
                .unwrap()
                .is_none(),
            "D8"
        );
        state
            .application
            .commit_delete_intent(awaken_session_application::SessionDeleteCommand::new(
                &session.id,
            ))
            .await
            .unwrap();
        let event = state
            .observe_committed_session_deletion(&session.id, owner)
            .await
            .unwrap()
            .expect("D9");
        assert_eq!(
            event,
            session_deletion_event(owner, &session.id),
            "D9 exact deletion projection"
        );
        assert!(
            state
                .observe_committed_session_deletion(&session.id, "foreign-owner")
                .await
                .unwrap()
                .is_none(),
            "D10"
        );
    }

    #[test]
    fn observation_fallback_decision_table_is_closed() {
        // C1 a prior complete prefix is absent/present; C2 this refresh
        // retries/completes. E1 completion always publishes fresh; E2 only a
        // complete prior prefix permits stale observation; E3 cold retry stays
        // unavailable. All four Boolean rows are explicit.
        assert_eq!(
            projection_observation_decision(false, false),
            ProjectionObservationDecision::Retry,
            "R1/E3"
        );
        assert_eq!(
            projection_observation_decision(true, false),
            ProjectionObservationDecision::ServePrior,
            "R2/E2"
        );
        for prior in [false, true] {
            assert_eq!(
                projection_observation_decision(prior, true),
                ProjectionObservationDecision::UseRefreshed,
                "R3-R4/E1"
            );
        }
    }
}
