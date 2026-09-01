//! Durable terminal Session transitions.

use awaken_provisioning_contract::RepositoryPublicationExpectation;
use awaken_resource_contract::{BindingId, ResourceAccess};
use awaken_session_contract::{
    ManagedLifecycleFact, PersistedSession, ResolvedInput, ResolvedInputSource, RunError,
    SessionDisposition, SessionExecutionState, SessionRepositoryPublicationIntent,
    SessionRepositoryPublicationReceipt,
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

/// Protocol-neutral selector for the one writable Repository binding whose
/// frozen Session input is to be published during archive. Repository identity,
/// configuration, and credentials remain owned by that existing input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRepositoryPublicationSelector {
    pub binding_id: BindingId,
    pub expectation: RepositoryPublicationExpectation,
}

/// Private profiled-release command. The asserted owner scope is checked on
/// every root-CAS attempt so an authenticated Workspace route cannot address a
/// Session through another Workspace's identifier.
#[derive(Clone, Debug)]
pub struct SessionArchiveWithRepositoryPublicationCommand {
    pub owner_scope: String,
    pub session_id: String,
    pub archived_at: String,
    pub lifecycle_fact: ManagedLifecycleFact,
    pub repository: SessionRepositoryPublicationSelector,
}

/// A release response is not successful until the exact Repository receipt is
/// durable in the Session root. The disposition mutation is retained so exact
/// response-loss replay exposes the same aggregate truth.
#[derive(Clone, Debug)]
pub struct SessionArchiveWithRepositoryPublicationOutcome {
    pub mutation: SessionDispositionMutation,
    pub publication_receipt: SessionRepositoryPublicationReceipt,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionArchiveWithRepositoryPublicationError {
    #[error("Session was not found in the asserted owner scope")]
    NotFound,
    #[error("Session Repository publication conflicts with durable state")]
    Conflict,
    #[error("Session Repository publication is durably pending: {0}")]
    Pending(String),
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session Repository publication is unavailable: {0}")]
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveAdmission {
    IdleOnly,
    ForceTerminal,
}

fn pending_event_batch_error() -> SessionPreparationError {
    SessionPreparationError::Rejected(RunError::unavailable_classified(
        "session_event_batch_pending",
        "Session Event batch effects must settle before a terminal transition",
    ))
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
    fn resolve_repository_publication_intent(
        session: &PersistedSession,
        selector: &SessionRepositoryPublicationSelector,
    ) -> Result<SessionRepositoryPublicationIntent, SessionArchiveWithRepositoryPublicationError>
    {
        if matches!(session.disposition, SessionDisposition::Archived { .. }) {
            let durable = session
                .terminal_cleanup
                .repository_publication_intent()
                .cloned()
                .ok_or(SessionArchiveWithRepositoryPublicationError::Conflict)?;
            if durable.input.binding_id != selector.binding_id
                || durable.expectation != selector.expectation
            {
                return Err(SessionArchiveWithRepositoryPublicationError::Conflict);
            }
            return Ok(durable);
        }

        let matching = session
            .resources
            .active
            .inputs()
            .iter()
            .filter(|input| input.binding_id == selector.binding_id)
            .cloned()
            .collect::<Vec<ResolvedInput>>();
        let [input] = matching.as_slice() else {
            return Err(SessionArchiveWithRepositoryPublicationError::Rejected(
                RunError::bad_request(
                    "Repository publication binding must select exactly one active Session input",
                ),
            ));
        };
        if input.access != ResourceAccess::ReadWrite
            || !matches!(input.source, ResolvedInputSource::Repository { .. })
        {
            return Err(SessionArchiveWithRepositoryPublicationError::Rejected(
                RunError::bad_request(
                    "Repository publication binding must select one writable Repository input",
                ),
            ));
        }
        let intent = SessionRepositoryPublicationIntent {
            input: input.clone(),
            expectation: selector.expectation.clone(),
        };
        intent.validate().map_err(|error| {
            SessionArchiveWithRepositoryPublicationError::Rejected(RunError::bad_request(
                error.to_string(),
            ))
        })?;
        Ok(intent)
    }

    /// Atomically archive one idle Session with an exact Repository publication
    /// intent, then eagerly drive the existing cleanup saga. A successful
    /// response is possible only after the canonical receipt is in the root;
    /// process loss is replayed from the same immutable cleanup operation.
    pub async fn archive_with_repository_publication(
        &self,
        command: SessionArchiveWithRepositoryPublicationCommand,
    ) -> Result<
        SessionArchiveWithRepositoryPublicationOutcome,
        SessionArchiveWithRepositoryPublicationError,
    > {
        let mut transition = None;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope =
                self.owner(&command.session_id)
                    .await
                    .map_err(|error| match error {
                        SessionMutationError::NotFound => {
                            SessionArchiveWithRepositoryPublicationError::NotFound
                        }
                        SessionMutationError::Conflict
                        | SessionMutationError::IdempotencyMismatch => {
                            SessionArchiveWithRepositoryPublicationError::Conflict
                        }
                        SessionMutationError::Unavailable(message) => {
                            SessionArchiveWithRepositoryPublicationError::Unavailable(message)
                        }
                    })?;
            if owner_scope != command.owner_scope {
                return Err(SessionArchiveWithRepositoryPublicationError::NotFound);
            }
            let mut session = self
                .session_repository()
                .get(&command.session_id)
                .await
                .map_err(|error| match repository_failure(error) {
                    SessionMutationError::NotFound => {
                        SessionArchiveWithRepositoryPublicationError::NotFound
                    }
                    SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
                        SessionArchiveWithRepositoryPublicationError::Conflict
                    }
                    SessionMutationError::Unavailable(message) => {
                        SessionArchiveWithRepositoryPublicationError::Unavailable(message)
                    }
                })?;
            if session.is_hidden() {
                return Err(SessionArchiveWithRepositoryPublicationError::NotFound);
            }
            if session.has_incomplete_event_batches() {
                self.wake_lifecycle_supervisor();
                return Err(SessionArchiveWithRepositoryPublicationError::Pending(
                    "Session Event batch effects must settle before Repository publication".into(),
                ));
            }
            if !matches!(session.disposition, SessionDisposition::Archived { .. })
                && session.execution != SessionExecutionState::Idle
            {
                return Err(SessionArchiveWithRepositoryPublicationError::Rejected(
                    RunError::bad_request(
                        "only an idle Session may be archived with Repository publication",
                    ),
                ));
            }
            let intent =
                Self::resolve_repository_publication_intent(&session, &command.repository)?;
            let changed = session
                .archive_with_repository_publication(&command.archived_at, intent)
                .map_err(|error| match error {
                    awaken_session_contract::SessionArchiveWithRepositoryPublicationError::Disposition(_) => {
                        SessionArchiveWithRepositoryPublicationError::NotFound
                    }
                    awaken_session_contract::SessionArchiveWithRepositoryPublicationError::Cleanup(_) => {
                        SessionArchiveWithRepositoryPublicationError::Conflict
                    }
                })?;
            if !changed {
                transition = Some(SessionDispositionMutation {
                    owner_scope,
                    session,
                    transitioned: false,
                });
                break;
            }
            let mut facts = session
                .close_runtime_interval(now_unix_ms())
                .map(|interval| runtime_interval_fact(&owner_scope, &command.session_id, interval))
                .into_iter()
                .collect::<Vec<_>>();
            facts.push(command.lifecycle_fact.clone());
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "archive-with-repository-publication",
                    facts,
                )
                .await
            {
                Ok(session) => {
                    transition = Some(SessionDispositionMutation {
                        owner_scope,
                        session,
                        transitioned: true,
                    });
                    break;
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionArchiveWithRepositoryPublicationError::Conflict);
                }
                Err(SessionMutationError::NotFound) => {
                    return Err(SessionArchiveWithRepositoryPublicationError::NotFound);
                }
                Err(SessionMutationError::IdempotencyMismatch) => {
                    return Err(SessionArchiveWithRepositoryPublicationError::Conflict);
                }
                Err(SessionMutationError::Unavailable(message)) => {
                    return Err(SessionArchiveWithRepositoryPublicationError::Unavailable(
                        message,
                    ));
                }
            }
        }
        let mut transition =
            transition.ok_or(SessionArchiveWithRepositoryPublicationError::Conflict)?;
        if transition.transitioned {
            self.notify_lifecycle_fact();
        }
        self.wake_lifecycle_supervisor();
        let pending_reason = self
            .release_terminal_resources(&transition.owner_scope, &command.session_id)
            .await
            .err()
            .map(|error| error.to_string());
        let durable = self
            .session_repository()
            .get(&command.session_id)
            .await
            .map_err(|error| match repository_failure(error) {
                SessionMutationError::NotFound => {
                    SessionArchiveWithRepositoryPublicationError::NotFound
                }
                SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
                    SessionArchiveWithRepositoryPublicationError::Conflict
                }
                SessionMutationError::Unavailable(message) => {
                    SessionArchiveWithRepositoryPublicationError::Unavailable(message)
                }
            })?;
        transition.session = durable.clone();
        if let Some(rejection) = durable
            .terminal_cleanup
            .repository_publication_rejection(&command.session_id)
            .map_err(|_| SessionArchiveWithRepositoryPublicationError::Conflict)?
        {
            return Err(SessionArchiveWithRepositoryPublicationError::Rejected(
                RunError::bad_request_classified(
                    "repository_publication_rejected",
                    rejection.effect_rejection.to_string(),
                ),
            ));
        }
        let publication_receipt = durable
            .terminal_cleanup
            .repository_publication_receipt(&command.session_id)
            .map_err(|_| SessionArchiveWithRepositoryPublicationError::Conflict)?
            .cloned()
            .ok_or_else(|| {
                SessionArchiveWithRepositoryPublicationError::Pending(
                    pending_reason.unwrap_or_else(|| {
                        "Repository publication Worker receipt has not been recorded".into()
                    }),
                )
            })?;
        Ok(SessionArchiveWithRepositoryPublicationOutcome {
            mutation: transition,
            publication_receipt,
        })
    }

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
        if transition.transitioned {
            self.notify_lifecycle_fact();
        }
        // Archive is linearized by the durable terminal fence above. Physical
        // Runtime/Resource cleanup is a recoverable saga and must not turn that
        // committed business transition into a retryable command failure. Wake
        // the lifecycle owner before the eager attempt so caller cancellation
        // cannot strand the intent; the attempt only shortens convergence when
        // the current projector is immediately available.
        self.wake_lifecycle_supervisor();
        if let Err(error) = self
            .release_terminal_resources(&transition.owner_scope, session_id)
            .await
        {
            tracing::warn!(
                session = session_id,
                error = ?error,
                "Session archive cleanup remains pending for lifecycle recovery"
            );
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
        let admission_name = match admission {
            ArchiveAdmission::IdleOnly => "idle_only",
            ArchiveAdmission::ForceTerminal => "force_terminal",
        };
        let command_hash = awaken_session_contract::stable_fingerprint(&(
            "session-archive-v1",
            session_id,
            archived_at,
            &fact,
            admission_name,
        ));
        let command_record = awaken_session_contract::IdempotencyRecord {
            key: format!("session:archive:{session_id}:{}", fact.id),
            payload_hash: command_hash,
        };
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(mutation_failure)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?;
            // Terminal disposition is absorbing and therefore precedes command
            // payload validation. A later retry may carry a different display
            // timestamp, but it cannot reopen or rewrite the archived root.
            if matches!(session.disposition, SessionDisposition::Archived { .. }) {
                return Ok(SessionDispositionMutation {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            if let Some(receipt) = self
                .session_repository()
                .idempotency_receipt(session_id, &command_record.key)
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?
            {
                if receipt.payload_hash != command_record.payload_hash {
                    return Err(SessionPreparationError::Unavailable(
                        "Session archive command identity was reused with another payload".into(),
                    ));
                }
                return Ok(SessionDispositionMutation {
                    owner_scope,
                    session,
                    transitioned: false,
                });
            }
            if session.is_hidden() {
                return Err(SessionPreparationError::NotFound);
            }
            if session.has_incomplete_event_batches() {
                // Admission itself owns no activity epoch, so Idle does not
                // imply that every accepted Event effect has crossed its
                // authoritative Runtime boundary. Keep the root nonterminal
                // and let the existing lifecycle supervisor finish the batch;
                // otherwise terminal state would make that work undiscoverable.
                self.wake_lifecycle_supervisor();
                return Err(pending_event_batch_error());
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
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    command_record.clone(),
                    facts,
                )
                .await
            {
                Ok((session, transitioned)) => {
                    return Ok(SessionDispositionMutation {
                        owner_scope,
                        session,
                        transitioned,
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
            if session.has_incomplete_event_batches() {
                self.wake_lifecycle_supervisor();
                return Err(pending_event_batch_error());
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
