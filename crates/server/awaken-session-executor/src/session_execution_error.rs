//! The error vocabulary every Session operation of this executor returns, and
//! its one projection to and from the neutral Run error. A Session realization
//! failure enters it in exactly one place, `From<RealizationError>`, through
//! the realization's own Run projection, so its disposition survives.

use awaken_session_execution_contract::ApplicationError;
use awaken_session_mcp::{McpError, McpErrorKind};
use awaken_session_types::RunError;

/// A Session execution failure classified by fault: `BadRequest` is the caller's (bad id,
/// wrong binding, no await), `Internal` is the runtime's. Each adapter maps this
/// to its own public error shape.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SessionExecutionError {
    pub message: String,
    pub kind: SessionExecutionErrorKind,
    /// Stable neutral classification. Protocol adapters project this value and
    /// never infer fault identity from human-readable text.
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionExecutionErrorKind {
    Internal,
    BadRequest,
    Conflict,
    Unavailable,
}

impl SessionExecutionError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self::classified("internal", message)
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SessionExecutionErrorKind::BadRequest,
            code: "invalid_request".into(),
        }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SessionExecutionErrorKind::Conflict,
            code: "conflict".into(),
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::unavailable_classified("unavailable", message)
    }
    pub fn unavailable_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SessionExecutionErrorKind::Unavailable,
            code: code.into(),
        }
    }
    pub fn classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SessionExecutionErrorKind::Internal,
            code: code.into(),
        }
    }
}

pub fn to_run_error(error: SessionExecutionError) -> RunError {
    match error.kind {
        SessionExecutionErrorKind::BadRequest => {
            RunError::bad_request_classified(error.code, error.message)
        }
        SessionExecutionErrorKind::Conflict => {
            RunError::conflict_classified(error.code, error.message)
        }
        SessionExecutionErrorKind::Unavailable if error.code == "unavailable" => {
            RunError::unavailable(error.message)
        }
        SessionExecutionErrorKind::Unavailable => {
            RunError::unavailable_classified(error.code, error.message)
        }
        SessionExecutionErrorKind::Internal if error.code == "internal" => {
            RunError::internal(error.message)
        }
        SessionExecutionErrorKind::Internal => RunError::classified(error.code, error.message),
    }
}

/// The Session executor's error as the neutral application error its Coordinator ports return.
pub(crate) fn to_durable_error(error: SessionExecutionError) -> ApplicationError {
    awaken_session_run_delivery::application_error(to_run_error(error))
}

/// Preserve the neutral Run classification when a durable Session port calls
/// back into Host lifecycle orchestration. This is the sole inverse projector;
/// authorization, receipt persistence, and readback must not each reinterpret
/// retryability from error strings.
pub fn from_run_error(error: RunError) -> SessionExecutionError {
    SessionExecutionError {
        message: error.message,
        kind: match error.kind {
            awaken_session_types::RunErrorKind::BadRequest => SessionExecutionErrorKind::BadRequest,
            awaken_session_types::RunErrorKind::Internal => SessionExecutionErrorKind::Internal,
            awaken_session_types::RunErrorKind::Unavailable => {
                SessionExecutionErrorKind::Unavailable
            }
            awaken_session_types::RunErrorKind::Conflict => SessionExecutionErrorKind::Conflict,
        },
        code: error.code,
    }
}

/// A neutral Run failure from the committed reads and Run delivery this
/// executor builds on keeps its fault and code.
impl From<RunError> for SessionExecutionError {
    fn from(error: RunError) -> Self {
        from_run_error(error)
    }
}

/// The one entry of a realization failure: its Run projection already carries
/// the fault and code its disposition implies.
impl From<awaken_session_realization::realization_error::RealizationError>
    for SessionExecutionError
{
    fn from(error: awaken_session_realization::realization_error::RealizationError) -> Self {
        from_run_error(RunError::from(error))
    }
}

/// A poisoned installed-Sessions registry is refused as the realization
/// failure it is, never read through or recreated.
impl From<awaken_session_realization::realization_error::RealizationLock>
    for SessionExecutionError
{
    fn from(error: awaken_session_realization::realization_error::RealizationLock) -> Self {
        Self::from(awaken_session_realization::realization_error::RealizationError::from(error))
    }
}

/// An executor failure inside a realization port (substrate, frozen
/// projection install) keeps its neutral Run class.
impl From<SessionExecutionError>
    for awaken_session_realization::realization_error::RealizationError
{
    fn from(error: SessionExecutionError) -> Self {
        Self::Run(to_run_error(error))
    }
}

/// Dispatch admission failures keep their Run classification; a realization
/// failure reaches dispatch as its one Run projection.
impl From<awaken_dispatch_execution::AdmissionError> for SessionExecutionError {
    fn from(error: awaken_dispatch_execution::AdmissionError) -> Self {
        from_run_error(RunError::from(error))
    }
}

/// An executor failure crossing into dispatch keeps its neutral Run class.
impl From<SessionExecutionError> for awaken_dispatch_execution::AdmissionError {
    fn from(error: SessionExecutionError) -> Self {
        Self::Run(to_run_error(error))
    }
}

/// A process-local executor lock whose holder panicked. Its state may be
/// half-written, so the operation that needed it is refused; the class is
/// decided once, here, as a retryable `Unavailable` with one stable code.
/// The installed-Session registry has its own class
/// ([`awaken_session_realization::realization_error::RealizationLock`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExecutorLock {
    #[error("Skill catalog cache lock poisoned")]
    SkillCatalogCache,
}

/// Poison `lock` the way production can: a holder panics while holding it.
#[cfg(test)]
pub(crate) fn poison_for_test<T>(lock: &std::sync::Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = lock.lock();
        panic!("poison the lock under test");
    }));
    assert!(lock.is_poisoned());
}

/// Stable code of every [`ExecutorLock`] refusal: the one code the delivery
/// locks share.
pub const EXECUTOR_LOCK_POISONED_CODE: &str = awaken_session_run_delivery::LOCK_POISONED_CODE;

impl From<ExecutorLock> for SessionExecutionError {
    fn from(error: ExecutorLock) -> Self {
        Self::unavailable_classified(EXECUTOR_LOCK_POISONED_CODE, error.to_string())
    }
}

impl From<ExecutorLock> for RunError {
    fn from(error: ExecutorLock) -> Self {
        to_run_error(SessionExecutionError::from(error))
    }
}

/// A poisoned completion relay or Thread event hub is the same retryable
/// refusal as a poisoned executor lock.
impl From<awaken_session_run_delivery::DeliveryLock> for SessionExecutionError {
    fn from(error: awaken_session_run_delivery::DeliveryLock) -> Self {
        from_run_error(RunError::from(error))
    }
}

/// A foreground completion signal that closed before committed truth settled
/// is the executor's internal failure.
impl From<awaken_session_run_delivery::CompletionSignalClosed> for SessionExecutionError {
    fn from(error: awaken_session_run_delivery::CompletionSignalClosed) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<McpError> for SessionExecutionError {
    fn from(error: McpError) -> Self {
        Self {
            message: error.message,
            kind: match error.kind {
                McpErrorKind::Internal => SessionExecutionErrorKind::Internal,
                McpErrorKind::BadRequest => SessionExecutionErrorKind::BadRequest,
                McpErrorKind::Unavailable => SessionExecutionErrorKind::Unavailable,
            },
            code: error.code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_executor_lock_is_one_retryable_class() {
        // Cause: any executor-owned lock is poisoned. Effect: one Unavailable
        // refusal with one stable code, identical in the Host and Run
        // vocabularies, so no caller re-classifies it from text.
        // D2 a delivery lock crosses this boundary: ordinary delivery retains
        // its executor code; the moved Memory context retains its original
        // realization code. Both stay Unavailable through Host -> Run.
        let lock = ExecutorLock::SkillCatalogCache;
        let host = SessionExecutionError::from(lock);
        assert_eq!(host.kind, SessionExecutionErrorKind::Unavailable, "{lock}");
        assert_eq!(host.code, EXECUTOR_LOCK_POISONED_CODE, "{lock}");
        let run = RunError::from(lock);
        assert_eq!(
            run.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "{lock}"
        );
        assert_eq!(run.code, EXECUTOR_LOCK_POISONED_CODE, "{lock}");
        for (lock, expected) in [
            (
                awaken_session_run_delivery::DeliveryLock::ToolAdmission,
                "session_executor_lock_poisoned",
            ),
            (
                awaken_session_run_delivery::DeliveryLock::MemoryExecutionContext,
                "session_realization_lock_poisoned",
            ),
        ] {
            let host = SessionExecutionError::from(lock);
            assert_eq!(host.kind, SessionExecutionErrorKind::Unavailable, "D2");
            assert_eq!(host.code, expected, "D2");
            let run = to_run_error(host);
            assert_eq!(
                run.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "D2"
            );
            assert_eq!(run.code, expected, "D2");
        }
    }

    #[test]
    fn run_and_host_error_projection_preserves_fault_and_code() {
        // Cause/effect table: C1 Run fault is BadRequest/Internal/Unavailable/
        // Conflict; C2 its code is default/custom. Every row must preserve both
        // fields and the original diagnostic through Run -> Host -> Run so
        // lifecycle adapters cannot turn a
        // stale lease into an absorbing Internal error, flatten a conflict into
        // a request fault or erase a stable diagnosis.
        for original in [
            RunError::bad_request("bad"),
            RunError::bad_request_classified("not_found", "missing"),
            RunError::internal("unknown"),
            RunError::classified("custom_internal", "internal"),
            RunError::unavailable("not ready"),
            RunError::unavailable_classified("session_realization_stale", "retry"),
            RunError::conflict("raced"),
            RunError::conflict_classified("projection_changed", "raced"),
        ] {
            let expected_kind = original.kind;
            let expected_code = original.code.clone();
            let expected_message = original.message.clone();
            let projected = to_run_error(from_run_error(original));
            assert_eq!(projected.kind, expected_kind);
            assert_eq!(projected.code, expected_code);
            assert_eq!(projected.message, expected_message);
        }
    }

    #[test]
    fn a_host_conflict_stays_a_conflict_across_the_run_boundary() {
        // Cause: an executor state conflict. Effects: its Run projection is a
        // Conflict with the Host's code, and the inverse restores the Host's
        // Conflict, so every adapter answers 409 conflict.
        let projected = to_run_error(SessionExecutionError::conflict("raced"));
        assert_eq!(projected.kind, awaken_session_types::RunErrorKind::Conflict);
        assert_eq!(projected.code, "conflict");
        let host = from_run_error(projected);
        assert_eq!(host.kind, SessionExecutionErrorKind::Conflict);
        assert_eq!(host.code, "conflict");
        assert_eq!(host.message, "raced");
    }
}
