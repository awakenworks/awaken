//! Durable activity fencing for overlapping Session turns.

use awaken_session_contract::PersistedSession;

use super::{SessionApplication, SessionMutationError};

/// Failure from a Session activity transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionActivityError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is terminal and cannot begin another activity")]
    Terminal,
    #[error("Session activity epoch is exhausted")]
    EpochExhausted,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session activity persistence is unavailable: {0}")]
    Unavailable(String),
}

impl SessionActivityError {
    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::Unavailable(
                "activity mutation idempotency key unexpectedly changed payload".into(),
            ),
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }
}

impl SessionApplication {
    /// Admit one driving event and return the committed aggregate carrying its
    /// monotonically increasing completion fence.
    pub async fn begin_activity(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, SessionActivityError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .ok_or(SessionActivityError::NotFound)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .ok_or(SessionActivityError::NotFound)?;
            if session.is_terminal() {
                return Err(SessionActivityError::Terminal);
            }
            session.activity_epoch = session
                .activity_epoch
                .checked_add(1)
                .ok_or(SessionActivityError::EpochExhausted)?;
            session.status = "running".into();
            match self
                .commit_session_snapshot(&owner_scope, session, "begin-activity", Vec::new())
                .await
            {
                Ok(session) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }

    /// Settle a driving event only while its epoch still owns the Session.
    /// A newer activity or any terminal transition fences the completion.
    pub async fn settle_activity(
        &self,
        session_id: &str,
        expected_epoch: u64,
    ) -> Result<PersistedSession, SessionActivityError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .owner(session_id)
                .await
                .ok_or(SessionActivityError::NotFound)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .ok_or(SessionActivityError::NotFound)?;
            if session.is_terminal() || session.activity_epoch != expected_epoch {
                return Ok(session);
            }
            session.status = "idle".into();
            match self
                .commit_session_snapshot(&owner_scope, session, "settle-activity", Vec::new())
                .await
            {
                Ok(session) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(SessionActivityError::mutation(error)),
            }
        }
        Err(SessionActivityError::Conflict)
    }
}
