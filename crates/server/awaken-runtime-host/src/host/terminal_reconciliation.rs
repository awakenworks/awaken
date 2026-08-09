//! Environment-free terminal dispatch control and reconciliation.
//!
//! Cancellation and committed-terminal repair share one boundary worker because
//! both need the dispatch fence and Thread commit authority, while neither may be
//! blocked by Session environment provisioning. Only cancellation installs an
//! executor, because repair can settle only a Run already committed as terminal.

use super::*;

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
            || self.dispatch_maintenance.get().is_some()
        {
            return false;
        }
        let Ok(store) = self.dispatch_store() else {
            return false;
        };
        let resolver: Arc<dyn awaken_run_ingress::WorkerResolver<AnyDispatchStore>> =
            Arc::new(HostWorkerResolver {
                host: Arc::downgrade(self),
            });
        let wake = self
            .authority
            .as_ref()
            .and_then(|authority| authority.dispatch_wake())
            .unwrap_or_else(|| Arc::new(awaken_run_ingress::LocalWakeSignal::new()));
        let maintenance = awaken_run_ingress::DispatchMaintenance::spawn(
            store,
            Arc::new(awaken_run_ingress::SystemClock),
            wake,
            awaken_run_ingress::DispatchServiceConfig::default(),
            resolver,
            Some(self.completion.clone()),
        );
        self.dispatch_maintenance.set(maintenance).is_ok()
    }
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

    pub(super) async fn boundary_worker(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{AdoptionModel, claim};

    /// Cause/effect decision table for Host-wide legacy reconciliation:
    ///
    /// | Rule | Host role | Dispatch | Thread commit | Expected effect |
    /// |------|-----------|----------|---------------|-----------------|
    /// | R0 | local pool | any | any | no duplicate coordinator daemon |
    /// | R1 | coordinator-only | Awaiting | no terminal Run | preserve row and Session state |
    /// | R2 | coordinator-only | Awaiting | exact Run Ended | environment-free fenced Done |
    /// | R3 | coordinator-only + R2 | exact settlement | exact terminal | completion tombstone, no Sandbox |
    ///
    /// Constraints: each row is read through its own Thread commit boundary;
    /// neither queue metadata nor environment availability can prove outcome.
    #[tokio::test]
    async fn host_reconciles_only_exact_committed_terminals_without_opening_sessions() {
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};
        use awaken_run_ingress::{Clock, DispatchQueue};

        let mut local_pool = SharedHost::new(Arc::new(AdoptionModel), "stub");
        local_pool.deployment.durable = true;
        local_pool.deployment.disable_local_pool = false;
        assert!(
            !Arc::new(local_pool).ensure_terminal_dispatch_reconciliation(),
            "R0"
        );

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let mut coordinator = SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_store_dir(storage.path())
            .with_dispatch_store(store.clone());
        coordinator.deployment.durable = true;
        coordinator.deployment.disable_local_pool = true;
        let host = Arc::new(coordinator);

        let control = claim(&store, "thread-control", "run-control", "setup", now).await;
        store
            .settle(
                &control.lease.run_id,
                control.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("quiesce control");
        let terminal = claim(&store, "thread-terminal", "run-terminal", "setup", now).await;
        store
            .settle(
                &terminal.lease.run_id,
                terminal.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("quiesce terminal candidate");

        let commit = host
            .build_commit("thread-terminal")
            .await
            .expect("terminal Thread commit");
        commit_run(
            &commit,
            &ThreadId("thread-terminal".to_string()),
            RunDisposition::ended(RunId("run-terminal".to_string()), EndCause::NaturalEnd),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("record exact terminal truth");

        assert!(host.ensure_terminal_dispatch_reconciliation());
        assert!(
            !host.ensure_terminal_dispatch_reconciliation(),
            "one coordinator daemon"
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if store
                    .list_dispatches()
                    .await
                    .expect("reconciliation rows")
                    .len()
                    == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("R2 coordinator daemon reconciles immediately");
        let rows = store.list_dispatches().await.expect("remaining dispatches");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id, RunId("run-control".to_string()), "R1");
        assert!(host.session_environment("thread-control").await.is_none());
        assert!(
            host.session_environment("thread-terminal").await.is_none(),
            "R3"
        );
        let completions = store
            .completion_events_after(0, 10)
            .await
            .expect("completion tombstone");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].run_id, RunId("run-terminal".to_string()));
    }
}
