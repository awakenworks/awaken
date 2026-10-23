// @formal entrypoint dream.public_terminal_requires_exact_session_and_artifact_cleanup_receipts
// @formal entrypoint session.delete_intent_is_hidden_terminal_and_idempotent
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
//! Durable terminal Session transitions.

use crate::mutation::ROOT_CAS_ATTEMPTS;
use awaken_provisioning_contract::RepositoryPublicationExpectation;
use awaken_resource_contract::{BindingId, ResourceAccess};
use awaken_session_configuration_contract::resource::{ResolvedInput, ResolvedInputSource};
use awaken_session_state_contract::{
    ManagedLifecycleFact, PersistedSession, SessionDisposition, SessionExecutionState,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
};
use awaken_session_types::RunError;
use awaken_time::WallClockExt as _;

use awaken_session_state_contract::{
    SessionArchiveAdmission as ArchiveAdmission,
    SessionArchiveBusinessIntent as ArchiveBusinessIntent,
    session_archive_idempotency_record as archive_idempotency_record,
};

use super::{
    SessionApplication, SessionDeleteCommand, SessionMutationError, SessionPreparationError,
    SnapshotCommit, credentials::mutation_failure, mutation::repository_failure,
};

/// One durable disposition transition and whether *this* call committed it.
///
/// `transitioned` is private and has exactly two doors, because it is not a
/// fact the command path may assert: [`Self::committed`] takes the sealed
/// [`SnapshotCommit`] the root CAS returned, so the bit is whatever the CAS
/// decided; [`Self::unchanged`] is for the paths that reach an absorbing
/// outcome *without* attempting a commit at all. Writing `transitioned: true`
/// beside a commit — the shape that made a concurrent archive replay report
/// itself as an application — is a compile error:
///
/// ```compile_fail,E0451
/// use awaken_session_application::SessionDispositionMutation;
///
/// fn fabricate(session: awaken_session_state_contract::PersistedSession) {
///     let _ = SessionDispositionMutation {
///         owner_scope: "workspace".into(),
///         session,
///         transitioned: true,
///     };
/// }
/// ```
#[derive(Clone, Debug)]
pub struct SessionDispositionMutation {
    pub owner_scope: String,
    pub session: PersistedSession,
    transitioned: bool,
}

impl SessionDispositionMutation {
    /// Report the outcome the root CAS actually reached. The bit can only come
    /// from the commit, so a replay that lost the CAS cannot be published as an
    /// application.
    #[must_use]
    pub fn committed(owner_scope: String, commit: SnapshotCommit) -> Self {
        Self {
            owner_scope,
            transitioned: commit.transitioned(),
            session: commit.into_session(),
        }
    }

    /// An absorbing outcome reached without attempting a root CAS: the
    /// disposition is already terminal, an exact command receipt is durable, or
    /// the aggregate refused to produce a transition. No commit happened, so
    /// there is no outcome to carry.
    #[must_use]
    pub fn unchanged(owner_scope: String, session: PersistedSession) -> Self {
        Self {
            owner_scope,
            session,
            transitioned: false,
        }
    }

    /// Whether this call committed the transition. `false` is an absorbing
    /// replay: the durable root already carried it.
    #[must_use]
    pub fn transitioned(&self) -> bool {
        self.transitioned
    }
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
    pub session_id: awaken_session_types::SessionId,
    pub archived_at: String,
    pub lifecycle_fact: ManagedLifecycleFact,
    pub repository: SessionRepositoryPublicationSelector,
}

impl SessionArchiveWithRepositoryPublicationCommand {
    /// One release request observed at `observed_at_ms`. Its intent (owner,
    /// Session, fact identity and Repository selector) is independent of the
    /// observation time; only the display time and fact timestamp follow it.
    #[must_use]
    pub fn new(
        owner_scope: String,
        session_id: awaken_session_types::SessionId,
        repository: SessionRepositoryPublicationSelector,
        observed_at_ms: u64,
    ) -> Self {
        let (archived_at, lifecycle_fact) = session_terminated_record(
            session_id.as_str(),
            Some(owner_scope.clone()),
            observed_at_ms,
        );
        Self {
            owner_scope,
            session_id,
            archived_at,
            lifecycle_fact,
            repository,
        }
    }
}

/// The display time and `session.terminated` lifecycle fact of one archive
/// command observed at `observed_at_ms`. The fact identity derives from the
/// Session alone, so independent requests converge on one receipt.
pub(crate) fn session_terminated_record(
    session_id: &str,
    owner_scope: Option<String>,
    observed_at_ms: u64,
) -> (String, ManagedLifecycleFact) {
    (
        awaken_session_application_contract::epoch_millis_to_rfc3339(observed_at_ms),
        ManagedLifecycleFact {
            id: awaken_session_event_contract::session_terminated_fact_id(session_id),
            object_id: session_id.to_string(),
            workspace_id: owner_scope,
            event_type: awaken_session_event_contract::SESSION_TERMINATED.to_string(),
            timestamp: i64::try_from(observed_at_ms / 1_000).unwrap_or(i64::MAX),
            runtime_interval: None,
        },
    )
}

/// The archive record of a command issued now by the Session's owner.
async fn archive_record_now(
    mutations: &crate::SessionMutations,
    clock: &dyn awaken_time::WallClock,
    session_id: &str,
) -> Result<(String, ManagedLifecycleFact), SessionPreparationError> {
    let owner_scope = mutations
        .owner(&awaken_session_types::SessionId::from(session_id))
        .await
        .map_err(mutation_failure)?;
    Ok(session_terminated_record(
        session_id,
        Some(owner_scope),
        clock.now_ms(),
    ))
}

/// The first archive admission a profiled release with verified cleanup
/// issues before joining terminal cleanup.
///
/// | Rule | Execution | First admission |
/// |---|---|---|
/// | F1 | terminal | none: join cleanup directly |
/// | F2 | Preparing or Running | profiled release |
/// | F3 | any other live state | idle-only archive |
const fn profiled_release_first_admission(
    execution: SessionExecutionState,
    terminal: bool,
) -> Option<ArchiveAdmission> {
    if terminal {
        None
    } else if matches!(
        execution,
        SessionExecutionState::Preparing | SessionExecutionState::Running
    ) {
        Some(ArchiveAdmission::ProfiledRelease)
    } else {
        Some(ArchiveAdmission::IdleOnly)
    }
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

impl From<SessionMutationError> for SessionArchiveWithRepositoryPublicationError {
    fn from(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
                Self::Conflict
            }
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }
}

pub(crate) fn pending_event_batch_error() -> SessionPreparationError {
    SessionPreparationError::Rejected(RunError::unavailable_classified(
        "session_event_batch_pending",
        "Session Event batch effects must settle before a terminal transition",
    ))
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
            return Err(RunError::bad_request(
                "Repository publication binding must select exactly one active Session input",
            )
            .into());
        };
        if input.access != ResourceAccess::ReadWrite
            || !matches!(input.source, ResolvedInputSource::Repository { .. })
        {
            return Err(RunError::bad_request(
                "Repository publication binding must select one writable Repository input",
            )
            .into());
        }
        let intent = SessionRepositoryPublicationIntent {
            input: input.clone(),
            expectation: selector.expectation.clone(),
        };
        intent
            .validate()
            .map_err(|error| RunError::bad_request(error.to_string()))?;
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
        let mutations = &self.mutations;
        let mut transition = None;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations.owner(&command.session_id).await?;
            if owner_scope != command.owner_scope {
                return Err(SessionArchiveWithRepositoryPublicationError::NotFound);
            }
            let mut session = mutations
                .session_aggregate(&command.session_id)
                .await
                .map_err(repository_failure)?;
            if session.is_hidden() {
                return Err(SessionArchiveWithRepositoryPublicationError::NotFound);
            }
            if session.has_incomplete_event_batches() {
                self.lifecycle.request_reconciliation();
                return Err(SessionArchiveWithRepositoryPublicationError::Pending(
                    "Session Event batch effects must settle before Repository publication".into(),
                ));
            }
            if !matches!(session.disposition, SessionDisposition::Archived { .. })
                && session.execution != SessionExecutionState::Idle
            {
                return Err(RunError::bad_request(
                    "only an idle Session may be archived with Repository publication",
                )
                .into());
            }
            let intent =
                Self::resolve_repository_publication_intent(&session, &command.repository)?;
            let idempotency = archive_idempotency_record(
                &owner_scope,
                &command.session_id,
                &command.lifecycle_fact,
                ArchiveBusinessIntent::RepositoryPublication {
                    source_revision: session.revision,
                    binding_id: &command.repository.binding_id,
                    expectation: &command.repository.expectation,
                    resolved: &intent,
                },
            );
            // An Archived replay must retain its first time without asking the
            // clock again. Only an Active transition receives a new sample.
            let archive_time = matches!(&session.disposition, SessionDisposition::Active)
                .then(|| self.clock.now_ms());
            let changed = session.lifecycle().archive_with_repository_publication(&command.archived_at, archive_time, intent)
                .map_err(|error| match error {
                    awaken_session_state_contract::SessionArchiveWithRepositoryPublicationError::Disposition(
                        awaken_session_state_contract::SessionDispositionTransitionError::ArchiveAfterDelete,
                    ) => {
                        SessionArchiveWithRepositoryPublicationError::NotFound
                    }
                    awaken_session_state_contract::SessionArchiveWithRepositoryPublicationError::Disposition(
                        awaken_session_state_contract::SessionDispositionTransitionError::ArchiveTimeRequired,
                    ) => SessionArchiveWithRepositoryPublicationError::Unavailable(
                        "Session archive has no Application time observation".into(),
                    ),
                    awaken_session_state_contract::SessionArchiveWithRepositoryPublicationError::Cleanup(_) => {
                        SessionArchiveWithRepositoryPublicationError::Conflict
                    }
                })?;
            if !changed {
                transition = Some(SessionDispositionMutation::unchanged(
                    owner_scope,
                    session.into_snapshot(),
                ));
                break;
            }
            let facts = vec![command.lifecycle_fact.clone()];
            match mutations
                .commit_session_snapshot_with_record(&owner_scope, session, idempotency, facts)
                .await
            {
                Ok(commit) => {
                    transition = Some(SessionDispositionMutation::committed(owner_scope, commit));
                    break;
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
        let mut transition =
            transition.ok_or(SessionArchiveWithRepositoryPublicationError::Conflict)?;
        if transition.transitioned {
            self.lifecycle.notify_fact();
        }
        self.lifecycle.request_reconciliation();
        let pending_reason = self
            .release_terminal_resources(&transition.owner_scope, command.session_id.as_str())
            .await
            .err()
            .map(|error| error.to_string());
        let durable = mutations
            .session_aggregate(&command.session_id)
            .await
            .map_err(repository_failure)?;
        transition.session = durable.snapshot().clone();
        if let Some(rejection) = durable
            .terminal_cleanup
            .repository_publication_rejection(&command.session_id)
            .map_err(|_| SessionArchiveWithRepositoryPublicationError::Conflict)?
        {
            return Err(RunError::bad_request_classified(
                if rejection.effect_rejection.is_source_closed() {
                    "repository_publication_source_closed"
                } else {
                    "repository_publication_rejected"
                },
                rejection.effect_rejection.to_string(),
            )
            .into());
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
            .assignment
            .retire_session(&baseline.environment.environment_id, &session.session_id)
            .await
            .map(|_| ())
            .map_err(|error| {
                SessionPreparationError::Rejected(
                    awaken_session_types::RunError::unavailable_classified(
                        "session_work_retire_failed",
                        format!("Session Work could not be retired: {error}"),
                    ),
                )
            })
    }

    /// Archive one publicly idle Session and release every remaining Resource
    /// exactly once. The idle check and archive transition share the root CAS;
    /// protocol adapters must not race a separate status read against this
    /// command. Already-terminal execution uses [`Self::release_terminal_session`];
    /// owners authorized to stop live execution use [`Self::force_terminate_session`].
    pub async fn terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        self.terminate_session_with_admission(
            &awaken_session_types::SessionId::from(session_id),
            archived_at,
            fact,
            ArchiveAdmission::IdleOnly,
        )
        .await
    }

    /// Archive one publicly idle Session now: the command records the current
    /// application time and the owner's `session.terminated` fact.
    pub async fn archive_session(
        &self,
        session_id: &str,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let (archived_at, fact) =
            archive_record_now(&self.mutations, self.clock.as_ref(), session_id).await?;
        self.terminate_session(session_id, &archived_at, fact).await
    }

    /// Release a profiled Session through verified cleanup: a live Session is
    /// first archived under [`profiled_release_first_admission`], then the
    /// terminal Session joins its canonical cleanup receipt. Each command
    /// rereads and rechecks the durable root.
    pub async fn release_profiled_session_with_verified_cleanup(
        &self,
        session_id: &str,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let (archived_at, fact) =
            archive_record_now(&self.mutations, self.clock.as_ref(), session_id).await?;
        let id = awaken_session_types::SessionId::from(session_id);
        let root = self
            .mutations
            .session_aggregate(&id)
            .await
            .map_err(repository_failure)
            .map_err(mutation_failure)?;
        if let Some(admission) =
            profiled_release_first_admission(root.execution, root.is_terminal())
        {
            self.terminate_session_with_admission(&id, &archived_at, fact.clone(), admission)
                .await?;
        }
        self.release_terminal_session(session_id, &archived_at, fact)
            .await
    }

    /// Force a terminal Session fence for an internal lifecycle owner such as
    /// Dream cleanup. This deliberately bypasses public idle admission while
    /// retaining the same archive CAS and terminal cleanup implementation. A
    /// successful response additionally requires the canonical terminal-cleanup
    /// receipt; callers such as Dream must not publish their own completion
    /// while a registered Worker still owns that external effect.
    pub async fn force_terminate_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        self.terminate_session_with_admission(
            &awaken_session_types::SessionId::from(session_id),
            archived_at,
            fact,
            ArchiveAdmission::ForceTerminal,
        )
        .await
    }

    /// Release an already-terminal Session through its existing cleanup owner.
    /// Unlike public idle archive or internal force termination, this command
    /// cannot terminate live execution. Terminal execution is absorbing, so a
    /// completed Run is not used to infer permission over a reusable Session.
    /// Success requires the canonical verified cleanup receipt.
    pub async fn release_terminal_session(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: ManagedLifecycleFact,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        self.terminate_session_with_admission(
            &awaken_session_types::SessionId::from(session_id),
            archived_at,
            fact,
            ArchiveAdmission::TerminalOnly,
        )
        .await
    }

    pub(crate) async fn terminate_session_with_admission(
        &self,
        session_id: &awaken_session_types::SessionId,
        archived_at: &str,
        fact: ManagedLifecycleFact,
        admission: ArchiveAdmission,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let transition = self
            .commit_archive(session_id, archived_at, fact, admission)
            .await?;
        if transition.transitioned {
            self.lifecycle.notify_fact();
        }
        // Archive is linearized by the durable terminal fence above. Physical
        // Runtime/Resource cleanup is a recoverable saga. Public archive returns
        // the committed transition; internal required-completion callers join
        // its verified cleanup below. Wake the lifecycle owner before the eager
        // attempt so caller cancellation cannot strand the intent; the attempt
        // only shortens convergence when the current projector is available.
        self.lifecycle.request_reconciliation();
        let cleanup = self
            .release_terminal_resources(&transition.owner_scope, session_id.as_str())
            .await;
        match (admission != ArchiveAdmission::IdleOnly, cleanup) {
            (true, Ok(Some(session))) => {
                if !session
                    .verified_terminal_cleanup()
                    .map_err(super::resource_reconciliation::internal)?
                    .is_completed()
                {
                    return Err(SessionPreparationError::Unavailable(
                        "Session archive cleanup remains pending".into(),
                    ));
                }
            }
            (true, Err(error)) => return Err(error),
            (false, Err(error)) => {
                tracing::warn!(
                    session = session_id.as_str(),
                    error = ?error,
                    "Session archive cleanup remains pending for lifecycle recovery"
                );
            }
            (true, Ok(None)) | (false, Ok(_)) => {}
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
        let transition = self.lifecycle.commit_delete_intent(command).await?;
        self.lifecycle.notify_fact();
        self.lifecycle.request_reconciliation();
        let application = std::sync::Arc::clone(self);
        let owner_scope = transition.owner_scope.clone();
        let session_id = transition.session.session_id.clone();
        tokio::spawn(async move {
            if let Err(error) = application
                .release_terminal_resources(&owner_scope, session_id.as_str())
                .await
            {
                tracing::warn!(
                    session = session_id.as_str(),
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
        session_id: &awaken_session_types::SessionId,
        archived_at: &str,
        fact: ManagedLifecycleFact,
        admission: ArchiveAdmission,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let mutations = &self.mutations;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(session_id)
                .await
                .map_err(mutation_failure)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?;
            // Terminal disposition is absorbing and therefore precedes command
            // payload validation. A later retry may carry a different display
            // timestamp, but it cannot reopen or rewrite the archived root.
            if matches!(session.disposition, SessionDisposition::Archived { .. }) {
                return Ok(SessionDispositionMutation::unchanged(
                    owner_scope,
                    session.into_snapshot(),
                ));
            }
            let admission = admission.resolve(session.execution);
            let command_record = archive_idempotency_record(
                &owner_scope,
                session_id,
                &fact,
                ArchiveBusinessIntent::Ordinary {
                    admission: admission.as_identity_str(),
                },
            );
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
                return Ok(SessionDispositionMutation::unchanged(
                    owner_scope,
                    session.into_snapshot(),
                ));
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
                self.lifecycle.request_reconciliation();
                return Err(pending_event_batch_error());
            }
            admission
                .validate_execution(&session)
                .map_err(SessionPreparationError::Rejected)?;
            if admission == ArchiveAdmission::FailedProfiledExecution {
                crate::run_events::validate_failed_profiled_execution(self, &session).await?;
            }
            let archive_time = self.clock.now_ms();
            session
                .lifecycle()
                .archive(archived_at, archive_time)
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?;
            let facts = vec![fact.clone()];
            match mutations
                .commit_session_snapshot_with_record(
                    &owner_scope,
                    session,
                    command_record.clone(),
                    facts,
                )
                .await
            {
                Ok(commit) => {
                    return Ok(SessionDispositionMutation::committed(owner_scope, commit));
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }
}

#[cfg(test)]
mod publication_error_tests {
    use super::*;

    #[test]
    fn canonical_mutation_error_mapping_retains_every_publication_outcome() {
        // DT-PUBLICATION-ERROR: owner or repository read/CAS reports C1 missing,
        // C2 conflict, C3 changed retry identity, or C4 unavailable. R1 -> missing;
        // R2/R3 -> conflict; R4 -> unavailable with the original message. Every
        // call site uses this conversion, while lost-root-CAS tests own retries.
        for (cause, expected) in [
            (
                SessionMutationError::NotFound,
                SessionArchiveWithRepositoryPublicationError::NotFound,
            ),
            (
                SessionMutationError::Conflict,
                SessionArchiveWithRepositoryPublicationError::Conflict,
            ),
            (
                SessionMutationError::IdempotencyMismatch,
                SessionArchiveWithRepositoryPublicationError::Conflict,
            ),
            (
                SessionMutationError::Unavailable("owner unavailable".into()),
                SessionArchiveWithRepositoryPublicationError::Unavailable(
                    "owner unavailable".into(),
                ),
            ),
        ] {
            let actual = SessionArchiveWithRepositoryPublicationError::from(cause);
            assert_eq!(
                std::mem::discriminant(&actual),
                std::mem::discriminant(&expected)
            );
            assert_eq!(actual.to_string(), expected.to_string());
        }
    }

    #[test]
    fn independent_managed_archives_share_intent_across_route_clock_samples() {
        // Causes: M1 two independent archive requests sample 1s/2s for the
        // same owned Session. Effects: M1 display and fact timestamp differ,
        // while Session/Workspace/fact id/type and interval semantics remain
        // identical. The lost-CAS table tests one receipt.
        let (first_time, first) =
            super::session_terminated_record("session", Some("workspace".into()), 1_000);
        let (second_time, second) =
            super::session_terminated_record("session", Some("workspace".into()), 2_000);
        assert_ne!(first_time, second_time, "M1");
        assert_ne!(first.timestamp, second.timestamp, "M1");
        assert_eq!(first.id, second.id, "M1");
        assert_eq!(first.object_id, second.object_id, "M1");
        assert_eq!(first.workspace_id, second.workspace_id, "M1");
        assert_eq!(first.event_type, second.event_type, "M1");
        assert_eq!(first.runtime_interval, second.runtime_interval, "M1");
    }

    #[test]
    fn independent_route_requests_keep_intent_while_observation_time_changes() {
        // Causes: P1 two release requests carry the same Repository selector
        // but sample 1s and 2s; P2 another request changes the expected Git
        // commit. Effects: P1 identical Session/Workspace/fact identity and
        // selector, different display/fact timestamps; P2 a distinct intent.
        let selector = |commit: &str| super::SessionRepositoryPublicationSelector {
            binding_id: awaken_resource_contract::BindingId::new("source"),
            expectation: serde_json::from_value(serde_json::json!({
                "branch": "awf/work",
                "commit": commit,
            }))
            .unwrap(),
        };
        let command = |commit: &str, observed_at| {
            super::SessionArchiveWithRepositoryPublicationCommand::new(
                "workspace".into(),
                awaken_session_types::SessionId::from("session"),
                selector(commit),
                observed_at,
            )
        };
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let first = command(commit, 1_000);
        let second = command(commit, 2_000);
        assert_eq!(first.owner_scope, second.owner_scope, "P1");
        assert_eq!(first.session_id, second.session_id, "P1");
        assert_eq!(first.lifecycle_fact.id, second.lifecycle_fact.id, "P1");
        assert_eq!(
            first.lifecycle_fact.event_type, second.lifecycle_fact.event_type,
            "P1"
        );
        assert_eq!(first.repository, second.repository, "P1");
        assert_ne!(first.archived_at, second.archived_at, "P1");
        assert_ne!(
            first.lifecycle_fact.timestamp, second.lifecycle_fact.timestamp,
            "P1"
        );
        assert_ne!(
            first.repository,
            command("1111111111111111111111111111111111111111", 3_000).repository,
            "P2"
        );
    }

    #[test]
    fn profiled_release_selects_its_first_admission_from_the_execution_state() {
        // Exhaustive table over every execution state x terminal flag:
        // F1 terminal -> none; F2 Preparing/Running -> profiled release;
        // F3 any other live state -> idle-only archive.
        use awaken_session_state_contract::{SessionArchiveAdmission, SessionExecutionState};
        for execution in [
            SessionExecutionState::Preparing,
            SessionExecutionState::Activating,
            SessionExecutionState::ActivationFailed,
            SessionExecutionState::Running,
            SessionExecutionState::Rescheduling,
            SessionExecutionState::Idle,
            SessionExecutionState::Terminated,
        ] {
            assert_eq!(
                super::profiled_release_first_admission(execution, true),
                None,
                "F1 {execution:?}"
            );
            let expected = match execution {
                SessionExecutionState::Preparing | SessionExecutionState::Running => {
                    SessionArchiveAdmission::ProfiledRelease
                }
                _ => SessionArchiveAdmission::IdleOnly,
            };
            assert_eq!(
                super::profiled_release_first_admission(execution, false),
                Some(expected),
                "F2/F3 {execution:?}"
            );
        }
    }
}
