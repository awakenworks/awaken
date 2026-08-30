//! Shared Session reconciliation scan and persistence-failure policy.
//!
//! Durable stores remain the only row-decoding and quarantine owners. This
//! module supplies their closed page cursor and result vocabulary; it does not
//! persist scheduling state or introduce another recovery registry.

use super::{ScopedPersistedSession, SessionRepositoryError};

/// Closed supervisor policy for persistence failures. Retryable outages remain
/// pending; corrupt durable truth is isolated for operator repair; command and
/// concurrency failures are returned without background replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRepositoryRecoveryAction {
    Retry,
    Quarantine,
    Reject,
}

/// One corrupt durable Session row isolated by the authoritative store scan.
/// The raw aggregate is deliberately absent so recovery reporting cannot leak
/// persisted configuration or credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecoveryQuarantine {
    pub session_id: String,
    pub reason: String,
}

/// Opaque keyset position for the shared Session reconciliation index.
///
/// The position carries no durable ownership or scheduling authority. It is
/// valid only as the exclusive lower bound of a subsequent read from the same
/// repository authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecoveryCursor(String);

impl SessionRecoveryCursor {
    /// Project one already-read durable row identity into the next-page lower
    /// bound. Store implementations are the normal constructor; callers only
    /// carry the typed value returned by [`SessionRecoveryScan`].
    #[must_use]
    pub fn after_session_id(session_id: impl Into<String>) -> Self {
        Self(session_id.into())
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.0
    }
}

/// Complete result of one recovery scan. Store adapters own row decoding and
/// durable quarantine, so callers receive healthy work and isolation evidence
/// from one authority instead of maintaining a parallel recovery registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRecoveryScan {
    pub sessions: Vec<ScopedPersistedSession>,
    pub quarantined: Vec<SessionRecoveryQuarantine>,
    /// Exclusive keyset lower bound for the next fixed-size page. `None`
    /// proves this scan reached the end of the current index snapshot.
    pub next_cursor: Option<SessionRecoveryCursor>,
}

impl SessionRepositoryError {
    #[must_use]
    pub const fn recovery_action(&self) -> SessionRepositoryRecoveryAction {
        match self {
            Self::Unavailable(_) => SessionRepositoryRecoveryAction::Retry,
            Self::Corrupt(_) => SessionRepositoryRecoveryAction::Quarantine,
            Self::NotFound | Self::Conflict(_) | Self::InvalidMutation(_) => {
                SessionRepositoryRecoveryAction::Reject
            }
        }
    }
}
