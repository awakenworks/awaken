//! Resolver identity, host lifetime, and durable worker-handle lookup.

use super::*;

/// Routes a claimed run to the worker that owns its thread, opening (or reusing)
/// the session through the host. Holds a `Weak` back-reference so the pool's tasks
/// never keep the host alive; if the host is dropped, `worker_for` fails and the
/// pool's drains idle out.
pub(crate) struct HostWorkerResolver {
    pub(crate) host: std::sync::Weak<SharedHost>,
}

impl HostWorkerResolver {
    pub(super) fn require_local_execution(
        host: &SharedHost,
    ) -> Result<(), awaken_run_ingress::Error> {
        if host.runs_local_dispatch_pool() {
            Ok(())
        } else {
            Err(Self::execution_error(
                "coordinator-only Host cannot resolve claimed execution locally",
            ))
        }
    }

    pub(in crate::host) fn execution_error(
        message: impl Into<String>,
    ) -> awaken_run_ingress::Error {
        awaken_run_ingress::Error::Execution(awaken_runtime_contract::execution::Error::Execution(
            message.into(),
        ))
    }

    pub(in crate::host) fn terminal_resolution_error(
        message: impl Into<String>,
    ) -> awaken_run_ingress::Error {
        awaken_run_ingress::Error::TerminalResolution(
            awaken_runtime_contract::execution::Error::Execution(message.into()),
        )
    }

    pub(in crate::host) fn host(&self) -> Result<Arc<SharedHost>, awaken_run_ingress::Error> {
        self.host
            .upgrade()
            .ok_or_else(|| Self::execution_error("host dropped; pool idling"))
    }

    pub(super) async fn resolve(
        &self,
        host: &SharedHost,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let agent = agent_id.filter(|a| !a.is_empty());
        let ctx = host
            .ctx_for_snapshot(&thread_id.0, agent, published_snapshot)
            .await
            .map_err(|e| Self::execution_error(e.to_string()))?;
        Ok(ctx.claimed_worker.clone())
    }

    pub(super) async fn resolve_claimed(
        &self,
        host: &SharedHost,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
        published_snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
        attempt: ClaimedRuntimeInput,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let agent = agent_id.filter(|a| !a.is_empty());
        let ctx = host
            .ctx_for_claimed_snapshot(&thread_id.0, agent, published_snapshot, attempt)
            .await
            .map_err(|error| Self::execution_error(error.to_string()))?;
        Ok(ctx.claimed_worker.clone())
    }
}
