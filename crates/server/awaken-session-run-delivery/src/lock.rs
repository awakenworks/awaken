//! The one refusal of a poisoned delivery lock.

/// Stable code of the original execution-owned delivery lock refusals.
pub const LOCK_POISONED_CODE: &str = "session_executor_lock_poisoned";

/// A process-local delivery lock whose holder panicked. Its state may be
/// half-written, so the operation that needed it is refused as a retryable
/// `Unavailable` with its original public code ([`DeliveryLock::code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryLock {
    #[error("Memory execution context lock poisoned")]
    MemoryExecutionContext,
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

impl DeliveryLock {
    /// Keep each moved lock's original public error code. The Memory binding
    /// previously projected its refusal through realization; its owner changes,
    /// while callers still receive the same retryable refusal.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MemoryExecutionContext => "session_realization_lock_poisoned",
            Self::ToolAdmission
            | Self::CompletionRegistry
            | Self::ThreadEventHub
            | Self::SessionProjections
            | Self::StepProjections
            | Self::OutcomeCommands => LOCK_POISONED_CODE,
        }
    }
}

impl From<DeliveryLock> for awaken_runtime_contract::runs::RunError {
    fn from(error: DeliveryLock) -> Self {
        Self::unavailable_classified(error.code(), error.to_string())
    }
}

impl From<crate::CompletionSignalClosed> for awaken_runtime_contract::runs::RunError {
    fn from(error: crate::CompletionSignalClosed) -> Self {
        Self::internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_delivery_lock_is_one_retryable_class() {
        // Cause/effect table: any original execution lock -> Unavailable with
        // its existing code; moved Memory context -> the same Unavailable with
        // its original realization code. No caller reclassifies text.
        for lock in [
            DeliveryLock::MemoryExecutionContext,
            DeliveryLock::ToolAdmission,
            DeliveryLock::CompletionRegistry,
            DeliveryLock::ThreadEventHub,
            DeliveryLock::SessionProjections,
            DeliveryLock::StepProjections,
            DeliveryLock::OutcomeCommands,
        ] {
            let run = awaken_runtime_contract::runs::RunError::from(lock);
            assert_eq!(
                run.kind,
                awaken_runtime_contract::runs::RunErrorKind::Unavailable,
                "{lock}"
            );
            let expected = if lock == DeliveryLock::MemoryExecutionContext {
                "session_realization_lock_poisoned"
            } else {
                LOCK_POISONED_CODE
            };
            assert_eq!(run.code, expected, "{lock}");
        }
    }
}
