//! Host-composed dispatch integration tests; production routing belongs to dispatch-execution.
use super::*;
mod claimed_session;
mod dispatched_mcp_tests;
mod realization_failure;
mod session_projection;
pub(super) mod test_support;
#[path = "worker_resolver/tests.rs"]
mod tests;
