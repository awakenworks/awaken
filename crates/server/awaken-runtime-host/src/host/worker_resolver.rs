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
        model_ref: Option<&str>,
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
        // Bind the claimed run's own model to the thread BEFORE the session's runtime
        // is built. A worker process has no config service and no per-session model
        // binding, so without this a cold thread would fall back to the host default
        // executor; with it, `ctx_for`'s `resolve_executor` picks up the registered
        // ref and the executor provider (e.g. ConfigExecutorProvider) resolves the
        // run's configured model. Skipped when empty (leaves today's default).
        if let Some(model_ref) = model_ref.filter(|m| !m.is_empty()) {
            host.register_thread_model(&thread_id.0, model_ref);
        }
        // Reopen the session with the default agent binding: an already-resident
        // session (the common case) is returned from the cache with its original
        // binding; a cold thread after a restart rebuilds from committed truth.
        let ctx = host
            .ctx_for(&thread_id.0, None)
            .await
            .map_err(|e| exec_err(e.to_string()))?;
        ctx.durable_ingress
            .as_ref()
            .map(|ingress| ingress.worker_handle())
            .ok_or_else(|| exec_err(format!("thread {} has no durable ingress", thread_id.0)))
    }
}
