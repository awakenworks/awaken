// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! The Session executor's dispatch composition: the claimed-Run resolver the
//! process dispatch starts its pool and terminal reconciliation with, and the
//! drain that joins auxiliary work after the pool settled.

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

    /// Stop local durable admission, await every already-claimed Run, then join
    /// auxiliary work created by those Runs.  The ordering is significant: a
    /// terminal Run may enqueue Memory extraction immediately before releasing
    /// its pool capacity, so draining auxiliary work first can miss that task.
    pub async fn drain_runtime(&self, timeout: std::time::Duration) -> bool {
        let deadline = awaken_time::mono::MonoDeadline::after(timeout);
        let Some(_claimed_runs) = self.dispatch.drain_claimed_runs(&deadline).await else {
            return false;
        };
        let remaining = deadline.remaining();
        !remaining.is_zero() && self.resources.memory.drain(remaining).await
    }

    /// Spawn the process dispatch pool over this executor's claimed-Run
    /// resolver, once, whenever this process owns local execution and its
    /// dispatch authority is bound. A coordinator-only process
    /// ([`awaken_dispatch_execution::LocalExecution::RemoteWorkersOnly`]) stays
    /// admission-only. Idempotent.
    pub fn ensure_dispatch_pool(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.dispatch
            .ensure_pool(self.authority.dispatch_binding(), || {
                let resolver =
                    crate::host::session_worker_resolver(&self.run_assembly, &self.dispatch)
                        .map_err(|error| {
                            awaken_dispatch_execution::session_execution::execution_error(
                                error.message,
                            )
                        })?;
                Ok(Arc::new(resolver) as Arc<dyn WorkerResolver<AnyDispatchStore>>)
            })
    }

    /// Start the coordinator-side terminal reconciliation loop over this
    /// executor's resolver when the process owns durable dispatch and committed
    /// truth but runs no pool. Returns whether this call started the loop.
    pub fn ensure_terminal_dispatch_reconciliation(&self) -> bool {
        self.dispatch.ensure_terminal_reconciliation(
            self.authority.scans_committed_terminals(),
            local_dispatch_owner(self.authority.as_ref()),
            || {
                crate::host::session_worker_resolver(&self.run_assembly, &self.dispatch)
                    .ok()
                    .map(|resolver| Arc::new(resolver) as Arc<dyn WorkerResolver<AnyDispatchStore>>)
            },
        )
    }
}
