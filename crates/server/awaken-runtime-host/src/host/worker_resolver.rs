//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;
mod claimed_dispatch;
mod claimed_session;
#[cfg(test)]
mod dispatched_mcp_tests;
mod resolver;
mod session_realization;
#[cfg(test)]
pub(super) mod test_support;
use claimed_session::install_claimed_session_projection;
pub(crate) use resolver::HostWorkerResolver;

#[cfg(test)]
#[path = "worker_resolver/tests.rs"]
mod tests;
