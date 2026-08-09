//! Durable terminal Session transitions.

use awaken_session_contract::{ManagedLifecycleFact, PersistedSession, SessionLifecycleState};

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError,
    resource_reconciliation::mutation_failure,
};

#[derive(Clone, Debug)]
pub struct SessionLifecycleTransition {
    pub owner_scope: String,
    pub session: PersistedSession,
    pub transitioned: bool,
}

impl SessionApplication {
    /// Commit the archive fact once. Cleanup is a separate idempotent application
    /// phase so an interface can publish the committed terminal wire frame before
    /// potentially slow external teardown.
    pub async fn begin_archive(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionLifecycleTransition, SessionMutationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .ok_or(SessionMutationError::NotFound)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .ok_or(SessionMutationError::NotFound)?;
            if session.lifecycle == SessionLifecycleState::Terminated {
                return Ok(SessionLifecycleTransition {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            if session.is_terminal() {
                return Err(SessionMutationError::NotFound);
            }
            session
                .transition_lifecycle(SessionLifecycleState::Terminated)
                .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
            session.archived_at = Some(archived_at.into());
            match self
                .commit_session_snapshot(&owner_scope, session, "archive", vec![fact.clone()])
                .await
            {
                Ok(session) => {
                    return Ok(SessionLifecycleTransition {
                        owner_scope,
                        session,
                        transitioned: true,
                    });
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(SessionMutationError::Conflict)
    }

    /// Commit the hidden delete intent and Resource release intent atomically.
    pub async fn begin_delete(
        &self,
        session_id: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionLifecycleTransition, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .ok_or(SessionPreparationError::NotFound)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .ok_or(SessionPreparationError::NotFound)?;
            if session.lifecycle == SessionLifecycleState::Deleted {
                return Ok(SessionLifecycleTransition {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            if session.is_terminal() {
                return Err(SessionPreparationError::NotFound);
            }
            session
                .transition_lifecycle(SessionLifecycleState::Deleted)
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?;
            if session.resources.pending.is_none() {
                session
                    .resources
                    .begin_release()
                    .map_err(super::resource_reconciliation::internal)?;
            }
            match self
                .commit_session_snapshot(&owner_scope, session, "delete-intent", vec![fact.clone()])
                .await
            {
                Ok(session) => {
                    return Ok(SessionLifecycleTransition {
                        owner_scope,
                        session,
                        transitioned: true,
                    });
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }
}
