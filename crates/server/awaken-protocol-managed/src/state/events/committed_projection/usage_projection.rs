//! Usage projection from the same committed Thread prefix as transcript recovery.

use super::*;

/// Read and price one logical Thread's usage without opening an independent
/// repository snapshot. A missing Thread snapshot and an empty committed usage
/// cell both retain the Managed wire's optional `null` representation.
pub(super) fn project_committed_thread_usage(
    snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    price_snapshot: Option<&awaken_session_contract::ManagedListPriceSnapshot>,
) -> Result<Option<crate::types::SessionThreadUsage>, StateError> {
    let usage = snapshot.map_or_else(SessionUsage::default, |snapshot| {
        awaken_runtime_contract::llm::ThreadUsage::from_committed_state(&snapshot.state).into()
    });
    session_thread_usage_value(usage, price_snapshot).map_err(StateError::Run)
}
