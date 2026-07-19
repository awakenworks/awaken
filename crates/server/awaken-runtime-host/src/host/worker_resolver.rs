//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;

fn decode_binding(
    encoded: &str,
    expected_sandbox_id: &str,
    run_id: &RunId,
) -> Result<awaken_provisioning_contract::SandboxHandle, awaken_run_ingress::Error> {
    let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(encoded)
        .map_err(|e| {
            HostWorkerResolver::execution_error(format!(
                "run {} has invalid sandbox binding: {e}",
                run_id.0
            ))
        })?;
    if handle.sandbox_id != expected_sandbox_id {
        return Err(HostWorkerResolver::execution_error(format!(
            "run {} sandbox {} does not belong to thread {}",
            run_id.0, handle.sandbox_id, expected_sandbox_id
        )));
    }
    Ok(handle)
}

/// Routes a claimed run to the worker that owns its thread, opening (or reusing)
/// the session through the host. Holds a `Weak` back-reference so the pool's tasks
/// never keep the host alive; if the host is dropped, `worker_for` fails and the
/// pool's drains idle out.
pub(crate) struct HostWorkerResolver {
    pub(crate) host: std::sync::Weak<SharedHost>,
}

impl HostWorkerResolver {
    fn execution_error(message: impl Into<String>) -> awaken_run_ingress::Error {
        awaken_run_ingress::Error::Execution(awaken_runtime_contract::execution::Error::Execution(
            message.into(),
        ))
    }

    fn host(&self) -> Result<Arc<SharedHost>, awaken_run_ingress::Error> {
        self.host
            .upgrade()
            .ok_or_else(|| Self::execution_error("host dropped; pool idling"))
    }

    async fn resolve(
        &self,
        host: &SharedHost,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
        sandbox: Option<LocalSandbox>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let agent = agent_id.filter(|a| !a.is_empty());
        let ctx = host
            .ctx_for_with_sandbox(&thread_id.0, agent, sandbox)
            .await
            .map_err(|e| Self::execution_error(e.to_string()))?;
        ctx.durable_ingress
            .as_ref()
            .map(|ingress| ingress.worker_handle())
            .ok_or_else(|| {
                Self::execution_error(format!("thread {} has no durable ingress", thread_id.0))
            })
    }
}

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for HostWorkerResolver {
    async fn worker_for(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let host = self.host()?;
        // Open the session bound to the claimed run's OWN agent, so `ctx_for` resolves
        // that agent's published config from the worker's config service — its own
        // catalog and model binding. A cold worker thus runs the configured model
        // against a matching fingerprint, with no session-level model registry. An
        // already-resident session is returned from the cache; a cold thread rebuilds
        // from committed truth. `None`/empty opens the built-in default agent.
        self.resolve(&host, thread_id, agent_id, None).await
    }

    async fn worker_for_claimed(
        &self,
        claimed: &awaken_run_ingress::Claimed,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let host = self.host()?;
        let thread_id = claimed.request.session_thread_id();
        let agent_id = claimed.request.activation.snapshot.root_agent_id.0.as_str();
        let agent_id = (!agent_id.is_empty()).then_some(agent_id);

        let adopted = if let Some(encoded) = &claimed.sandbox {
            let handle = decode_binding(encoded, &thread_id.0, &claimed.lease.run_id)?;
            let sandbox = host
                .provider
                .adopt_sandbox(&handle)
                .await
                .map_err(|e| Self::execution_error(e.to_string()))?;
            if sandbox
                .status()
                .await
                .map_err(|e| Self::execution_error(e.to_string()))?
                != awaken_provisioning_contract::SandboxStatus::Ready
            {
                return Err(Self::execution_error(format!(
                    "run {} sandbox {} is no longer available",
                    claimed.lease.run_id.0, handle.sandbox_id
                )));
            }
            Some(sandbox)
        } else {
            None
        };

        let worker = self.resolve(&host, thread_id, agent_id, adopted).await?;

        // Persist the first placement before executing the claimed run. If the
        // process dies after this write, the next owner sees the handle and adopts
        // the same environment; a failed write leaves the run unexecuted/retryable.
        if claimed.sandbox.is_none() {
            let ctx = host
                .sessions
                .lock()
                .await
                .get(&thread_id.0)
                .cloned()
                .ok_or_else(|| Self::execution_error("resolved session disappeared"))?;
            let encoded = serde_json::to_string(&ctx.env.handle())
                .map_err(|e| Self::execution_error(e.to_string()))?;
            crate::dispatch_backend::shared_durable_store(host.store_dir.as_deref())
                .map_err(|e| Self::execution_error(e.to_string()))?
                .bind_sandbox(&claimed.lease.run_id, &encoded)
                .await
                .map_err(awaken_run_ingress::Error::from)?;
        }
        Ok(worker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_binding_is_validated_before_provider_adoption() {
        let run = RunId("run-binding".into());
        assert!(decode_binding("not-json", "thread-a", &run).is_err());

        let wrong = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "local", "thread-b",
        ))
        .unwrap();
        assert!(decode_binding(&wrong, "thread-a", &run).is_err());

        let valid = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "local", "thread-a",
        ))
        .unwrap();
        assert_eq!(
            decode_binding(&valid, "thread-a", &run).unwrap().sandbox_id,
            "thread-a"
        );
    }
}
