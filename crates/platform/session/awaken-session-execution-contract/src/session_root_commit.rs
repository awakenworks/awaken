// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This contract names the original final Root CAS and typed refusal without selecting persistence.
//! Final Session-root mutation admission shared by application and claim composition.

use awaken_session_contract::{
    SessionMutation, SessionMutationResult, SessionRepositoryConflict, SessionRepositoryError,
};
use awaken_session_types::SessionRevision;

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

/// One lossless admission cause beside the cloneable CAS error. Runtime
/// diagnostics and original realization Control causes never duplicate state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionAdmissionFailure {
    #[error("{message}")]
    Runtime {
        kind: awaken_runtime_contract::runs::RunErrorKind,
        code: String,
        message: String,
    },
    #[error(transparent)]
    Control(crate::SessionRealizationControlFailure),
}

impl From<awaken_runtime_contract::runs::RunError> for SessionAdmissionFailure {
    fn from(error: awaken_runtime_contract::runs::RunError) -> Self {
        Self::Runtime {
            kind: error.kind,
            code: error.code,
            message: error.message,
        }
    }
}

impl From<SessionAdmissionFailure> for awaken_runtime_contract::runs::RunError {
    fn from(error: SessionAdmissionFailure) -> Self {
        match error {
            SessionAdmissionFailure::Runtime {
                kind,
                code,
                message,
            } => Self {
                kind,
                code,
                message,
            },
            SessionAdmissionFailure::Control(cause) => cause.into(),
        }
    }
}

/// Failure returned by the Session application's one repository CAS boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionMutationError {
    /// The composing claim/Work authority refused this operation. Preserve its
    /// typed HTTP family; a stale claim is not a retryable Root CAS conflict.
    #[error(transparent)]
    Admission(SessionAdmissionFailure),
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

impl SessionMutationError {
    #[must_use]
    pub fn admission(error: awaken_runtime_contract::runs::RunError) -> Self {
        Self::Admission(error.into())
    }
}

impl From<crate::SessionRealizationControlFailure> for SessionMutationError {
    fn from(cause: crate::SessionRealizationControlFailure) -> Self {
        Self::Admission(SessionAdmissionFailure::Control(cause))
    }
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

impl From<SessionMutationError> for awaken_runtime_contract::runs::RunError {
    fn from(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::Admission(error) => error.into(),
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
impl From<SessionMutationError> for crate::SessionRealizationControlFailure {
    fn from(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::Admission(SessionAdmissionFailure::Control(cause)) => cause,
            SessionMutationError::Admission(SessionAdmissionFailure::Runtime {
                kind,
                message,
                ..
            }) => match kind {
                awaken_runtime_contract::runs::RunErrorKind::BadRequest => Self::Invalid(message),
                awaken_runtime_contract::runs::RunErrorKind::Conflict => Self::Conflict,
                awaken_runtime_contract::runs::RunErrorKind::Internal => Self::Internal(message),
                awaken_runtime_contract::runs::RunErrorKind::Unavailable => {
                    Self::Unavailable(message)
                }
            },
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

/// Final admission for an already-compiled Session root mutation.
///
/// The Session use case retains its original bounded CAS loop and cold reads.
/// A composing claim authority binds this final CAS to its original authority
/// transaction. The use case retains its cold-work and retry boundaries.
#[async_trait::async_trait]
pub trait SessionRootCommit: Send + Sync {
    /// Final tenure check for a result that observed no Root mutation (for
    /// example an absent idempotency receipt). It proves no Root existence.
    async fn validate_claim(&self) -> Result<(), SessionMutationError>;

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionMutationError>;

    /// Validate even a no-change/read result before it can leave this use case.
    async fn validate(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_revision: SessionRevision,
    ) -> Result<bool, SessionMutationError>;
}
