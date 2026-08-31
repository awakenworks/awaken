//! Canonical durable Session root mutation boundary.

use super::*;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionCreateResult,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionRepositoryConflict,
    SessionRepositoryError,
};

use crate::creation::{SessionCreationCompletion, validated_create_replay};

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
    pub async fn create_session_root(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionCreationError> {
        self.sessions_repo
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await
            .map_err(SessionCreationError::repository)
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
        let expected_revision = session.revision;
        let session_id = session.session_id.clone();
        let mutation = SessionMutation {
            expected_revision,
            idempotency,
            payload: SessionMutationPayload::Replace(session.clone()),
            lifecycle_facts,
        };
        match self.commit_mutation(owner_scope, mutation).await? {
            SessionMutationResult::Applied { new_revision } => {
                session.revision = new_revision;
                Ok((session, true))
            }
            SessionMutationResult::Replayed { .. } => self
                .sessions_repo
                .get(&session_id)
                .await
                .map(|session| (session, false))
                .map_err(repository_failure),
            SessionMutationResult::Conflict { .. } => Err(SessionMutationError::Conflict),
            SessionMutationResult::IdempotencyMismatch => {
                Err(SessionMutationError::IdempotencyMismatch)
            }
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
