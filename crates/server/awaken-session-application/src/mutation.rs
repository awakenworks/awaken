// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
// @formal evidence session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
//! Canonical durable Session root mutation boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::SessionCreationError;
use awaken_session_contract::SessionAggregate;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRepositoryConflict,
    SessionRepositoryError,
};
use awaken_session_contract::{SessionRepository, SessionRepositoryReads};
use awaken_session_types::SessionRevision;
use awaken_time::WallClockExt as _;

use crate::creation::{
    CompleteSessionCreateResult, CompleteSessionRoot, DurableSessionRoot,
    SessionCreationCompletion, validated_create_replay,
};

pub(crate) fn repository_failure(error: SessionRepositoryError) -> SessionMutationError {
    error.into()
}

impl From<SessionRepositoryError> for SessionMutationError {
    fn from(error: SessionRepositoryError) -> Self {
        match error {
            SessionRepositoryError::NotFound => SessionMutationError::NotFound,
            SessionRepositoryError::Conflict(SessionRepositoryConflict::IdempotencyMismatch) => {
                SessionMutationError::IdempotencyMismatch
            }
            SessionRepositoryError::Conflict(_conflict) => SessionMutationError::Conflict,
            SessionRepositoryError::AuthorityUnavailable(message) => Self::Unavailable(message),
            SessionRepositoryError::NotApplied(message) => Self::NotApplied(message),
            SessionRepositoryError::Corrupt(message)
            | SessionRepositoryError::InvalidMutation(message) => Self::Internal(message),
        }
    }
}

pub(crate) async fn session_owner(
    repository: &dyn SessionRepositoryReads,
    session_id: &awaken_session_types::SessionId,
) -> Result<String, SessionMutationError> {
    repository
        .owner(session_id)
        .await
        .map_err(repository_failure)
}

/// What one snapshot commit did to the durable root: the committed aggregate,
/// whether *this* call applied it, and the original atomic receipt revision.
///
/// These values are sealed because the result and its revision are decisions
/// a caller must not author. A concurrent replay loses the root CAS and reads
/// the winner's
/// aggregate back; reporting that read as an application is what duplicated a
/// terminal fact on the delete path. That reload may observe a later mutation,
/// so its revision cannot replace the original receipt. All three values leave
/// the CAS
/// together, and the only producer is
/// [`SessionMutations::commit_session_snapshot_with_record`] — a literal is a
/// compile error outside this module:
///
/// ```compile_fail,E0451
/// use awaken_session_application::SnapshotCommit;
///
/// fn fabricate(session: awaken_session_contract::PersistedSession) -> SnapshotCommit {
///     SnapshotCommit {
///         session,
///         transitioned: true,
///         committed_revision: awaken_session_types::SessionRevision(1),
///     }
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
#[must_use = "a snapshot commit carries the applied/replayed outcome; name what happens to it"]
pub struct SnapshotCommit {
    session: PersistedSession,
    transitioned: bool,
    committed_revision: SessionRevision,
}

impl SnapshotCommit {
    /// Whether this call won the root CAS. `false` is an absorbing replay whose
    /// aggregate was written by a concurrent winner.
    #[must_use]
    pub fn transitioned(&self) -> bool {
        self.transitioned
    }

    #[must_use]
    pub fn session(&self) -> &PersistedSession {
        &self.session
    }

    /// Original revision of this command's atomic repository receipt. A replay
    /// may reload a newer aggregate; its current revision cannot replace this
    /// immutable command coordinate.
    #[must_use]
    pub fn committed_revision(&self) -> SessionRevision {
        self.committed_revision
    }

    /// Take the current committed aggregate and deliberately drop the receipt
    /// outcome/revision, for a command whose response does not report them.
    #[must_use]
    pub fn into_session(self) -> PersistedSession {
        self.session
    }
}

/// A typed refusal raised inside the application by the Session aggregate or
/// its budget ledger. It is classified once, by its conversion into
/// [`SessionMutationError`]: an outage of the list-price authority is
/// retryable [`SessionMutationError::Unavailable`]; every other refusal cannot
/// be repaired by repeating the command and stays [`SessionMutationError::Invalid`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionStateViolation {
    /// The aggregate refused to validate or apply a snapshot.
    #[error(transparent)]
    Aggregate(#[from] awaken_session_contract::SessionMutationValidationError),
    /// Usage could not be priced or reconciled against the frozen budget.
    #[error(transparent)]
    Budget(#[from] awaken_session_contract::ManagedListPriceError),
}

/// Failure returned by the Session application's one repository CAS boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionMutationError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    /// A dependency outage may be retried without changing the command.
    #[error("Session mutation repository is unavailable: {0}")]
    Unavailable(String),
    /// The repository refused before applying the mutation: nothing changed,
    /// and the same command may be retried.
    #[error("Session mutation was not applied: {0}")]
    NotApplied(String),
    /// Invalid durable state or a violated application invariant. Repeating the
    /// same command cannot repair this failure; adapters must preserve 500.
    #[error("Session mutation failed because durable state is invalid: {0}")]
    Internal(String),
    /// The aggregate or budget ledger refused the transition: the same
    /// permanent class as `Internal`, carrying its typed source.
    #[error("Session mutation failed because durable state is invalid: {0}")]
    Invalid(SessionStateViolation),
}

/// The one classification of a Session state violation. An unavailable
/// list-price authority is an outage the same command may retry; every other
/// violation is permanent.
impl From<SessionStateViolation> for SessionMutationError {
    fn from(violation: SessionStateViolation) -> Self {
        match violation {
            SessionStateViolation::Budget(
                error @ awaken_session_contract::ManagedListPriceError::Unavailable(_),
            ) => Self::Unavailable(error.to_string()),
            violation => Self::Invalid(violation),
        }
    }
}

impl From<SessionMutationError> for awaken_session_types::RunError {
    fn from(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::bad_request("Session was not found"),
            SessionMutationError::Conflict => Self::conflict("Session revision conflict"),
            SessionMutationError::IdempotencyMismatch => {
                Self::bad_request("Session idempotency key was reused with another payload")
            }
            SessionMutationError::Unavailable(message)
            | SessionMutationError::NotApplied(message) => Self::unavailable(message),
            SessionMutationError::Internal(message) => {
                Self::classified("session_application_invalid", message)
            }
            SessionMutationError::Invalid(violation) => {
                Self::classified("session_application_invalid", violation.to_string())
            }
        }
    }
}

/// Project a canonical mutation failure at the existing realization interface.
/// Phase commands delegate to this conversion instead of owning error policy.
impl From<SessionMutationError>
    for awaken_session_execution_contract::SessionRealizationControlFailure
{
    fn from(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::Unavailable(detail)
            | SessionMutationError::NotApplied(detail) => Self::Unavailable(detail),
            SessionMutationError::Internal(detail) => Self::Internal(detail),
            // The serialized control failure crosses the Worker boundary as its
            // closed class plus text; this is the one place the typed cause is
            // rendered for it.
            SessionMutationError::Invalid(violation) => Self::Internal(violation.to_string()),
            SessionMutationError::IdempotencyMismatch => {
                Self::Internal("Session idempotency key was reused with another payload".into())
            }
        }
    }
}

/// Update only the existing root observation. The caller compiles idempotency
/// before invoking this kernel, and only the repository CAS can persist it.
#[cfg(any(test, kani))]
fn observe_root_update(previous: &mut Option<u64>, now: u64) {
    *previous = awaken_session_contract::session_root_update_observation(*previous, now);
}

/// Interpret the actual repository receipt once. Both successful variants keep
/// their original revision; only Applied permits accepting the local candidate.
/// Replay requires loading the durable root through the existing port.
fn snapshot_commit_receipt(
    result: SessionMutationResult,
) -> Result<(SessionRevision, bool), SessionMutationError> {
    match result {
        SessionMutationResult::Applied { new_revision } => Ok((new_revision, true)),
        SessionMutationResult::Replayed { new_revision } => Ok((new_revision, false)),
        SessionMutationResult::Conflict { .. } => Err(SessionMutationError::Conflict),
        SessionMutationResult::IdempotencyMismatch => {
            Err(SessionMutationError::IdempotencyMismatch)
        }
        SessionMutationResult::AcknowledgedNoOp { .. } => Err(SessionMutationError::Internal(
            "an Event no-op receipt cannot certify a root mutation".into(),
        )),
    }
}

/// The sole full-snapshot key/hash compiler for application commands and
/// Environment admission/receipts. Unlike default Create request identity,
/// this fingerprint retains all server-owned materialization facts.
/// The existing B16 CAS-race and T6 old-create-replay cause tables cover the
/// distinction: different first expiry is a different snapshot command;
/// missing/present-empty is excluded only from default Create request identity.
pub(crate) fn compile_session_snapshot_idempotency(
    session: &SessionAggregate,
    operation: &str,
) -> IdempotencyRecord {
    let payload_hash = SessionMutationPayload::Replace(session.snapshot().clone()).stable_hash();
    IdempotencyRecord {
        key: format!(
            "session:{operation}:{}:{}:{payload_hash}",
            session.session_id, session.revision.0
        ),
        payload_hash,
    }
}

pub(crate) fn aggregate_from_snapshot(
    session: PersistedSession,
) -> Result<SessionAggregate, SessionRepositoryError> {
    SessionAggregate::try_from(session)
        .map_err(|error| SessionRepositoryError::Corrupt(error.to_string()))
}

/// One root-CAS retry bound shared by commands and binding receipts.
pub(crate) const ROOT_CAS_ATTEMPTS: usize = 3;

/// The one Session-root mutation component. Use-case services share the
/// original Repository and Clock; this component does not create a store,
/// driver, retry queue or alternative idempotency protocol.
pub struct SessionMutations {
    repository: Arc<dyn SessionRepository>,
    clock: Arc<dyn awaken_time::WallClock>,
    mutation_sequence: AtomicU64,
}

impl SessionMutations {
    pub(crate) fn new(
        repository: Arc<dyn SessionRepository>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            repository,
            clock,
            mutation_sequence: AtomicU64::new(0),
        }
    }

    pub(crate) fn repository(&self) -> &Arc<dyn SessionRepository> {
        &self.repository
    }

    /// Shared replacement boundary for application commands and Runtime binding
    /// receipts. Idempotency is compiled before observation; the repository remains
    /// the sole CAS owner. Callers retain their domain-specific retry/error policy.
    pub(crate) async fn commit_observed_snapshot(
        repository: &dyn SessionRepository,
        owner_scope: &str,
        session: &mut SessionAggregate,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
        now: u64,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        Self::commit_candidate(
            repository,
            owner_scope,
            session.compile_mutation(owner_scope, idempotency, lifecycle_facts, now),
            now,
        )
        .await
    }

    async fn commit_candidate(
        repository: &dyn SessionRepository,
        owner_scope: &str,
        mutation: SessionMutation,
        now: u64,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let observation = crate::sandbox_observation::prepare(&mutation.payload, now);
        let result = repository.commit(owner_scope, mutation).await;
        crate::sandbox_observation::committed(observation, result.as_ref().ok());
        result
    }

    /// Import one authoritative read into the controlled working candidate.
    pub(crate) async fn session_aggregate(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<SessionAggregate, SessionRepositoryError> {
        self.repository
            .get(session_id)
            .await
            .and_then(aggregate_from_snapshot)
    }

    /// Allocate one process-call identity for a mutation that intentionally has
    /// no external idempotency key. The identity remains stable across this
    /// call's root-CAS retries, while two genuinely distinct concurrent calls
    /// can never collapse merely because their compiled snapshots are equal.
    pub(crate) fn fresh_mutation_record(
        &self,
        runtime_incarnation: &str,
        session_id: &str,
        operation: &str,
    ) -> IdempotencyRecord {
        let sequence = self.mutation_sequence.fetch_add(1, Ordering::Relaxed);
        let identity = awaken_agent_contract::stable_fingerprint(&(
            "session-application-mutation-v1",
            runtime_incarnation,
            sequence,
            session_id,
            operation,
        ));
        IdempotencyRecord {
            key: format!("session:{operation}:{session_id}:{identity}"),
            payload_hash: identity,
        }
    }

    /// Explicitly close (expiry zero) exactly the assignment `attempted`, by
    /// root CAS, when it is still the root's current lease.
    ///
    /// For a lease its caller minted but never issued: no directive left the
    /// process, so no effect can run under it. An only time-expired lease is
    /// not explicit closure, so leaving it would keep the Session leased.
    ///
    /// | current root lease | effect                      |
    /// |--------------------|-----------------------------|
    /// | `attempted`        | expired (explicitly closed) |
    /// | another / none     | unchanged                   |
    /// | Session absent     | unchanged                   |
    pub(crate) async fn expire_exact_assignment(
        &self,
        session_id: &awaken_session_types::SessionId,
        attempted: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), SessionMutationError> {
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = match self.owner(session_id).await {
                Ok(owner_scope) => owner_scope,
                Err(SessionMutationError::NotFound) => return Ok(()),
                Err(error) => return Err(error),
            };
            let mut session = match self.session_aggregate(session_id).await {
                Ok(session) => session,
                Err(SessionRepositoryError::NotFound) => return Ok(()),
                Err(error) => return Err(repository_failure(error)),
            };
            if session.realization.as_ref() != Some(attempted) {
                return Ok(());
            }
            session.assignment().expire();
            match self
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "expire-exact-assignment",
                    Vec::new(),
                )
                .await
            {
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                result => return result.map(|_| ()),
            }
        }
        Err(SessionMutationError::Conflict)
    }

    pub async fn owner(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, SessionMutationError> {
        session_owner(self.repository.as_ref(), session_id).await
    }

    /// Insert one newly compiled Session root. Exact replays return the durable
    /// aggregate; a different command targeting the same identity conflicts.
    pub(crate) async fn create_session_root(
        &self,
        owner_scope: &str,
        mut root: CompleteSessionRoot,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<CompleteSessionCreateResult, SessionCreationError> {
        let private_binding =
            awaken_session_contract::SessionPrivateBinding::of(root.as_session()).cloned();
        // Receipt identity was compiled before this server-owned observation.
        // A repository replay returns the first committed times unchanged.
        let now = self.clock.now_ms();
        root.observe_creation(now)?;
        let result = self
            .repository
            .create(
                owner_scope,
                root.into_session(),
                idempotency,
                lifecycle_facts,
            )
            .await
            .map_err(SessionCreationError::repository)?;
        let returned = match &result {
            awaken_session_contract::SessionCreateResult::Applied(session)
            | awaken_session_contract::SessionCreateResult::Replayed(session) => session,
        };
        // A valid receipt identifies an original effect, never a different
        // private owner. Check before projection, realization or returning it.
        if awaken_session_contract::SessionPrivateBinding::of(returned) != private_binding.as_ref()
        {
            return Err(SessionCreationError::Conflict);
        }
        match result {
            awaken_session_contract::SessionCreateResult::Applied(session) => {
                crate::sandbox_observation::created(&session, session.revision, now);
                Ok(CompleteSessionCreateResult::Applied(
                    DurableSessionRoot::from_applied(session)?,
                ))
            }
            awaken_session_contract::SessionCreateResult::Replayed(session) => {
                Ok(CompleteSessionCreateResult::Replayed(
                    DurableSessionRoot::from_recovery(session)
                        .map_err(|message| SessionCreationError::Internal(message.into()))?,
                ))
            }
        }
    }

    /// Atomically inspect the repository's create receipt and identity. This is
    /// the only preflight used by deterministic protocol creates; it never
    /// assembles receipt, aggregate, and owner from separate reads.
    pub async fn replay_session_create(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.replay_session_create_with_completion(
            owner_scope,
            session_id,
            idempotency,
            SessionCreationCompletion::AwaitRealization,
            None,
        )
        .await
    }

    /// Atomically inspect an asynchronous create receipt without converting a
    /// durably failed realization into a transport-level create conflict. The
    /// repository read and identity comparison remain the same authority as
    /// [`Self::replay_session_create`]; only the caller's completion boundary
    /// differs.
    pub async fn replay_accepted_session_create(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.replay_session_create_with_completion(
            owner_scope,
            session_id,
            idempotency,
            SessionCreationCompletion::AcceptDurableRoot,
            None,
        )
        .await
    }

    /// Recover an exact private create receipt after current trusted ingress.
    /// Ownership is checked against the original root in the same repository
    /// result; this method never upgrades a Workspace or legacy root.
    pub async fn replay_private_session_create(
        &self,
        binding: &awaken_session_contract::SessionPrivateBinding,
        session_id: &awaken_session_types::SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.replay_session_create_with_completion(
            binding.workspace_id(),
            session_id,
            idempotency,
            SessionCreationCompletion::AwaitRealization,
            Some(binding),
        )
        .await
    }

    /// Recover a private accepted root without re-entering realization.
    pub async fn replay_accepted_private_session_create(
        &self,
        binding: &awaken_session_contract::SessionPrivateBinding,
        session_id: &awaken_session_types::SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.replay_session_create_with_completion(
            binding.workspace_id(),
            session_id,
            idempotency,
            SessionCreationCompletion::AcceptDurableRoot,
            Some(binding),
        )
        .await
    }

    pub(super) async fn replay_session_create_with_completion(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency: &IdempotencyRecord,
        completion: SessionCreationCompletion,
        private_binding: Option<&awaken_session_contract::SessionPrivateBinding>,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.repository
            .replay_create(owner_scope, session_id, idempotency)
            .await
            .map_err(SessionCreationError::repository)?
            .filter(|session| {
                awaken_session_contract::SessionPrivateBinding::of(session) == private_binding
            })
            .map(|session| validated_create_replay(session, completion))
            .transpose()
    }

    /// Commit a complete aggregate replacement through the sole root CAS and
    /// return the canonical Applied/Replayed outcome with it.
    ///
    /// There is deliberately no outcome-discarding sibling. The one that
    /// existed returned only the aggregate, so a command that *did* report
    /// applied/replayed had to invent the second half beside the call — which
    /// is exactly how a concurrent archive replay came to report itself as an
    /// application. A caller whose response does not carry the distinction
    /// names the discard with [`SnapshotCommit::into_session`].
    pub async fn commit_session_snapshot_with_outcome(
        &self,
        owner_scope: &str,
        session: SessionAggregate,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SnapshotCommit, SessionMutationError> {
        // Keep the shared aggregate-owning future off each caller's stack, as
        // at the existing creation boundary. Polling/cancellation remain inline
        // with this caller; no task or retry owner is detached.
        let idempotency = compile_session_snapshot_idempotency(&session, operation);
        Box::pin(self.commit_session_snapshot_with_record(
            owner_scope,
            session,
            idempotency,
            lifecycle_facts,
        ))
        .await
    }

    pub async fn commit_session_snapshot_with_record(
        &self,
        owner_scope: &str,
        mut session: SessionAggregate,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SnapshotCommit, SessionMutationError> {
        let session_id = session.session_id.clone();
        let result = Self::commit_observed_snapshot(
            self.repository.as_ref(),
            owner_scope,
            &mut session,
            idempotency,
            lifecycle_facts,
            self.clock.now_ms(),
        )
        .await
        .map_err(repository_failure)?;
        let (committed_revision, transitioned) = snapshot_commit_receipt(result)?;
        let session = if transitioned {
            session
                .into_applied_snapshot(committed_revision)
                .map_err(SessionStateViolation::from)?
        } else {
            self.repository
                .get(&session_id)
                .await
                .map_err(repository_failure)?
        };
        Ok(SnapshotCommit {
            session,
            transitioned,
            committed_revision,
        })
    }

    /// Execute any validated root mutation, including terminal tombstones.
    /// Aggregate command compilation happens before this boundary; repository
    /// result classification happens only here.
    pub async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionMutationError> {
        Self::commit_candidate(
            self.repository.as_ref(),
            owner_scope,
            mutation,
            self.clock.now_ms(),
        )
        .await
        .map_err(repository_failure)
    }
}

#[cfg(any(test, kani))]
mod observation_proofs;
