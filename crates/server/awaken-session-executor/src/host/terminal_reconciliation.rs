// @formal surface obligation=session_execution.terminal_state_never_reopens
// @formal surface obligation=session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact
// @formal surface obligation=checkpoint.terminal_completion_clears_resume
// @formal reason: Terminal reconciliation from committed Run truth.
//! Environment-free terminal dispatch control and reconciliation.
//!
//! Cancellation and committed-terminal repair share one boundary worker because
//! both need the dispatch fence and Thread commit authority, while neither may be
//! blocked by Session environment provisioning. Only cancellation installs an
//! executor, because repair can settle only a Run already committed as terminal.

use super::*;
use awaken_dispatch_contract::ControlDispatchQueue;

/// The coordinator-only maintenance loop and the terminal scan it drives.
/// Consumer: this module alone; the Host's dispatch capability owns the value.
#[derive(Default)]
pub(crate) struct TerminalReconciliationState {
    /// The run-ingress-owned maintenance loop used by a coordinator-only Host.
    /// Local pools already own the same canonical maintenance mechanism.
    dispatch_reconciler: std::sync::OnceLock<awaken_dispatch_execution::DispatchReconciler>,
    terminal_scan: awaken_dispatch_execution::TerminalReconciliationScan,
    worker_evidence: std::sync::OnceLock<(
        Arc<dyn awaken_dispatch_contract::WorkerDirectory>,
        Arc<dyn awaken_dispatch_execution::WorkerEvidenceRelease>,
    )>,
    worker_evidence_after: tokio::sync::Mutex<Option<String>>,
}

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
    /// Bind the exact transport/Registry owners before maintenance startup.
    /// Retention retry joins the original Dispatch reconciliation schedule.
    pub fn bind_worker_evidence_reconciliation(
        &self,
        directory: Arc<dyn awaken_dispatch_contract::WorkerDirectory>,
        source: Arc<dyn awaken_dispatch_execution::WorkerEvidenceRelease>,
    ) -> Result<(), HostError> {
        self.dispatch
            .terminal_reconciliation
            .worker_evidence
            .set((directory, source))
            .map_err(|_| HostError::internal("Worker evidence reconciliation already bound"))
    }

    pub(super) async fn reconcile_worker_evidence(
        &self,
    ) -> Result<(), awaken_dispatch_execution::Error> {
        let Some((directory, source)) = self.dispatch.terminal_reconciliation.worker_evidence.get()
        else {
            return Ok(());
        };
        let dispatch = self
            .dispatch_store()
            .map_err(|e| HostWorkerResolver::execution_error(e.to_string()))?;
        let now = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        let mut records = directory
            .list()
            .await
            .map_err(|e| HostWorkerResolver::execution_error(e.to_string()))?;
        let Ok(mut after) = self
            .dispatch
            .terminal_reconciliation
            .worker_evidence_after
            .try_lock()
        else {
            return Ok(());
        };
        records.sort_by(|a, b| {
            a.snapshot
                .identity
                .worker_id
                .cmp(&b.snapshot.identity.worker_id)
        });
        let start = after.as_ref().map_or(0, |cursor| {
            records.partition_point(|r| r.snapshot.identity.worker_id <= *cursor)
        });
        let start = if start == records.len() { 0 } else { start };
        // One candidate per original maintenance tick. Advance scheduling before
        // provider I/O so cancellation/failure cannot starve a later Worker.
        for record in records.into_iter().skip(start).take(1) {
            let identity = &record.snapshot.identity;
            *after = Some(identity.worker_id.clone());
            if record.snapshot.expires_at_ms <= now {
                directory
                    .expire_incarnation(identity, now)
                    .await
                    .map_err(|e| HostWorkerResolver::execution_error(e.to_string()))?;
            }
            // Registry expiry applies to every exact incarnation. Provider
            // ownership controls evidence operations, never lease closure.
            if !source.owns_worker_evidence(identity) {
                continue;
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                source.release_closed_idle_worker_evidence(
                    dispatch.as_ref(),
                    directory.as_ref(),
                    identity,
                ),
            )
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "exact Worker retention retry pending"),
                Err(_) => tracing::warn!("exact Worker retention retry timed out"),
            }
        }
        Ok(())
    }

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
            || !self.authority.observes_terminal_memory_locally()
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
                let frozen_model_ref = installed
                    .as_ref()
                    .map_or(self.agent.model_ref.as_str(), |root| {
                        root.resolved_spec.model_binding.identity().1
                    });
                let effective_model_ref = self
                    .agent
                    .inference_routing
                    .model_ref(thread, frozen_model_ref);
                let frozen = installed
                    .as_ref()
                    .map(|root| {
                        crate::agent_catalog::freeze_run_publications(
                            root,
                            self.agent.agent_publications.as_deref(),
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
            || !self.authority.scans_committed_terminals()
            || self
                .dispatch
                .terminal_reconciliation
                .dispatch_reconciler
                .get()
                .is_some()
        {
            return false;
        }
        let Ok(store) = self.dispatch_store() else {
            return false;
        };
        let resolver: Arc<dyn awaken_dispatch_execution::WorkerResolver<AnyDispatchStore>> =
            Arc::new(HostWorkerResolver {
                host: Arc::downgrade(self),
            });
        let wake = self
            .authority
            .dispatch_wake()
            .unwrap_or_else(|| Arc::new(awaken_dispatch_execution::LocalWakeSignal::new()));
        let reconciler = awaken_dispatch_execution::DispatchReconciler::spawn(
            store,
            self.clock.clone(),
            wake,
            self.deployment.dispatch_service_config(),
            resolver,
            Some(self.completion.clone()),
        );
        self.dispatch
            .terminal_reconciliation
            .dispatch_reconciler
            .set(reconciler)
            .is_ok()
    }
}

impl HostWorkerResolver {
    pub(super) async fn cancellation_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<
        Arc<awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>>,
        awaken_dispatch_execution::Error,
    > {
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.authority
                .open_commit(&thread_id.0)
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?,
        );
        Ok(Arc::new(
            self.boundary_worker(host, claimed, commit, None, true)
                .await?,
        ))
    }

    pub(crate) async fn boundary_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_dispatch_contract::Claimed,
        commit: Arc<crate::store::HostCommit>,
        context: Option<awaken_runtime_contract::RuntimeRunContext>,
        install_cancellation_executor: bool,
    ) -> Result<
        awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
        awaken_dispatch_execution::Error,
    > {
        let recovery_projection = commit.recovery_projection();
        let store = host
            .dispatch_store()
            .map_err(|error| Self::execution_error(error.to_string()))?;
        let mut worker = awaken_dispatch_execution::DispatchWorker::new(
            Arc::new(awaken_runtime::Runtime::new()),
            store,
            commit.clone(),
            claimed.lease.owner.clone(),
        );
        if let Some(observer) = host.dispatch_settlement_observer() {
            worker = worker.with_settlement_observer(observer);
        }
        if let Some(observer) = host.session_run_admission_observer() {
            worker = worker.with_session_run_admission_observer(observer);
        }
        let mut context = context;
        if host.authority.observes_terminal_memory_locally()
            && let Some(observer) = host
                .dispatched_memory_terminal_observer(&claimed.request, commit.clone())
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?
        {
            // Child execution supplies its attempt context here. Adding the
            // exact logical Run observer before the single `with_context`
            // prevents a later child install from erasing terminal Memory work.
            context = Some(context.unwrap_or_default().with_terminal_observer(observer));
        }
        if let Some(context) = context {
            worker = worker.with_context(context);
        }
        if install_cancellation_executor
            && matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    claimed
                        .request
                        .activation
                        .snapshot
                        .resolved_spec
                        .model_binding
                        .identity()
                        .2
                ),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            )
        {
            let executor = host.agent.remote_attempt_executor.clone().ok_or_else(|| {
                Self::execution_error(
                    "remote cancellation requires a configured remote attempt executor",
                )
            })?;
            worker.install_attempt_executor(executor);
        }
        if let Some(claimed_commit) = host
            .authority
            .claimed_commit()
            .map_err(|error| Self::execution_error(error.to_string()))?
        {
            worker = worker.with_claimed_commit(claimed_commit);
        }
        if let Some(projection) = recovery_projection {
            worker = worker.with_recovery_projection(projection);
        }
        Ok(worker)
    }
}

pub(super) async fn reconcile_committed_terminals(
    resolver: &HostWorkerResolver,
    limit: usize,
) -> Result<Vec<(RunId, RunState)>, awaken_dispatch_execution::Error> {
    let host = resolver.host()?;
    // A database-less Worker delegates committed truth upstream and has no
    // process-local registry from which to discover every Thread reader.
    if !host.authority.scans_committed_terminals() || limit == 0 {
        return Ok(Vec::new());
    }

    let store = host
        .dispatch_store()
        .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
    host.dispatch
        .terminal_reconciliation
        .terminal_scan
        .reconcile(store.as_ref(), limit, |row| {
            let host = &host;
            let store = &store;
            async move {
                // The dispatch summary already carries the canonical Session affinity.
                // Read committed truth from that physical partition before taking the
                // maintenance claim: claiming every Awaiting/expired row and restoring
                // non-terminal ones to Awaiting would steal a recoverable lease from a
                // registered replacement Worker and advance its fencing epoch forever.
                // `settle_claimed_terminal` repeats this check after the claim, closing
                // the read/claim race without introducing another recovery registry.
                let commit_thread = row.session_thread_id.as_ref().unwrap_or(&row.thread_id);
                let commit = Arc::new(
                    host.authority
                        .open_commit(&commit_thread.0)
                        .await
                        .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?,
                );
                if !matches!(commit.run_state(&row.run_id), Some(RunState::Ended(_))) {
                    return Ok(None);
                }

                let Some(claimed) = store
                    .claim_for_terminal_recovery(
                        &row.run_id,
                        &host.deployment.dispatch_owner,
                        DEFAULT_LEASE_MS,
                    )
                    .await?
                else {
                    return Ok(None);
                };

                let worker = match resolver
                    .boundary_worker(host, &claimed, commit, None, false)
                    .await
                {
                    Ok(worker) => worker,
                    Err(error) => {
                        let _ = store
                            .settle(
                                &claimed.lease.run_id,
                                claimed.lease.epoch,
                                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                                &[],
                            )
                            .await;
                        return Err(error);
                    }
                };
                worker.settle_claimed_terminal(claimed).await
            }
        })
        .await
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

    struct IdleEvidence {
        released: std::sync::Mutex<Vec<awaken_dispatch_contract::WorkerIdentity>>,
        fail_first: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionStopSource for IdleEvidence {
        fn owns_worker_evidence(&self, _: &awaken_dispatch_contract::WorkerIdentity) -> bool {
            true
        }
        async fn release_closed_idle_worker_evidence(
            &self,
            identity: &awaken_dispatch_contract::WorkerIdentity,
        ) -> Result<(), awaken_run_ingress_application_contract::ApplicationError> {
            if self.fail_first.swap(false, Ordering::SeqCst) {
                return Err(
                    awaken_run_ingress_application_contract::ApplicationError::unavailable(
                        "provider unavailable",
                    ),
                );
            }
            self.released.lock().unwrap().push(identity.clone());
            Ok(())
        }
        async fn stop_closed_worker(
            &self,
            _: &awaken_dispatch::ClosedWorkerExecution,
        ) -> Result<
            awaken_dispatch::WorkerProcessStopStatus,
            awaken_run_ingress_application_contract::ApplicationError,
        > {
            panic!("idle maintenance cannot issue a physical stop")
        }
    }

    #[tokio::test]
    async fn idle_retention_retries_expired_originals_without_starving_later_workers() {
        // M0 one injected Ingress release adapter owns the provider action;
        // M1 expired exact registrations + empty original Dispatch -> close and
        // release metadata, no physical-stop receipt. M2 first provider failure
        // -> retain and advance scheduling; M3 next tick handles later Worker;
        // M4 cursor wraps -> original retry succeeds. The cursor is scheduling
        // only; Registry closure and physical-slot authority are re-read.
        use awaken_dispatch_contract::WorkerDirectory as _;
        let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let mut identities = Vec::new();
        for id in ["retention-a", "retention-b"] {
            identities.push(
                directory
                    .register(
                        awaken_dispatch_contract::WorkerRegistration {
                            worker_id: id.into(),
                            incarnation_id: "boot".into(),
                            manifest: Default::default(),
                        },
                        0,
                        1,
                    )
                    .await
                    .unwrap()
                    .snapshot
                    .identity,
            );
        }
        let host =
            (crate::HostDependencies::ephemeral(Arc::new(crate::host::MemoryHostModel), "fixture")
                .await
                .with_ephemeral_dispatch(Arc::new(
                    awaken_dispatch::open_sqlite_dispatch_store_in_memory(Arc::new(
                        awaken_lease_testkit::ManualClock::new(0),
                    ))
                    .await
                    .unwrap(),
                )))
            .build()
            .expect("validated Host dependencies");
        let source = Arc::new(IdleEvidence {
            released: Default::default(),
            fail_first: std::sync::atomic::AtomicBool::new(true),
        });
        host.bind_worker_evidence_reconciliation(
            directory.clone(),
            Arc::new(awaken_dispatch::WorkerEvidenceReleaseAdapter::new(
                source.clone(),
            )),
        )
        .unwrap();
        for _ in 0..3 {
            host.reconcile_worker_evidence().await.unwrap();
        }
        assert_eq!(
            *source.released.lock().unwrap(),
            [identities[1].clone(), identities[0].clone()]
        );
        for identity in identities {
            let record = directory.incarnation(&identity).await.unwrap().unwrap();
            assert_eq!(
                record.snapshot.state,
                awaken_dispatch_contract::WorkerState::Dead
            );
            assert!(
                record.process_exit.is_none(),
                "M1 metadata release does not mint exit"
            );
        }
    }

    struct TerminalProjectionView(Option<RunRecord>);

    struct UnownedEvidence;

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionStopSource for UnownedEvidence {
        async fn release_closed_idle_worker_evidence(
            &self,
            _: &awaken_dispatch_contract::WorkerIdentity,
        ) -> Result<(), awaken_run_ingress_application_contract::ApplicationError> {
            panic!("an unowned domain cannot release provider evidence")
        }
        async fn stop_closed_worker(
            &self,
            _: &awaken_dispatch::ClosedWorkerExecution,
        ) -> Result<
            awaken_dispatch::WorkerProcessStopStatus,
            awaken_run_ingress_application_contract::ApplicationError,
        > {
            panic!("expiry is not physical termination")
        }
    }

    #[tokio::test]
    async fn registry_expiry_does_not_depend_on_provider_ownership_or_retire_unknown_attempts() {
        // Cause/effect graph: U1 the selected provider does not own a legacy
        // identity; U2 its exact Registry lease has expired; U3 another legacy
        // identity is live; U4 an unbound physical attempt remains occupied.
        // Effects: E1 U1+U2 closes only Registry admission; E2 U1+U3 preserves
        // the live identity; E3 no provider call/exit receipt is invented; E4
        // U4 keeps the exact original slot. One injected Ingress release
        // adapter owns the optional provider action; the Host only scans and
        // expires the Registry. Rules UE1=U1+U2=>E1+E3+E4;
        // UE2=U1+U3=>E2+E3. This uses normal registration/claim/admission APIs,
        // not a database backfill or synthetic termination assertion.
        use awaken_dispatch::{DispatchQueue as _, WorkerDirectory as _};
        let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let mut identities = Vec::new();
        for (id, ttl) in [("legacy-a", 1), ("legacy-b", u64::MAX)] {
            identities.push(
                directory
                    .register(
                        awaken_dispatch_contract::WorkerRegistration {
                            worker_id: id.into(),
                            incarnation_id: "boot".into(),
                            manifest: Default::default(),
                        },
                        0,
                        ttl,
                    )
                    .await
                    .unwrap()
                    .snapshot
                    .identity,
            );
        }
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let claimed = claim(
            &dispatch,
            "legacy-thread",
            "legacy-run",
            &identities[0].lease_owner(),
            0,
            &clock,
        )
        .await;
        assert_eq!(
            dispatch
                .begin_attempt(
                    &awaken_dispatch_contract::RunClaim::from(&claimed.lease),
                    None
                )
                .await
                .unwrap(),
            awaken_dispatch_contract::AttemptAdmission::Applied
        );
        let before = dispatch
            .load_physical_attempt(&claimed.lease.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(before.binding.is_none(), "U4 historical unbound slot");
        let host =
            (crate::HostDependencies::ephemeral(Arc::new(crate::host::MemoryHostModel), "fixture")
                .await
                .with_ephemeral_dispatch(dispatch.clone()))
            .build()
            .expect("validated Host dependencies");
        host.bind_worker_evidence_reconciliation(
            directory.clone(),
            Arc::new(awaken_dispatch::WorkerEvidenceReleaseAdapter::new(
                Arc::new(UnownedEvidence),
            )),
        )
        .unwrap();
        for _ in 0..2 {
            host.reconcile_worker_evidence().await.unwrap();
        }
        let expired = directory
            .incarnation(&identities[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            expired.snapshot.state,
            awaken_dispatch_contract::WorkerState::Dead,
            "UE1/E1"
        );
        assert!(expired.process_exit.is_none(), "UE1/E3");
        let live = directory
            .incarnation(&identities[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            live.snapshot.state,
            awaken_dispatch_contract::WorkerState::Starting,
            "UE2/E2"
        );
        assert!(live.process_exit.is_none(), "UE2/E3");
        let after = dispatch
            .load_physical_attempt(&claimed.lease.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after, before,
            "UE1/E4 exact unknown physical attempt retained"
        );
    }

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
    impl awaken_container_contract::ContainerEnvironmentProvider
        for CountingRejectedEnvironmentProvider
    {
        fn adoption_layout(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<
            awaken_container_contract::ContainerAdoptionLayout,
            awaken_provisioning_contract::SandboxError,
        > {
            awaken_sandbox_container::container_adoption_layout(handle)
        }

        fn physical_incarnation(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<String, awaken_provisioning_contract::SandboxError> {
            awaken_sandbox_container::container_physical_incarnation(handle)
        }

        async fn create_environment_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
            _intent: awaken_container_contract::EnvironmentRealizationIntent,
        ) -> Result<
            std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced Environment creation",
            ))
        }
        async fn observe_environment(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
            _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
        ) -> Result<
            awaken_provisioning_contract::SandboxObservation,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not observe Environments",
            ))
        }
        async fn adopt_environment_for_effect(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
            _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced Environment adoption",
            ))
        }
        async fn prepare_terminal_environment_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
            _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
            _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            Option<std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced terminal preparation",
            ))
        }

        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Ok(())
        }

        async fn create_unattached_environment(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
        ) -> Result<
            Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(awaken_provisioning_contract::SandboxError::backend(
                "create_environment",
                "scripted Environment entry",
            ))
        }

        async fn adopt_unattached_environment(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        ) -> Result<
            Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::backend(
                "adopt_environment",
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
        let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_session_container_provider(
                Arc::new(CountingRejectedEnvironmentProvider(creates.clone())),
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            )
            .with_memory_extraction_repository(Arc::new(FailingTerminalExtractionRepository)))
        .build()
        .expect("validated Host dependencies");
        let _managed = crate::host::tests::install_test_dispatch_runtime(&host);
        crate::host::tests::bind_test_memory(
            &host,
            "direct-terminal-observer-failure",
            "observer-failure-store",
            true,
        );
        let commit = host
            .authority
            .open_commit("direct-terminal-observer-failure")
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
        use awaken_dispatch_contract::DispatchQueue;

        let mut local_pool =
            crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub").await;
        local_pool.deployment.durable = true;
        local_pool.deployment.disable_local_pool = false;
        let local_pool = (local_pool).build().expect("validated Host dependencies");
        assert!(!local_pool.ensure_terminal_dispatch_reconciliation(), "R0");

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(now));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.deployment.durable = true;
        coordinator.deployment.disable_local_pool = true;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;

        let control = claim(
            &store,
            "thread-control",
            "run-control",
            "setup",
            now,
            &clock,
        )
        .await;
        store
            .settle(
                &control.lease.run_id,
                control.lease.epoch,
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("quiesce control");
        let terminal = claim(
            &store,
            "thread-terminal",
            "run-terminal",
            "setup",
            now,
            &clock,
        )
        .await;
        store
            .settle(
                &terminal.lease.run_id,
                terminal.lease.epoch,
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("quiesce terminal candidate");

        let commit = host
            .authority
            .open_commit("thread-terminal")
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
        use awaken_dispatch::{DispatchQueue, SettleOutcome};

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.deployment.durable = true;
        coordinator.deployment.disable_local_pool = true;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let parked = claim(
            &store,
            "thread-awaiting",
            "run-awaiting",
            "worker-a",
            now,
            &clock,
        )
        .await;
        assert_eq!(
            store
                .settle(
                    &parked.lease.run_id,
                    parked.lease.epoch,
                    awaken_dispatch_contract::DispatchOutcome::Awaiting,
                    &[],
                )
                .await
                .expect("park Awaiting row"),
            SettleOutcome::Applied
        );
        let leased = claim(
            &store,
            "thread-leased",
            "run-leased",
            "worker-a",
            now,
            &clock,
        )
        .await;
        let recovery_now = leased.lease.expires_ms.saturating_add(1);

        assert!(
            reconcile_committed_terminals(&resolver, 8,)
                .await
                .expect("scan non-terminal rows")
                .is_empty(),
            "A1/A2: non-terminal rows are not maintenance work"
        );

        clock.set(recovery_now);
        let awaiting_probe = store
            .claim_for_terminal_recovery(&parked.lease.run_id, "epoch-probe", DEFAULT_LEASE_MS)
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
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("restore Awaiting probe");

        clock.set(recovery_now);
        let replacement = store
            .claim_run(
                &leased.lease.run_id,
                "worker-b",
                DEFAULT_LEASE_MS,
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
                    awaken_dispatch_contract::DispatchOutcome::Done,
                    &[],
                )
                .await
                .expect("stale Worker A settle"),
            SettleOutcome::Fenced,
            "A4/E3"
        );
        assert!(
            store
                .lock_commit_epoch(&awaken_dispatch_contract::RunClaim::from(
                    &replacement.lease
                ))
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
        use awaken_dispatch::{DispatchQueue, WorkerResolver};

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.deployment.durable = true;
        coordinator.deployment.disable_local_pool = true;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;
        let fresh = claim(
            &store,
            "thread-exhausted",
            "run-exhausted",
            "crashed",
            now,
            &clock,
        )
        .await;
        clock.set(fresh.lease.expires_ms + 1);
        store
            .claim_run(&fresh.lease.run_id, "crashed", 1_000, &Default::default())
            .await
            .expect("recovery claim")
            .expect("expired row recovers");
        clock.set(now + 2_002);
        let claimed = store
            .claim_retry_exhausted("remote-worker", 1_000, 1)
            .await
            .expect("special claim")
            .expect("exhausted row claims");
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        assert_eq!(
            resolver
                .terminalize_retry_exhausted(&claimed,)
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
        use awaken_dispatch::{DispatchQueue, SessionChildAdmission};

        let storage = tempfile::tempdir().expect("storage");
        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let mut coordinator = crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone());
        coordinator.deployment.durable = true;
        coordinator.deployment.disable_local_pool = true;
        let coordinator = (coordinator).build().expect("validated Host dependencies");
        let host = coordinator;
        let parent = ThreadId("parent-session".into());
        let child = ThreadId("logical-child".into());
        let run = RunId("child-terminal".into());
        store
            .enqueue_session_child(
                awaken_dispatch_contract::RunDispatch::new(test_activation(&child.0, &run.0))
                    .for_session(parent.clone()),
                SessionChildAdmission::new(25, Vec::new()),
            )
            .await
            .expect("admit child");
        clock.set(now);
        let claimed = store
            .claim_run(&run, "setup", DEFAULT_LEASE_MS, &Default::default())
            .await
            .expect("claim child")
            .expect("child claim");
        store
            .settle(
                &run,
                claimed.lease.epoch,
                awaken_dispatch_contract::DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("park child");
        let parent_commit = host
            .authority
            .open_commit(&parent.0)
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
        let reconciled = reconcile_committed_terminals(&resolver, 8)
            .await
            .expect("reconcile child");
        assert_eq!(reconciled.len(), 1, "P1/E2");
        assert_eq!(reconciled[0].0, run, "P1/E2");
        assert!(store.list_dispatches().await.unwrap().is_empty(), "P1/E2");
        assert!(host.session_environment(&child.0).await.is_none(), "P1/E3");
    }
}
