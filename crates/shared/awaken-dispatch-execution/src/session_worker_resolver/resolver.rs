//! Resolver identity, host lifetime, and durable worker-handle lookup.

use super::{
    Arc, RunWorkerFactory, SessionDispatch, SessionProjectionExecution, TerminalRecovery,
    execution_error,
};

/// Routes a claimed run to the worker that owns its thread, opening (or reusing)
/// the session through the host. Holds a `Weak` back-reference so the pool's tasks
/// never keep the host alive; if the host is dropped, `worker_for` fails and the
/// pool's drains idle out.
pub struct SessionWorkerResolver<T> {
    pub host: std::sync::Weak<T>,
}

impl<T> SessionWorkerResolver<T>
where
    T: SessionDispatch + RunWorkerFactory + SessionProjectionExecution + TerminalRecovery,
{
    pub fn require_local_execution(host: &T) -> Result<(), crate::Error> {
        if host.runs_local_dispatch_pool() {
            Ok(())
        } else {
            Err(execution_error(
                "coordinator-only Host cannot resolve claimed execution locally",
            ))
        }
    }

    pub fn host(&self) -> Result<Arc<T>, crate::Error> {
        self.host
            .upgrade()
            .ok_or_else(|| execution_error("host dropped; pool idling"))
    }
}
