//! Environment-free terminal dispatch control and reconciliation.
//!
//! Cancellation and committed-terminal repair share one boundary worker because
//! both need the dispatch fence and Thread commit authority, while neither may be
//! blocked by Session environment provisioning. Only cancellation installs an
//! executor, because repair can settle only a Run already committed as terminal.

use super::*;

/// Deliver one direct cold-recovery fact from the Runtime contract's sole
/// committed-terminal predicate. The observer factory is deliberately lazy:
/// nonterminal or conflicting identity cannot bind resources, probe an outbox,
/// or enter any later Session Environment effect.
async fn reconcile_direct_terminal_from_committed_truth<F, Fut>(
    reader: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
    run_id: &RunId,
    thread_id: &ThreadId,
    observer_factory: F,
) -> Result<bool, HostError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                Vec<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
                HostError,
            >,
        >,
{
    let terminal = match awaken_runtime_contract::terminal::committed_terminal_projection(
        reader, run_id, thread_id,
    ) {
        awaken_runtime_contract::terminal::CommittedTerminalProjection::Nonterminal => {
            return Ok(false);
        }
        awaken_runtime_contract::terminal::CommittedTerminalProjection::IdentityConflict => {
            return Err(HostError::internal(
                "stable Run identity belongs to another Thread",
            ));
        }
        awaken_runtime_contract::terminal::CommittedTerminalProjection::Exact(terminal) => terminal,
    };
    let observers = observer_factory().await?;
    let failures =
        awaken_runtime_contract::terminal::deliver_committed_terminal(&observers, &terminal).await;
    if failures.is_empty() {
        return Ok(true);
    }
    Err(HostError::internal(format!(
        "direct committed-terminal observation failed: {}",
        failures
            .into_iter()
            .map(|failure| format!("{}: {}", failure.observer_id, failure.error))
            .collect::<Vec<_>>()
            .join("; ")
    )))
}

impl SharedHost {
    /// Repair direct-ingress terminal Memory work before ordinary Session
    /// construction can enter the physical Environment lifecycle. This is a
    /// cold recovery wake only: resident contexts already own their observer and
    /// durable/remote delivery has a guarded settlement owner.
    pub(super) async fn reconcile_direct_terminal_before_environment(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<bool, HostError> {
        if self.deployment.durable
            || self.upstream.is_some()
            || self
                .session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        {
            return Ok(false);
        }
        let thread_id = ThreadId(thread.to_string());
        let commit = self.commit_for_read(thread).await?;
        let Some(latest) = commit.latest_run(&thread_id) else {
            return Ok(false);
        };
        reconcile_direct_terminal_from_committed_truth(
            commit.as_ref(),
            &latest.id,
            &thread_id,
            || async {
                let (workspace, _, installed, _) =
                    self.resolve_session_publication(thread, agent, None)?;
                let frozen_model_ref = installed.as_ref().map_or(self.model_ref.as_str(), |root| {
                    root.resolved_spec.model_binding.model_ref.as_str()
                });
                let effective_model_ref =
                    self.inference_routing.model_ref(thread, frozen_model_ref);
                let frozen = installed
                    .as_ref()
                    .map(|root| {
                        crate::agent_catalog::freeze_run_publications(
                            root,
                            self.agent_publications.as_deref(),
                            &workspace,
                        )
                    })
                    .transpose()
                    .map_err(HostError::bad_request)?
                    .unwrap_or_default();
                let source =
                    awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(frozen)
                        .map_err(|error| {
                            HostError::bad_request(format!(
                                "invalid Agent publication closure: {error}"
                            ))
                        })?;
                self.direct_memory_terminal_observer(
                    thread,
                    installed.as_ref(),
                    &effective_model_ref,
                    &source,
                    commit.clone(),
                )
                .await
                .map(|observer| observer.into_iter().collect())
            },
        )
        .await
    }

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
    use async_trait::async_trait;
    use awaken_agent_contract::agent::awaiting::ResumeTicket;
    use awaken_agent_contract::agent::message::Message;
    use awaken_agent_contract::agent::run::Record as RunRecord;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TerminalProjectionView(Option<RunRecord>);

    impl CommittedThreadView for TerminalProjectionView {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
            self.0.clone()
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
            self.0.clone()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn open_wait_for_thread(&self, _thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
            // This terminal-only projection intentionally carries no awaiting
            // ticket, so it has no open wait to expose.
            None
        }
    }

    struct CountingTerminalObserver(Arc<AtomicUsize>);

    #[async_trait]
    impl awaken_runtime_contract::terminal::RunTerminalObserver for CountingTerminalObserver {
        fn observer_id(&self) -> &str {
            "counting-direct-terminal"
        }

        async fn observe(
            &self,
            _terminal: &awaken_runtime_contract::terminal::CommittedTerminalRun,
        ) -> Result<(), awaken_runtime_contract::terminal::RunTerminalObserverError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingTerminalExtractionRepository;

    #[async_trait]
    impl awaken_ext_memory::MemoryExtractionRepository for FailingTerminalExtractionRepository {
        async fn put_extraction_if_absent(
            &self,
            _intent: awaken_ext_memory::MemoryExtractionIntent,
        ) -> Result<
            awaken_ext_memory::PutMemoryExtractionOutcome,
            awaken_ext_memory::MemoryExtractionError,
        > {
            Err(awaken_ext_memory::MemoryExtractionError::Invalid(
                "scripted terminal observer failure".into(),
            ))
        }

        async fn get_extraction(
            &self,
            _intent_id: &str,
        ) -> Result<
            Option<awaken_ext_memory::MemoryExtractionIntent>,
            awaken_ext_memory::MemoryExtractionError,
        > {
            Err(awaken_ext_memory::MemoryExtractionError::Invalid(
                "scripted terminal observer failure".into(),
            ))
        }

        async fn extraction_cursor(
            &self,
            _thread_id: &str,
        ) -> Result<usize, awaken_ext_memory::MemoryExtractionError> {
            Ok(0)
        }

        async fn recoverable_extractions(
            &self,
            _limit: usize,
        ) -> Result<
            Vec<awaken_ext_memory::MemoryExtractionIntent>,
            awaken_ext_memory::MemoryExtractionError,
        > {
            Ok(Vec::new())
        }

        async fn compare_and_swap_extraction(
            &self,
            _expected_revision: u64,
            _intent: awaken_ext_memory::MemoryExtractionIntent,
        ) -> Result<(), awaken_ext_memory::MemoryExtractionError> {
            Err(awaken_ext_memory::MemoryExtractionError::Invalid(
                "scripted terminal observer failure".into(),
            ))
        }
    }

    struct CountingRejectedEnvironmentProvider(Arc<AtomicUsize>);

    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironmentProvider
        for CountingRejectedEnvironmentProvider
    {
        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Ok(())
        }

        async fn create_environment(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
        ) -> Result<
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(awaken_provisioning_contract::SandboxError::new(
                "scripted Environment entry",
            ))
        }

        async fn adopt_environment(
            &self,
            _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        ) -> Result<
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::new(
                "scripted Environment adoption",
            ))
        }
    }

    /// Direct cold-recovery cause/effect decision table:
    ///
    /// | Rule | committed Run | exact Thread identity | Expected effect |
    /// |------|---------------|-----------------------|-----------------|
    /// | R1 | absent/running/awaiting | any | do not construct or call an observer |
    /// | R2 | Ended | different | fail closed without constructing/calling an observer |
    /// | R3 | Ended | exact | construct once and deliver the exact committed fact |
    ///
    /// Constraint: `committed_terminal_projection` remains the only terminal
    /// predicate. This orchestration may sequence effects but cannot reinterpret
    /// Run state or caller-supplied Thread identity.
    #[tokio::test]
    async fn direct_terminal_recovery_constructs_observers_only_for_exact_ended_truth() {
        let run_id = RunId("run-direct-recovery".into());
        let thread_id = ThreadId("thread-direct-recovery".into());

        for state in [None, Some(RunState::Running), Some(RunState::Awaiting)] {
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let observer_calls = Arc::new(AtomicUsize::new(0));
            let view = TerminalProjectionView(state.map(|state| RunRecord {
                id: run_id.clone(),
                thread_id: thread_id.clone(),
                state,
            }));
            let factory_counter = factory_calls.clone();
            let observer_counter = observer_calls.clone();
            let recovered = reconcile_direct_terminal_from_committed_truth(
                &view,
                &run_id,
                &thread_id,
                move || async move {
                    factory_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![Arc::new(CountingTerminalObserver(observer_counter))
                        as Arc<
                            dyn awaken_runtime_contract::terminal::RunTerminalObserver,
                        >])
                },
            )
            .await
            .expect("R1 nonterminal projection");
            assert!(!recovered, "R1");
            assert_eq!(factory_calls.load(Ordering::SeqCst), 0, "R1 factory");
            assert_eq!(observer_calls.load(Ordering::SeqCst), 0, "R1 observer");
        }

        let factory_calls = Arc::new(AtomicUsize::new(0));
        let observer_calls = Arc::new(AtomicUsize::new(0));
        let foreign = TerminalProjectionView(Some(RunRecord {
            id: run_id.clone(),
            thread_id: ThreadId("foreign-thread".into()),
            state: RunState::Ended(EndCause::NaturalEnd),
        }));
        let factory_counter = factory_calls.clone();
        let observer_counter = observer_calls.clone();
        let conflict = reconcile_direct_terminal_from_committed_truth(
            &foreign,
            &run_id,
            &thread_id,
            move || async move {
                factory_counter.fetch_add(1, Ordering::SeqCst);
                Ok(vec![Arc::new(CountingTerminalObserver(observer_counter))
                    as Arc<
                        dyn awaken_runtime_contract::terminal::RunTerminalObserver,
                    >])
            },
        )
        .await
        .expect_err("R2 identity conflict");
        assert!(
            conflict.message.contains("another Thread"),
            "R2: {conflict}"
        );
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0, "R2 factory");
        assert_eq!(observer_calls.load(Ordering::SeqCst), 0, "R2 observer");

        let factory_calls = Arc::new(AtomicUsize::new(0));
        let observer_calls = Arc::new(AtomicUsize::new(0));
        let exact = TerminalProjectionView(Some(RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            state: RunState::Ended(EndCause::NaturalEnd),
        }));
        let factory_counter = factory_calls.clone();
        let observer_counter = observer_calls.clone();
        let recovered = reconcile_direct_terminal_from_committed_truth(
            &exact,
            &run_id,
            &thread_id,
            move || async move {
                factory_counter.fetch_add(1, Ordering::SeqCst);
                Ok(vec![Arc::new(CountingTerminalObserver(observer_counter))
                    as Arc<
                        dyn awaken_runtime_contract::terminal::RunTerminalObserver,
                    >])
            },
        )
        .await
        .expect("R3 exact terminal projection");
        assert!(recovered, "R3");
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1, "R3 factory");
        assert_eq!(observer_calls.load(Ordering::SeqCst), 1, "R3 observer");
    }

    /// Cause/effect decision rule: C1 an exact committed terminal Run, C2 a
    /// writable direct Memory binding, and C3 terminal observer delivery fails.
    /// Effects: E1 `ctx_for` returns the observer error; E2 Session Environment
    /// creation is never entered. Constraint: an already-committed Run is not
    /// rewritten, and retry remains possible through the same observer/outbox.
    #[tokio::test]
    async fn direct_terminal_observer_failure_precedes_every_environment_effect() {
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};

        let creates = Arc::new(AtomicUsize::new(0));
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_session_container_provider(
                Arc::new(CountingRejectedEnvironmentProvider(creates.clone())),
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            ),
        );
        let _managed = crate::host::tests::install_test_dispatch_runtime(&host);
        host.install_memory_extraction_repository(Arc::new(FailingTerminalExtractionRepository));
        crate::host::tests::bind_test_memory(
            &host,
            "direct-terminal-observer-failure",
            "observer-failure-store",
            true,
        );
        let commit = host
            .build_commit("direct-terminal-observer-failure")
            .await
            .expect("commit authority");
        commit_run(
            &commit,
            &ThreadId("direct-terminal-observer-failure".into()),
            RunDisposition::ended(
                RunId("direct-terminal-observer-failure-run".into()),
                EndCause::NaturalEnd,
            ),
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("terminal truth");

        let error = match host.ctx_for("direct-terminal-observer-failure", None).await {
            Ok(_) => panic!("C3/E1 terminal observer failure must abort recovery"),
            Err(error) => error,
        };
        assert!(
            error.message.contains("scripted terminal observer failure"),
            "E1: {error:?}"
        );
        assert_eq!(creates.load(Ordering::SeqCst), 0, "E2");
    }

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
