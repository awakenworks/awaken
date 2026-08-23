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
        if let Some(observer) = host.dispatch_settlement_observer() {
            worker = worker.with_settlement_observer(observer);
        }
        if host.upstream.is_none()
            && let Some(observer) = host
                .dispatched_memory_terminal_observer(&claimed.request, commit.clone())
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?
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
                awaken_runtime_contract::resolved::Backend::Remote(_)
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
    clock: Arc<dyn awaken_run_ingress::Clock>,
    limit: usize,
) -> Result<Vec<(RunId, RunState)>, awaken_run_ingress::Error> {
    let now_ms = clock.now_ms();
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

        // The dispatch summary already carries the canonical Session affinity.
        // Read committed truth from that physical partition before taking the
        // maintenance claim: claiming every Awaiting/expired row and restoring
        // non-terminal ones to Awaiting would steal a recoverable lease from a
        // registered replacement Worker and advance its fencing epoch forever.
        // `settle_claimed_terminal` repeats this check after the claim, closing
        // the read/claim race without introducing another recovery registry.
        let commit_thread = row.session_thread_id.as_ref().unwrap_or(&row.thread_id);
        let commit = Arc::new(
            host.build_commit(&commit_thread.0)
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
        if let Some(terminal) = worker
            .settle_claimed_terminal(claimed, clock.clone())
            .await?
        {
            reconciled.push(terminal);
        }
    }
    Ok(reconciled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{AdoptionModel, claim, test_activation};

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

    #[tokio::test]
    async fn coordinator_terminal_scan_preserves_nonterminal_replacement_authority() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 a Coordinator-only active-active peer scans the
        // shared durable store; C2 a row is quiescent Awaiting without committed
        // Ended truth; C3 another row is Leased by Worker A, its lease expires,
        // and committed truth is still non-terminal; C4 Worker B reclaims C3;
        // C5 Worker A later settles with its old epoch. Effects: E1 the
        // coordinator does not claim or advance either row; E2 B receives the
        // next epoch through the existing Dispatch owner; E3 A is fenced while
        // B remains current. The Awaiting probe checks its exact next epoch, so
        // a claim-and-restore maintenance hot loop cannot hide behind unchanged
        // public status.
        //
        // | Rule | Queue phase | committed Ended | lease | actor | Effect |
        // |---|---|---|---|---|---|
        // | A1 | Awaiting | no | none | coordinator scan | E1 preserve next epoch |
        // | A2 | Leased | no | expired | coordinator scan | E1 leave recovery to Worker |
        // | A3 | Leased | no | expired | Worker B | E2 reclaim epoch + 1 |
        // | A4 | Leased | no | superseded | Worker A | E3 fenced |
        use awaken_run_ingress::{Clock, DispatchQueue, SettleOutcome};

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
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let parked = claim(&store, "thread-awaiting", "run-awaiting", "worker-a", now).await;
        assert_eq!(
            store
                .settle(
                    &parked.lease.run_id,
                    parked.lease.epoch,
                    awaken_run_ingress::DispatchOutcome::Awaiting,
                    &[],
                )
                .await
                .expect("park Awaiting row"),
            SettleOutcome::Applied
        );
        let leased = claim(&store, "thread-leased", "run-leased", "worker-a", now).await;
        let recovery_now = leased.lease.expires_ms.saturating_add(1);

        assert!(
            reconcile_committed_terminals(
                &resolver,
                Arc::new(awaken_run_ingress::ManualClock::new(recovery_now)),
                8,
            )
            .await
            .expect("scan non-terminal rows")
            .is_empty(),
            "A1/A2: non-terminal rows are not maintenance work"
        );

        let awaiting_probe = store
            .claim_for_terminal_recovery(
                &parked.lease.run_id,
                "epoch-probe",
                DEFAULT_LEASE_MS,
                recovery_now,
            )
            .await
            .expect("probe Awaiting epoch")
            .expect("Awaiting row remains available to its canonical recovery verb");
        assert_eq!(
            awaiting_probe.lease.epoch,
            parked.lease.epoch + 1,
            "A1/E1: coordinator scan did not invisibly claim and restore Awaiting"
        );
        store
            .settle(
                &awaiting_probe.lease.run_id,
                awaiting_probe.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("restore Awaiting probe");

        let replacement = store
            .claim_run(
                &leased.lease.run_id,
                "worker-b",
                DEFAULT_LEASE_MS,
                recovery_now,
                &Default::default(),
            )
            .await
            .expect("Worker B reclaim")
            .expect("A3: expired lease remains reclaimable");
        assert_eq!(replacement.lease.owner, "worker-b", "A3/E2");
        assert_eq!(replacement.lease.epoch, leased.lease.epoch + 1, "A3/E2");
        assert_eq!(
            store
                .settle(
                    &leased.lease.run_id,
                    leased.lease.epoch,
                    awaken_run_ingress::DispatchOutcome::Done,
                    &[],
                )
                .await
                .expect("stale Worker A settle"),
            SettleOutcome::Fenced,
            "A4/E3"
        );
        assert!(
            store
                .lock_commit_epoch(&awaken_run_ingress::RunClaim::from(&replacement.lease))
                .await
                .expect("current replacement fence read")
                .is_some(),
            "A4/E3: stale settlement cannot disturb Worker B"
        );
    }

    #[tokio::test]
    async fn host_retry_exhaustion_uses_the_environment_free_boundary_worker() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect table: H1 expired+exhausted exact claim -> boundary Worker
        // commits Indeterminate and Done; H2 no Session/Environment exists -> no
        // realization is attempted; H3 current epoch -> one completion tombstone.
        use awaken_run_ingress::{Clock, DispatchQueue, WorkerResolver};

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
        let fresh = claim(&store, "thread-exhausted", "run-exhausted", "crashed", now).await;
        store
            .claim_run(
                &fresh.lease.run_id,
                "crashed",
                1_000,
                fresh.lease.expires_ms + 1,
                &Default::default(),
            )
            .await
            .expect("recovery claim")
            .expect("expired row recovers");
        let claimed = store
            .claim_retry_exhausted("remote-worker", 1_000, now + 2_002, 1)
            .await
            .expect("special claim")
            .expect("exhausted row claims");
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        assert_eq!(
            resolver
                .terminalize_retry_exhausted(
                    &claimed,
                    Arc::new(awaken_run_ingress::ManualClock::new(now + 2_002)),
                )
                .await
                .expect("H1 terminalization"),
            Some((
                RunId("run-exhausted".into()),
                RunState::Ended(EndCause::Indeterminate),
            )),
            "H1"
        );
        assert!(
            host.session_environment("thread-exhausted").await.is_none(),
            "H2"
        );
        assert!(store.list_dispatches().await.expect("H1 rows").is_empty());
        assert_eq!(
            store
                .completion_events_after(0, 10)
                .await
                .expect("H3 completions")
                .len(),
            1,
            "H3"
        );
    }

    #[tokio::test]
    async fn terminal_recovery_opens_the_parent_partition_for_a_logical_child() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 queue Thread differs from parent affinity; C2
        // committed Ended truth exists only in the parent physical partition;
        // C3 the terminal-recovery claim is current. Effects: E1 claim supplies
        // trusted parent affinity before commit open; E2 child logical Run is
        // found and settled Done; E3 no child-named Session/environment is
        // created. Rule P1=C1+C2+C3=>E1+E2+E3. An implementation that opens the
        // queue Thread before claiming leaves the row Awaiting on FS/SQLite.
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};
        use awaken_run_ingress::{Clock, DispatchQueue, SessionChildAdmission};

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
        let parent = ThreadId("parent-session".into());
        let child = ThreadId("logical-child".into());
        let run = RunId("child-terminal".into());
        store
            .enqueue_session_child(
                awaken_run_ingress::RunDispatch::new(test_activation(&child.0, &run.0))
                    .for_session(parent.clone()),
                SessionChildAdmission::new(25, Vec::new()),
            )
            .await
            .expect("admit child");
        let claimed = store
            .claim_run(&run, "setup", DEFAULT_LEASE_MS, now, &Default::default())
            .await
            .expect("claim child")
            .expect("child claim");
        store
            .settle(
                &run,
                claimed.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("park child");
        let parent_commit = host
            .build_commit(&parent.0)
            .await
            .expect("parent physical commit");
        commit_run(
            &parent_commit,
            &child,
            RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("commit logical child in parent partition");

        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        let reconciled = reconcile_committed_terminals(
            &resolver,
            Arc::new(awaken_run_ingress::ManualClock::new(now + 1)),
            8,
        )
        .await
        .expect("reconcile child");
        assert_eq!(reconciled.len(), 1, "P1/E2");
        assert_eq!(reconciled[0].0, run, "P1/E2");
        assert!(store.list_dispatches().await.unwrap().is_empty(), "P1/E2");
        assert!(host.session_environment(&child.0).await.is_none(), "P1/E3");
    }
}
