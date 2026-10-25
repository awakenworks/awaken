//! The one refusal of a poisoned delivery lock.

/// Stable code of every poisoned process-local lock refusal of the Session
/// execution (this crate's and the executor's), so no caller re-classifies a
/// poisoned lock from text.
pub const LOCK_POISONED_CODE: &str = "session_executor_lock_poisoned";

/// A process-local delivery lock whose holder panicked. Its state may be
/// half-written, so the operation that needed it is refused as a retryable
/// `Unavailable` with [`LOCK_POISONED_CODE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryLock {
    #[error("shared tool admission lock poisoned")]
    ToolAdmission,
    #[error("foreground completion registry lock poisoned")]
    CompletionRegistry,
    #[error("Thread event hub lock poisoned")]
    ThreadEventHub,
    #[error("Session dispatch projections lock poisoned")]
    SessionProjections,
    #[error("Session step projection lock poisoned")]
    StepProjections,
    #[error("Session Outcome command lock poisoned")]
    OutcomeCommands,
}

impl From<DeliveryLock> for awaken_session_types::RunError {
    fn from(error: DeliveryLock) -> Self {
        Self::unavailable_classified(LOCK_POISONED_CODE, error.to_string())
    }
}

impl From<crate::CompletionSignalClosed> for awaken_session_types::RunError {
    fn from(error: crate::CompletionSignalClosed) -> Self {
        Self::internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_delivery_lock_is_one_retryable_class() {
        // Cause: any delivery lock is poisoned. Effect: one Unavailable Run
        // refusal with the one stable code the executor's locks share.
        for lock in [
            DeliveryLock::ToolAdmission,
            DeliveryLock::CompletionRegistry,
            DeliveryLock::ThreadEventHub,
            DeliveryLock::SessionProjections,
            DeliveryLock::StepProjections,
            DeliveryLock::OutcomeCommands,
        ] {
            let run = awaken_session_types::RunError::from(lock);
            assert_eq!(
                run.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "{lock}"
            );
            assert_eq!(run.code, LOCK_POISONED_CODE, "{lock}");
        }
    }
}
