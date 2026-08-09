//! Environment-free terminal dispatch control and reconciliation.
//!
//! Cancellation and committed-terminal repair share one boundary worker because
//! both need the dispatch fence and Thread commit authority, while neither may be
//! blocked by Session environment provisioning. Only cancellation installs an
//! executor, because repair can settle only a Run already committed as terminal.

use super::*;
use awaken_run_ingress::Clock as _;

const RECONCILIATION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

impl SharedHost {
    /// Start the one coordinator-side reconciliation loop when this Host owns
    /// durable dispatch/commit truth but intentionally does not run an execution
    /// pool. A local-pool Host already performs the same maintenance in its pool;
    /// a database-less Worker has no global committed reader and must not start it.
    /// Returns whether this call started the loop.
    pub fn ensure_terminal_dispatch_reconciliation(self: &Arc<Self>) -> bool {
        if !self.deployment.durable
            || self.runs_local_dispatch_pool()
            || self.upstream.is_some()
            || self.terminal_reconciliation_started.set(()).is_err()
        {
            return false;
        }
        spawn_reconciliation_loop(Arc::downgrade(self), RECONCILIATION_INTERVAL);
        true
    }
}

fn spawn_reconciliation_loop(host: std::sync::Weak<SharedHost>, interval: std::time::Duration) {
    tokio::spawn(async move {
        loop {
            let Some(host) = host.upgrade() else {
                break;
            };
            let resolver = HostWorkerResolver {
                host: Arc::downgrade(&host),
            };
            let now_ms = awaken_run_ingress::SystemClock.now_ms();
            if let Err(error) = reconcile_committed_terminals(&resolver, now_ms, 256).await {
                tracing::warn!(%error, "coordinator terminal dispatch reconciliation failed; retrying");
            }
            drop(host);
            tokio::time::sleep(interval).await;
        }
    });
}

impl HostWorkerResolver {
    pub(super) async fn cancellation_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_run_ingress::Claimed,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.build_commit(&thread_id.0)
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?,
        );
        self.boundary_worker(host, claimed, commit, true).await
    }

    async fn boundary_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_run_ingress::Claimed,
        commit: Arc<crate::store::HostCommit>,
        install_cancellation_executor: bool,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let thread_id = claimed.request.session_thread_id();
        let recovery_projection = commit.recovery_projection();
        let store = host
            .dispatch_store()
            .map_err(|error| Self::execution_error(error.to_string()))?;
        let mut worker = awaken_run_ingress::DispatchWorker::new(
            Arc::new(awaken_runtime::Runtime::new()),
            store,
            commit.clone(),
            claimed.lease.owner.clone(),
        );
        if host.upstream.is_none()
            && let Some(observer) = host
                .memory_terminal_observer(
                    &thread_id.0,
                    &claimed.request.activation.snapshot,
                    commit,
                )
                .await
        {
            worker = worker.with_context(
                awaken_runtime_contract::RuntimeRunContext::new().with_terminal_observer(observer),
            );
        }
        if install_cancellation_executor
            && matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &claimed
                        .request
                        .activation
                        .snapshot
                        .resolved_spec
                        .model_binding
                        .backend_ref
                ),
                awaken_runtime_contract::resolved::Backend::Remote { .. }
            )
        {
            let executor = host.remote_attempt_executor.clone().ok_or_else(|| {
                Self::execution_error(
                    "remote cancellation requires a configured remote attempt executor",
                )
            })?;
            worker.install_attempt_executor(executor);
        }
        if let Some(upstream) = &host.upstream {
            let claimed_commit = crate::commit_ingest::remote_claimed_commit(upstream)
                .map_err(|error| Self::execution_error(error.to_string()))?;
            worker = worker.with_claimed_commit(claimed_commit);
        }
        if let Some(projection) = recovery_projection {
            worker = worker.with_recovery_projection(projection);
        }
        Ok(Arc::new(worker))
    }
}

pub(super) async fn reconcile_committed_terminals(
    resolver: &HostWorkerResolver,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<(RunId, RunState)>, awaken_run_ingress::Error> {
    let host = resolver.host()?;
    // A database-less Worker delegates committed truth upstream and has no
    // process-local registry from which to discover every Thread reader.
    if host.upstream.is_some() || limit == 0 {
        return Ok(Vec::new());
    }

    let store = host
        .dispatch_store()
        .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
    let rows = store.list_dispatches().await?;
    let mut reconciled = Vec::new();
    for row in rows.into_iter().filter(|row| {
        matches!(
            row.state,
            awaken_run_ingress::DispatchState::Awaiting | awaken_run_ingress::DispatchState::Leased
        )
    }) {
        if reconciled.len() >= limit {
            break;
        }

        let commit = Arc::new(
            host.build_commit(&row.thread_id.0)
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?,
        );
        if !matches!(commit.run_state(&row.run_id), Some(RunState::Ended(_))) {
            continue;
        }
        let Some(claimed) = store
            .claim_for_terminal_recovery(
                &row.run_id,
                &host.deployment.dispatch_owner,
                DEFAULT_LEASE_MS,
                now_ms,
            )
            .await?
        else {
            continue;
        };

        let worker = match resolver
            .boundary_worker(&host, &claimed, commit, false)
            .await
        {
            Ok(worker) => worker,
            Err(error) => {
                let _ = store
                    .settle(
                        &claimed.lease.run_id,
                        claimed.lease.epoch,
                        awaken_run_ingress::DispatchOutcome::Awaiting,
                        &[],
                    )
                    .await;
                return Err(error);
            }
        };
        if let Some(terminal) = worker.settle_claimed_terminal(claimed).await? {
            reconciled.push(terminal);
        }
    }
    Ok(reconciled)
}
