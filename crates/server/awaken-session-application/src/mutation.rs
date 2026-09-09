//! Canonical durable Session root mutation boundary.

use super::*;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRepositoryConflict,
    SessionRepositoryError, SessionRevision,
};

use crate::creation::{
    CompleteSessionCreateResult, CompleteSessionRoot, DurableSessionRoot,
    SessionCreationCompletion, validated_create_replay,
};

pub(crate) fn repository_failure(error: SessionRepositoryError) -> SessionMutationError {
    match error {
        SessionRepositoryError::NotFound => SessionMutationError::NotFound,
        SessionRepositoryError::Conflict(SessionRepositoryConflict::IdempotencyMismatch) => {
            SessionMutationError::IdempotencyMismatch
        }
        SessionRepositoryError::Conflict(_conflict) => SessionMutationError::Conflict,
        SessionRepositoryError::Unavailable(message)
        | SessionRepositoryError::Corrupt(message)
        | SessionRepositoryError::InvalidMutation(message) => {
            SessionMutationError::Unavailable(message)
        }
    }
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
    #[error("Session mutation repository is unavailable: {0}")]
    Unavailable(String),
}

/// Update only the existing root observation. The caller compiles idempotency
/// before invoking this kernel, and only the repository CAS can persist it.
fn observe_root_update(previous: &mut Option<u64>, now: u64) {
    *previous = Some(now.max(previous.unwrap_or(0)));
}

/// Interpret the actual repository receipt without accepting a local candidate
/// on replay. `None` requires loading the durable root through the existing port.
fn applied_root_revision(
    result: SessionMutationResult,
) -> Result<Option<SessionRevision>, SessionMutationError> {
    match result {
        SessionMutationResult::Applied { new_revision } => Ok(Some(new_revision)),
        SessionMutationResult::Replayed { .. } => Ok(None),
        SessionMutationResult::Conflict { .. } => Err(SessionMutationError::Conflict),
        SessionMutationResult::IdempotencyMismatch => {
            Err(SessionMutationError::IdempotencyMismatch)
        }
    }
}

/// Shared replacement boundary for application commands and Runtime binding
/// receipts. Idempotency is compiled before observation; the repository remains
/// the sole CAS owner. Callers retain their domain-specific retry/error policy.
pub(crate) async fn commit_observed_snapshot(
    repository: &dyn ManagedSessionRepository,
    owner_scope: &str,
    session: &mut PersistedSession,
    idempotency: IdempotencyRecord,
    lifecycle_facts: Vec<ManagedLifecycleFact>,
    now: u64,
) -> Result<SessionMutationResult, SessionRepositoryError> {
    observe_root_update(&mut session.updated_at_unix_ms, now);
    repository
        .commit_mutation(
            owner_scope,
            SessionMutation {
                expected_revision: session.revision,
                idempotency,
                payload: SessionMutationPayload::Replace(session.clone()),
                lifecycle_facts,
            },
        )
        .await
}

impl SessionApplication {
    /// Allocate one process-call identity for a mutation that intentionally has
    /// no external idempotency key. The identity remains stable across this
    /// call's root-CAS retries, while two genuinely distinct concurrent calls
    /// can never collapse merely because their compiled snapshots are equal.
    pub(crate) fn fresh_mutation_record(
        &self,
        session_id: &str,
        operation: &str,
    ) -> IdempotencyRecord {
        let sequence = self.mutation_sequence.fetch_add(1, Ordering::Relaxed);
        let identity = awaken_session_contract::stable_fingerprint(&(
            "session-application-mutation-v1",
            self.runtime_incarnation.as_str(),
            sequence,
            session_id,
            operation,
        ));
        IdempotencyRecord {
            key: format!("session:{operation}:{session_id}:{identity}"),
            payload_hash: identity,
        }
    }

    /// Bounded retries reload the aggregate and rerun its domain command. Callers
    /// must never merge a stale snapshot after this many conflicts.
    pub const ROOT_CAS_ATTEMPTS: usize = 3;

    pub async fn owner(&self, session_id: &str) -> Result<String, SessionMutationError> {
        self.sessions_repo
            .owner(session_id)
            .await
            .map_err(repository_failure)
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
        // Receipt identity was compiled before this server-owned observation.
        // A repository replay returns the first committed times unchanged.
        let now = self.now_unix_ms();
        {
            let session = root.as_session_mut();
            session.created_at_unix_ms = Some(now);
            session
                .resources
                .active
                .observe_input_times(&Default::default(), now);
            if let Some(pending) = &mut session.resources.pending {
                pending.observe_input_times(&Default::default(), now);
            }
            session.updated_at_unix_ms = Some(now);
        }
        let result = self
            .sessions_repo
            .create(
                owner_scope,
                root.into_session(),
                idempotency,
                lifecycle_facts,
            )
            .await
            .map_err(SessionCreationError::repository)?;
        match result {
            awaken_session_contract::SessionCreateResult::Applied(session) => Ok(
                CompleteSessionCreateResult::Applied(DurableSessionRoot::from_applied(session)?),
            ),
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
        session_id: &str,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.replay_session_create_with_completion(
            owner_scope,
            session_id,
            idempotency,
            SessionCreationCompletion::AwaitRealization,
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
        session_id: &str,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.replay_session_create_with_completion(
            owner_scope,
            session_id,
            idempotency,
            SessionCreationCompletion::AcceptDurableRoot,
        )
        .await
    }

    async fn replay_session_create_with_completion(
        &self,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
        completion: SessionCreationCompletion,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.sessions_repo
            .replay_create(owner_scope, session_id, idempotency)
            .await
            .map_err(SessionCreationError::repository)?
            .map(|session| validated_create_replay(session, completion))
            .transpose()
    }

    /// Commit a complete aggregate replacement through the sole root CAS.
    pub async fn commit_session_snapshot(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<PersistedSession, SessionMutationError> {
        let expected_revision = session.revision;
        let payload = SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        let idempotency = IdempotencyRecord {
            key: format!(
                "session:{operation}:{}:{}:{payload_hash}",
                session.session_id, expected_revision.0
            ),
            payload_hash,
        };
        self.commit_session_snapshot_with_record(owner_scope, session, idempotency, lifecycle_facts)
            .await
            .map(|(session, _)| session)
    }

    pub async fn commit_session_snapshot_with_record(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<(PersistedSession, bool), SessionMutationError> {
        let session_id = session.session_id.clone();
        let result = commit_observed_snapshot(
            self.sessions_repo.as_ref(),
            owner_scope,
            &mut session,
            idempotency,
            lifecycle_facts,
            self.now_unix_ms(),
        )
        .await
        .map_err(repository_failure)?;
        match applied_root_revision(result)? {
            Some(new_revision) => {
                session.revision = new_revision;
                Ok((session, true))
            }
            None => self
                .sessions_repo
                .get(&session_id)
                .await
                .map(|session| (session, false))
                .map_err(repository_failure),
        }
    }

    /// Execute any validated root mutation, including terminal tombstones.
    /// Aggregate command compilation happens before this boundary; repository
    /// result classification happens only here.
    pub async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionMutationError> {
        self.sessions_repo
            .commit_mutation(owner_scope, mutation)
            .await
            .map_err(repository_failure)
    }
}

#[cfg(any(test, kani))]
mod observation_proofs;
