//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;

/// Routes a claimed run to the worker that owns its thread, opening (or reusing)
/// the session through the host. Holds a `Weak` back-reference so the pool's tasks
/// never keep the host alive; if the host is dropped, `worker_for` fails and the
/// pool's drains idle out.
pub(crate) struct HostWorkerResolver {
    pub(crate) host: std::sync::Weak<SharedHost>,
}

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for HostWorkerResolver {
    async fn worker_for(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let exec_err = |message: String| {
            awaken_run_ingress::Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(message),
            )
        };
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| exec_err("host dropped; pool idling".to_string()))?;
        // Open the session bound to the claimed run's OWN agent, so `ctx_for` resolves
        // that agent's published config from the worker's config service — its own
        // catalog and model binding. A cold worker thus runs the configured model
        // against a matching fingerprint, with no session-level model registry. An
        // already-resident session is returned from the cache; a cold thread rebuilds
        // from committed truth. `None`/empty opens the built-in default agent.
        let agent = agent_id.filter(|a| !a.is_empty());
        let ctx = host
            .ctx_for(&thread_id.0, agent)
            .await
            .map_err(|e| exec_err(e.to_string()))?;
        ctx.durable_ingress
            .as_ref()
            .map(|ingress| ingress.worker_handle())
            .ok_or_else(|| exec_err(format!("thread {} has no durable ingress", thread_id.0)))
    }
}
