// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-application/src/queries.rs
// @formal reason: Carries one already-decided Session ownership verdict by type. The verdict is
// @formal reason: computed only by SessionRootReads::authorize from the durable owner.
//! One Session ownership verdict, carried by type instead of re-checked.
//!
//! Every owner-scoped command outside this crate takes a [`SessionAccess`]. It
//! is minted only from the durable owner of an existing Session, so a command
//! cannot reach a Session its caller does not own, and no command decides
//! ownership a second time with a different answer.
//!
//! | Durable owner | Caller | Verdict |
//! |---|---|---|
//! | equal | owner | `SessionAccess` |
//! | another Workspace | owner | `NotFound` |
//! | none (missing) | owner | `NotFound`, byte-equal to the foreign row |
//! | dependency outage / invalid durable state | owner | `Unavailable` / `Internal` |

use awaken_runtime_contract::runs::{RunError, RunErrorKind};
use awaken_session_types::SessionId;

/// Proof that the native ownership port admitted one existing Session. A
/// Workspace-only request cannot obtain this proof for a private Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAccess {
    owner_scope: String,
    session_id: SessionId,
}

impl SessionAccess {
    /// Minted only by this crate after it read the durable owner.
    pub(crate) fn verified(owner_scope: &str, session_id: &SessionId) -> Self {
        Self {
            owner_scope: owner_scope.to_string(),
            session_id: session_id.clone(),
        }
    }

    #[must_use]
    pub fn owner_scope(&self) -> &str {
        &self.owner_scope
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Why no [`SessionAccess`] was minted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionAccessError {
    /// Missing and foreign Sessions are deliberately one answer.
    #[error("Session was not found")]
    NotFound,
    #[error("Session ownership is unavailable: {0}")]
    Unavailable(String),
    #[error("Session ownership durable state is invalid: {0}")]
    Internal(String),
}

impl From<SessionAccessError> for RunError {
    fn from(error: SessionAccessError) -> Self {
        match error {
            SessionAccessError::NotFound => RunError {
                message: "Session was not found".into(),
                kind: RunErrorKind::BadRequest,
                code: "session_not_found".into(),
            },
            SessionAccessError::Unavailable(message) => RunError::unavailable(message),
            SessionAccessError::Internal(message) => {
                crate::SessionMutationError::Internal(message).into()
            }
        }
    }
}

impl From<SessionAccessError> for crate::SessionProjectionRecoveryError {
    fn from(error: SessionAccessError) -> Self {
        match error {
            SessionAccessError::NotFound => Self::NotFound,
            SessionAccessError::Unavailable(message) => Self::Unavailable(message),
            SessionAccessError::Internal(message) => {
                Self::Rejected(crate::SessionMutationError::Internal(message).into())
            }
        }
    }
}
