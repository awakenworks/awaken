// @formal entrypoint dream.public_terminal_requires_exact_session_and_artifact_cleanup_receipts
// @formal entrypoint session.delete_intent_is_hidden_terminal_and_idempotent
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
//! Durable terminal Session transitions.

use awaken_provisioning_contract::RepositoryPublicationExpectation;
use awaken_resource_contract::{BindingId, ResourceAccess};
use awaken_session_error_contract::RunError;
use awaken_session_resource_contract::{ResolvedInput, ResolvedInputSource};
use awaken_session_state_contract::{
    ManagedLifecycleFact, PersistedSession, SessionDisposition, SessionExecutionState,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
};
use awaken_time::WallClockExt as _;

use super::{
    SessionApplication, SessionMutationError, SessionPreparationError, SnapshotCommit,
    activity::runtime_interval_fact, mutation::repository_failure,
    resource_reconciliation::mutation_failure,
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
    TerminalOnly,
    ForceTerminal,
}

impl ArchiveAdmission {
    const fn as_identity_str(self) -> &'static str {
        match self {
            Self::IdleOnly => "idle_only",
            Self::TerminalOnly => "terminal_only",
            Self::ForceTerminal => "force_terminal",
        }
    }
}

/// One Session Application command-identity law for both archive entrances.
/// Route observation time belongs only to the candidate that wins the root
/// CAS. It cannot distinguish two requests for the same business intent.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ArchiveBusinessIntent<'a> {
    Ordinary {
        admission: &'static str,
    },
    RepositoryPublication {
        source_revision: awaken_session_types::SessionRevision,
        binding_id: &'a BindingId,
        expectation: &'a RepositoryPublicationExpectation,
        resolved: &'a SessionRepositoryPublicationIntent,
    },
}

fn archive_idempotency_record(
    owner_scope: &str,
    session_id: &awaken_session_types::SessionId,
    fact: &ManagedLifecycleFact,
    business: ArchiveBusinessIntent<'_>,
) -> awaken_session_state_contract::IdempotencyRecord {
    let key = match &business {
        ArchiveBusinessIntent::Ordinary { .. } => {
            format!("session:archive:{session_id}:{}", fact.id)
        }
        ArchiveBusinessIntent::RepositoryPublication {
            source_revision, ..
        } => format!(
            "session:archive-with-repository-publication:{session_id}:{}:{}",
            source_revision.0, fact.id
        ),
    };
    let payload_hash = awaken_agent_contract::stable_fingerprint(&(
        "session-archive-intent-v2",
        owner_scope,
        session_id,
        &fact.id,
        &fact.object_id,
        &fact.workspace_id,
        &fact.event_type,
        &fact.runtime_interval,
        &business,
    ));
    awaken_session_state_contract::IdempotencyRecord { key, payload_hash }
}

#[cfg(test)]
mod archive_identity_tests {
    use super::*;

    #[test]
    fn sampled_time_is_not_identity_but_interval_and_admission_are() {
        // Causes: I1 same Session/owner/fact id/type/admission, different
        // Route timestamps; I2 same id/type but a different Runtime interval;
        // I3 same fact but a different admission business rule. Effects: I1
        // exact key+hash replay; I2/I3 same key but different payload hashes,
        // so the Repository refuses an accidental different-intent replay.
        let id = awaken_session_types::SessionId::from("session");
        let mut fact = ManagedLifecycleFact {
            id: "session:session:terminated".into(),
            object_id: id.to_string(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        let record = |fact: &ManagedLifecycleFact, admission| {
            archive_idempotency_record(
                "workspace",
                &id,
                fact,
                ArchiveBusinessIntent::Ordinary { admission },
            )
        };
        let original = record(&fact, "idle_only");
        fact.timestamp = 2;
        assert_eq!(record(&fact, "idle_only"), original, "I1");
        fact.runtime_interval = Some(
            serde_json::from_value(serde_json::json!({
                "interval_id": "interval", "activity_epoch": 1,
                "started_at_unix_ms": 1, "ended_at_unix_ms": 2
            }))
            .unwrap(),
        );
        let interval = record(&fact, "idle_only");
        assert_eq!(interval.key, original.key, "I2 same public fact id");
        assert_ne!(interval.payload_hash, original.payload_hash, "I2 conflict");
        let force = record(&fact, "force_terminal");
        assert_eq!(force.key, original.key, "I3 same public fact id");
        assert_ne!(force.payload_hash, interval.payload_hash, "I3 conflict");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveCleanupCompletion {
    Recoverable,
    Required,
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
    session_id: awaken_session_types::SessionId,
    /// A test's fixed request instant. Production commands carry none: the
    /// application stamps the request on its own clock when it admits it.
    requested_at_unix_s: Option<i64>,
}

impl SessionDeleteCommand {
    #[must_use]
    pub fn new(session_id: impl Into<awaken_session_types::SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            requested_at_unix_s: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn at_for_test(session_id: &str, requested_at_unix_s: i64) -> Self {
        Self {
            session_id: session_id.into(),
            requested_at_unix_s: Some(requested_at_unix_s),
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
            let changed = session
                .archive_with_repository_publication(&command.archived_at, archive_time, intent)
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
                transition = Some(SessionDispositionMutation::unchanged(owner_scope, session));
                break;
            }
            let archive_time = archive_time.ok_or_else(|| {
                SessionArchiveWithRepositoryPublicationError::Unavailable(
                    "committed archive has no Application time observation".into(),
                )
            })?;
            let mut facts = session
                .close_runtime_interval(archive_time)
                .map(|interval| {
                    runtime_interval_fact(&owner_scope, command.session_id.as_str(), interval)
                })
                .into_iter()
                .collect::<Vec<_>>();
            facts.push(command.lifecycle_fact.clone());
            match self
                .commit_session_snapshot_with_record(&owner_scope, session, idempotency, facts)
                .await
            {
                Ok(commit) => {
                    transition = Some(SessionDispositionMutation::committed(owner_scope, commit));
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
            .release_terminal_resources(&transition.owner_scope, command.session_id.as_str())
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
                    if rejection.effect_rejection.is_source_closed() {
                        "repository_publication_source_closed"
                    } else {
                        "repository_publication_rejected"
                    },
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
            .retire_session_work(
                &baseline.environment.environment_id,
                session.session_id.as_str(),
            )
            .await
            .map(|_| ())
            .map_err(|error| {
                SessionPreparationError::Rejected(
                    awaken_session_error_contract::RunError::unavailable_classified(
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
            ArchiveCleanupCompletion::Recoverable,
        )
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
            ArchiveCleanupCompletion::Required,
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
            ArchiveCleanupCompletion::Required,
        )
        .await
    }

    async fn terminate_session_with_admission(
        &self,
        session_id: &awaken_session_types::SessionId,
        archived_at: &str,
        fact: ManagedLifecycleFact,
        admission: ArchiveAdmission,
        cleanup_completion: ArchiveCleanupCompletion,
    ) -> Result<SessionDispositionMutation, SessionPreparationError> {
        let transition = self
            .commit_archive(session_id, archived_at, fact, admission)
            .await?;
        if transition.transitioned {
            self.notify_lifecycle_fact();
        }
        // Archive is linearized by the durable terminal fence above. Physical
        // Runtime/Resource cleanup is a recoverable saga. Public archive returns
        // the committed transition; internal required-completion callers join
        // its verified cleanup below. Wake the lifecycle owner before the eager
        // attempt so caller cancellation cannot strand the intent; the attempt
        // only shortens convergence when the current projector is available.
        self.wake_lifecycle_supervisor();
        let cleanup = self
            .release_terminal_resources(&transition.owner_scope, session_id.as_str())
            .await;
        match (cleanup_completion, cleanup) {
            (ArchiveCleanupCompletion::Required, Ok(Some(session))) => {
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
            (ArchiveCleanupCompletion::Required, Ok(None)) => {}
            (ArchiveCleanupCompletion::Required, Err(error)) => return Err(error),
            (ArchiveCleanupCompletion::Recoverable, Err(error)) => {
                tracing::warn!(
                    session = session_id.as_str(),
                    error = ?error,
                    "Session archive cleanup remains pending for lifecycle recovery"
                );
            }
            (ArchiveCleanupCompletion::Recoverable, Ok(_)) => {}
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
                return Ok(SessionDispositionMutation::unchanged(owner_scope, session));
            }
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
                return Ok(SessionDispositionMutation::unchanged(owner_scope, session));
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
            if admission == ArchiveAdmission::TerminalOnly && !session.execution.is_terminal() {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "terminal Session release requires already-terminal execution",
                )));
            }
            let archive_time = self.clock.now_ms();
            session
                .archive(archived_at, archive_time)
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?;
            let mut facts = session
                .close_runtime_interval(archive_time)
                .map(|interval| runtime_interval_fact(&owner_scope, session_id.as_str(), interval))
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
                Ok(commit) => {
                    return Ok(SessionDispositionMutation::committed(owner_scope, commit));
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
            let owner_scope = self
                .owner(&awaken_session_types::SessionId::from(session_id))
                .await
                .map_err(mutation_failure)?;
            let mut session = self
                .session_repository()
                .get(&awaken_session_types::SessionId::from(session_id))
                .await
                .map_err(repository_failure)
                .map_err(mutation_failure)?;
            // The aggregate owns the complete Delete admission rule. Calling
            // it before the pending-batch gate makes a hidden durable replay
            // absorbing without duplicating disposition interpretation here;
            // a rejected first request mutates only this detached candidate.
            let Some(delete_fact) = session
                .request_delete(
                    &owner_scope,
                    command.requested_at_unix_s.unwrap_or_else(|| {
                        i64::try_from(self.clock.now().duration_since_epoch().as_secs())
                            .unwrap_or(i64::MAX)
                    }),
                )
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))?
            else {
                return Ok(SessionDispositionMutation::unchanged(owner_scope, session));
            };
            if session.has_incomplete_event_batches() {
                self.wake_lifecycle_supervisor();
                return Err(pending_event_batch_error());
            }
            let mut facts = session
                .close_runtime_interval(self.clock.now_ms())
                .map(|interval| runtime_interval_fact(&owner_scope, session_id, interval))
                .into_iter()
                .collect::<Vec<_>>();
            facts.push(delete_fact);
            match self
                .commit_session_snapshot_with_outcome(&owner_scope, session, "delete-intent", facts)
                .await
            {
                Ok(commit) => {
                    return Ok(SessionDispositionMutation::committed(owner_scope, commit));
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
