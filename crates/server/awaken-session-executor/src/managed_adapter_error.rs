//! Error translation owned by the Managed `SessionRuntime` adapter.

use awaken_runtime_contract::live_inbox::EditError;
use awaken_session_execution_contract::LiveInboxError;

pub(crate) use crate::host_error::{from_run_error, to_run_error};

pub(super) fn to_live_inbox_error(error: EditError) -> LiveInboxError {
    match error {
        EditError::Closed => LiveInboxError::Inactive,
        EditError::UnknownMessage => LiveInboxError::UnknownMessage,
        EditError::StaleOrder => LiveInboxError::StaleOrder,
    }
}
