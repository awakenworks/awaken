// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: neutral token accounting projected from the same committed Thread recovery
// @formal reason: prefix as transcript and Run state. Dynamic path: Managed reads no second usage snapshot
// @formal reason: and applies only the Session-frozen price authority; this adds no accounting store or
// @formal reason: state machine.
//! Usage projection from the same committed Thread prefix as transcript recovery.

use super::*;

/// Read and price one logical Thread's usage without opening an independent
/// repository snapshot. A missing Thread snapshot and an empty committed usage
/// cell both retain the Managed wire's optional `null` representation.
pub(super) fn project_committed_thread_usage(
    snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    price_snapshot: Option<&awaken_session_contract::ManagedListPriceSnapshot>,
) -> Result<Option<crate::types::SessionThreadUsage>, StateError> {
    let usage = awaken_managed_session_events::committed_thread_usage(snapshot);
    session_thread_usage_value(usage, price_snapshot).map_err(StateError::Run)
}
