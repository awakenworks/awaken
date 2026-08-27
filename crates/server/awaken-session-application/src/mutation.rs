//! Canonical durable Session root mutation boundary.

use super::*;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionCreateResult,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionRepositoryConflict,
    SessionRepositoryError,
};

use crate::creation::validated_create_replay;

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
        self.sessions_repo
            .replay_create(owner_scope, session_id, idempotency)
            .await
            .map_err(SessionCreationError::repository)?
            .map(validated_create_replay)
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
