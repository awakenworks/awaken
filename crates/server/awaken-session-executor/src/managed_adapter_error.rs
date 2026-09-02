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
}
