//! Durable terminal Session transitions.

use awaken_session_contract::{ManagedLifecycleFact, PersistedSession, SessionDisposition};

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError,
    mutation::repository_failure, resource_reconciliation::mutation_failure,
};

#[derive(Clone, Debug)]
pub struct SessionDispositionMutation {
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
    ) -> Result<SessionDispositionMutation, SessionMutationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)?;
            if matches!(session.disposition, SessionDisposition::Archived { .. }) {
                return Ok(SessionDispositionMutation {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            if session.is_hidden() {
                return Err(SessionMutationError::NotFound);
            }
            session
                .archive(archived_at)
                .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
            match self
                .commit_session_snapshot(&owner_scope, session, "archive", vec![fact.clone()])
                .await
            {
                Ok(session) => {
                    return Ok(SessionDispositionMutation {
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
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(mutation_failure)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?;
            if matches!(
                session.disposition,
                SessionDisposition::Deleting | SessionDisposition::Deleted
            ) {
                return Ok(SessionDispositionMutation {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            session.request_delete();
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
                    return Ok(SessionDispositionMutation {
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
