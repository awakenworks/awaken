//! Durable terminal Session transitions.

use awaken_session_contract::{ManagedLifecycleFact, PersistedSession, SessionDisposition};

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError,
    activity::{now_unix_ms, runtime_interval_fact},
    mutation::repository_failure,
    resource_reconciliation::mutation_failure,
};

#[derive(Clone, Debug)]
pub struct SessionDispositionMutation {
    pub owner_scope: String,
    pub session: PersistedSession,
    pub transitioned: bool,
}

impl SessionApplication {
    /// Commit one terminal Session transition and release every remaining
    /// Resource exactly once. Public protocols may add wire projections after
    /// this application boundary, but no caller reproduces cleanup ordering.
    pub async fn terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let transition = self
            .begin_archive(session_id, archived_at, fact)
            .await
            .map_err(mutation_failure)?;
        let release_required = transition.transitioned
            || transition.session.resources.pending.is_some()
            || transition
                .session
                .resources
                .activations
                .iter()
                .any(|activation| {
                    activation.state == awaken_session_contract::ActivationState::Active
                });
        if release_required {
            self.release_terminal_resources(&transition.owner_scope, session_id)
                .await?;
        }
        if transition.transitioned {
            self.notify_lifecycle_fact();
        }
        Ok(transition)
    }

    /// Commit the archive fact once. [`Self::terminate_session`] follows this
    /// durable fence with idempotent cleanup; recovery may call the cleanup phase
    /// again without writing another terminal fact.
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
            let mut facts = session
                .close_runtime_interval(now_unix_ms())
                .map(|interval| runtime_interval_fact(&owner_scope, session_id, interval))
                .into_iter()
                .collect::<Vec<_>>();
            facts.push(fact.clone());
            match self
                .commit_session_snapshot(&owner_scope, session, "archive", facts)
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
            let mut facts = session
                .close_runtime_interval(now_unix_ms())
                .map(|interval| runtime_interval_fact(&owner_scope, session_id, interval))
                .into_iter()
                .collect::<Vec<_>>();
            facts.push(fact.clone());
            if session.resources.pending.is_none() {
                session
                    .resources
                    .begin_release()
                    .map_err(super::resource_reconciliation::internal)?;
            }
            match self
                .commit_session_snapshot(&owner_scope, session, "delete-intent", facts)
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
