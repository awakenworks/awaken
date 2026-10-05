//! Neutral usage of one committed Thread prefix.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_session_budget_contract::SessionUsage;

/// The usage one logical Thread committed, read from the same recovery prefix
/// as its transcript and Run state rather than a second accounting snapshot.
/// A missing Thread prefix is zero usage.
///
/// Consumer: the protocol adapter, which prices it with the Session-frozen
/// price authority for its per-Thread usage projection.
#[must_use]
pub fn committed_thread_usage(snapshot: Option<&RunRecoverySnapshot>) -> SessionUsage {
    snapshot.map_or_else(SessionUsage::default, |snapshot| {
        awaken_session_application_contract::session_usage_from_thread_usage(
            awaken_runtime_contract::llm::ThreadUsage::from_committed_state(&snapshot.state),
        )
    })
}
