//! Canonical durable Session root mutation boundary.

use super::*;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionMutation,
    SessionMutationPayload, SessionMutationResult,
};

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

    pub async fn owner(&self, session_id: &str) -> Option<String> {
        self.sessions_repo.owner(session_id).await
    }

    /// Insert one newly compiled Session root. Exact replays return the durable
    /// revision; a different command targeting the same identity conflicts.
    pub async fn create_session_root(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<PersistedSession, SessionMutationError> {
        let revision = self
            .sessions_repo
            .create(owner_scope, session.clone(), idempotency, lifecycle_facts)
            .await
            .map_err(|error| match error {
                awaken_session_contract::SessionRepositoryError::AlreadyExists => {
                    SessionMutationError::Conflict
                }
                awaken_session_contract::SessionRepositoryError::IdempotencyMismatch => {
                    SessionMutationError::IdempotencyMismatch
                }
                error => SessionMutationError::Unavailable(error.to_string()),
            })?;
        session.revision = revision;
        Ok(session)
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
                .ok_or(SessionMutationError::NotFound),
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
            .map_err(|error| SessionMutationError::Unavailable(error.to_string()))
    }
}
