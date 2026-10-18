//! The dispatch owner a Session executor reads from its authority, and the
//! executor's handle on this process's dispatch.

use crate::host::*;

/// The Coordinator dispatch owner when this process's authority holds it
/// locally; a database-less Worker reaches dispatch only remotely.
pub(crate) fn local_dispatch_owner(
    authority: &dyn crate::SessionAuthority,
) -> Option<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>> {
    match authority.dispatch_binding()?.placement() {
        awaken_dispatch_erasure::DispatchTopology::Local(owner) => Some(owner.clone()),
        awaken_dispatch_erasure::DispatchTopology::Remote { .. } => None,
    }
}

impl SharedHost {
    /// This process's dispatch: topology, pool, terminal reconciliation, hint.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<awaken_dispatch_execution::ProcessDispatch> {
        &self.dispatch
    }
}
