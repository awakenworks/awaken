//! Managed Session command failures and their protocol-facing classification.

use awaken_session_contract::{LiveInboxError, RunError, RunErrorKind};

const SESSION_PROJECTION_RECOVERY_REQUIRED_CODE: &str = "session_projection_recovery_required";

/// Why a session operation failed (mapped to an HTTP status by the router).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("session not found")]
    NotFound,
    /// A write was sent to an archived (terminated, read-only) session; the router
    /// maps it to 409 `invalid_request_error`.
    #[error("session is archived and is read-only")]
    Archived,
    /// The root Session revision changed while a command was being compiled.
    /// Callers re-read and retry the complete command; stale snapshots are never
    /// merged or written back.
    #[error("session changed concurrently; read the latest revision and retry")]
    Conflict,
    #[error("session idempotency key was reused with another request")]
    IdempotencyMismatch,
    /// Deterministic create found its exact durable receipt, but the original
    /// activation ended permanently. The identity remains occupied and cannot
    /// be replayed, retried, or replaced by the protocol adapter.
    #[error("session_create_terminal_conflict")]
    TerminalCreateConflict,
    /// A session create named a vault that does not exist (`vault_ids`); the
    /// router maps it to the standard 404 envelope naming the vault id.
    #[error("vault `{0}` not found")]
    VaultNotFound(String),
    #[error(transparent)]
    Run(#[from] RunError),
    /// A live-inbox operation was refused (inactive queue, unknown message,
    /// or a stale reorder); the router maps each case to its own status.
    #[error(transparent)]
    LiveInbox(#[from] LiveInboxError),
}

impl StateError {
    /// Classify a damaged committed Awaiting projection without weakening
    /// strict reply admission. Event reads expose the existing non-2xx health
    /// seam, while the Session route may still return root-owned lifecycle
    /// truth so a client can authorize the pure Interrupt recovery control.
    pub(crate) fn projection_recovery_required(message: impl Into<String>) -> Self {
        Self::Run(RunError {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
            code: SESSION_PROJECTION_RECOVERY_REQUIRED_CODE.into(),
        })
    }

    pub(crate) fn is_projection_recovery_required(&self) -> bool {
        matches!(
            self,
            Self::Run(error) if error.code == SESSION_PROJECTION_RECOVERY_REQUIRED_CODE
        )
    }
}

impl From<awaken_session_contract::SessionRepositoryError> for StateError {
    fn from(error: awaken_session_contract::SessionRepositoryError) -> Self {
        match error {
            awaken_session_contract::SessionRepositoryError::NotFound => Self::NotFound,
            awaken_session_contract::SessionRepositoryError::Unavailable(message) => {
                Self::Run(RunError::unavailable(message))
            }
            awaken_session_contract::SessionRepositoryError::Conflict(_) => Self::Conflict,
            error => Self::Run(RunError::internal(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_contract::{SessionRepositoryConflict, SessionRepositoryError};

    #[test]
    fn repository_causes_survive_the_managed_boundary() {
        // Cause/effect table: R1 missing => NotFound; R2 outage => retryable
        // Unavailable; R3 CAS conflict => Conflict; R4 corrupt/invalid durable
        // state => Internal. No error is converted to an empty successful read.
        assert!(
            matches!(
                StateError::from(SessionRepositoryError::NotFound),
                StateError::NotFound
            ),
            "R1"
        );
        assert!(
            matches!(
                StateError::from(SessionRepositoryError::Unavailable("offline".into())),
                StateError::Run(RunError {
                    kind: RunErrorKind::Unavailable,
                    ..
                })
            ),
            "R2"
        );
        assert!(
            matches!(
                StateError::from(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::AlreadyExists
                )),
                StateError::Conflict
            ),
            "R3"
        );
        assert!(
            matches!(
                StateError::from(SessionRepositoryError::Corrupt("bad row".into())),
                StateError::Run(RunError {
                    kind: RunErrorKind::Internal,
                    ..
                })
            ),
            "R4"
        );
    }

    #[test]
    fn projection_recovery_classification_is_narrow_and_caller_safe() {
        // Cause/effect table: P1 an unreconstructable committed Awaiting
        // projection => stable recovery classification plus caller-safe 400;
        // P2 an ordinary bad request => not the Session-root read exception.
        // The route-level DI table proves only P1 can leave aggregate GET
        // readable while Events GET remains unhealthy.
        let damaged = StateError::projection_recovery_required("damaged await");
        assert!(damaged.is_projection_recovery_required(), "P1");
        assert!(
            matches!(
                damaged,
                StateError::Run(RunError {
                    kind: RunErrorKind::BadRequest,
                    ..
                })
            ),
            "P1"
        );
        assert!(
            !StateError::Run(RunError::bad_request("ordinary")).is_projection_recovery_required(),
            "P2"
        );
    }
}
