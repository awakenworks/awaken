//! Error translation owned by the Managed `SessionRuntime` adapter.

use awaken_runtime_contract::live_inbox::EditError;
use awaken_session_contract::{LiveInboxError, RunError};

use crate::{HostError, HostErrorKind};

pub(super) fn to_live_inbox_error(error: EditError) -> LiveInboxError {
    match error {
        EditError::Closed => LiveInboxError::Inactive,
        EditError::UnknownMessage => LiveInboxError::UnknownMessage,
        EditError::StaleOrder => LiveInboxError::StaleOrder,
    }
}

pub(crate) fn to_run_error(error: HostError) -> RunError {
    match error.kind {
        HostErrorKind::BadRequest | HostErrorKind::Conflict => RunError::bad_request(error.message),
        HostErrorKind::Unavailable if error.code == "unavailable" => {
            RunError::unavailable(error.message)
        }
        HostErrorKind::Unavailable => RunError::unavailable_classified(error.code, error.message),
        HostErrorKind::Internal if error.code == "internal" => RunError::internal(error.message),
        HostErrorKind::Internal => RunError::classified(error.code, error.message),
    }
}

/// Preserve the neutral Run classification when a durable Session port calls
/// back into Host lifecycle orchestration. This is the sole inverse projector;
/// authorization, receipt persistence, and readback must not each reinterpret
/// retryability from error strings.
pub(crate) fn from_run_error(error: RunError) -> HostError {
    HostError {
        message: error.message,
        kind: match error.kind {
            awaken_session_contract::RunErrorKind::BadRequest => HostErrorKind::BadRequest,
            awaken_session_contract::RunErrorKind::Internal => HostErrorKind::Internal,
            awaken_session_contract::RunErrorKind::Unavailable => HostErrorKind::Unavailable,
        },
        code: error.code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_host_error_projection_preserves_fault_and_code() {
        // Cause/effect table: C1 Run fault is BadRequest/Internal/Unavailable;
        // C2 its code is default/custom. Every row must preserve both fields
        // through Run -> Host -> Run so lifecycle adapters cannot turn a stale
        // lease into an absorbing Internal error or erase a stable diagnosis.
        for original in [
            RunError::bad_request("bad"),
            RunError::classified("custom_internal", "internal"),
            RunError::unavailable_classified("session_realization_stale", "retry"),
        ] {
            let expected_kind = original.kind;
            let expected_code = original.code.clone();
            let projected = to_run_error(from_run_error(original));
            assert_eq!(projected.kind, expected_kind);
            assert_eq!(projected.code, expected_code);
        }
    }
}
