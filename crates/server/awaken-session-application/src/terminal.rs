//! Durable terminal Session transitions.

use awaken_session_contract::{
    ManagedLifecycleFact, PersistedSession, RunError, SessionDisposition, SessionExecutionState,
};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveAdmission {
    IdleOnly,
    ForceTerminal,
}

/// Protocol-neutral request to commit the durable Session Delete fence.
/// Application code, not an edge adapter, derives the canonical lifecycle fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDeleteCommand {
    session_id: String,
    requested_at_unix_s: i64,
}

impl SessionDeleteCommand {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        let requested_at_unix_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Self {
            session_id: session_id.into(),
            requested_at_unix_s,
        }
    }

    fn lifecycle_fact(&self, owner_scope: &str) -> ManagedLifecycleFact {
        ManagedLifecycleFact {
            id: format!("session:{}:deleted", self.session_id),
            object_id: self.session_id.clone(),
            workspace_id: Some(owner_scope.to_string()),
            event_type: "session.deleted".into(),
            timestamp: self.requested_at_unix_s,
            runtime_interval: None,
        }
    }
}

impl SessionApplication {
    pub(crate) async fn retire_terminal_work(
        &self,
        session: &PersistedSession,
    ) -> Result<(), SessionPreparationError> {
        let Some(baseline) = session
            .frozen_baseline()
            .filter(|baseline| baseline.environment.self_hosted)
        else {
            return Ok(());
        };
        self.environments
            .retire_session_work(&baseline.environment.environment_id, &session.session_id)
            .await
            .map(|_| ())
            .map_err(|error| {
                SessionPreparationError::Rejected(
                    awaken_session_contract::RunError::unavailable_classified(
                        "session_work_retire_failed",
                        format!("Session Work could not be retired: {error}"),
                    ),
                )
            })
    }

    /// Archive one publicly idle Session and release every remaining Resource
    /// exactly once. The idle check and archive transition share the root CAS;
    /// protocol adapters must not race a separate status read against this
    /// command. Internal terminal jobs use [`Self::force_terminate_session`].
    pub async fn terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        self.terminate_session_with_admission(
            session_id,
            archived_at,
            fact,
            ArchiveAdmission::IdleOnly,
        )
        .await
    }

    /// Force a terminal Session fence for an internal lifecycle owner such as
    /// Dream cleanup. This deliberately bypasses public idle admission while
    /// retaining the same archive CAS and terminal cleanup implementation.
    pub async fn force_terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        self.terminate_session_with_admission(
            session_id,
            archived_at,
            fact,
            ArchiveAdmission::ForceTerminal,
        )
        .await
    }

    async fn terminate_session_with_admission(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
        admission: ArchiveAdmission,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let transition = self
            .commit_archive(session_id, archived_at, fact, admission)
            .await?;
        self.release_terminal_resources(&transition.owner_scope, session_id)
            .await?;
        if transition.transitioned {
            self.notify_lifecycle_fact();
        }
        Ok(transition)
    }

    /// Commit one Delete transition, start one eager reconciliation attempt, and
    /// wake the durable lifecycle driver for any retry. Cleanup is application
    /// owned and detached: the caller observes the committed delete fence and
    /// cannot cancel or join the external-effect lifecycle.
    pub async fn delete_session(
        self: &std::sync::Arc<Self>,
        command: SessionDeleteCommand,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let transition = self.commit_delete_intent(command).await?;
        self.notify_lifecycle_fact();
        self.wake_lifecycle_supervisor();
        let application = std::sync::Arc::clone(self);
        let owner_scope = transition.owner_scope.clone();
        let session_id = transition.session.session_id.clone();
        tokio::spawn(async move {
            if let Err(error) = application
                .release_terminal_resources(&owner_scope, &session_id)
                .await
            {
                tracing::warn!(
                    session = session_id,
                    error = ?error,
                    "Session delete cleanup remains pending for lifecycle recovery"
                );
            }
        });
        Ok(transition)
    }

    /// The sole archive mutation kernel. Admission is re-evaluated after every
    /// conflicting CAS, so an activity that wins the root revision cannot have
    /// its Running truth or completion epoch erased by a stale public archive.
    /// An exact Archived replay is absorbing and precedes admission validation.
    async fn commit_archive(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
        admission: ArchiveAdmission,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(mutation_failure)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?;
            if matches!(session.disposition, SessionDisposition::Archived { .. }) {
                return Ok(SessionDispositionMutation {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            if session.is_hidden() {
                return Err(SessionPreparationError::NotFound);
            }
            if admission == ArchiveAdmission::IdleOnly
                && session.execution != SessionExecutionState::Idle
            {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "only an idle Session may be archived",
                )));
            }
            session
                .archive(archived_at)
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?;
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
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    /// Commit the hidden delete intent and Resource release intent atomically.
    pub async fn commit_delete_intent(
        &self,
        command: SessionDeleteCommand,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let session_id = command.session_id.as_str();
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
            facts.push(command.lifecycle_fact(&owner_scope));
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
