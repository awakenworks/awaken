// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! DISPATCH capability of [`SharedHost`]: the selected authority queue, the
//! one process-level [`DispatchPool`], the coordinator-side terminal
//! reconciliation, and the weak Session-application edges the pool's Workers
//! call back into.

use crate::host::*;

/// Dispatch views, sizing and pool owned by one [`SharedHost`]. Consumer: every
/// admission, pool, drain, and terminal-reconciliation path of the Host.
pub(crate) struct DispatchCapability {
    /// Cold-Worker adapter over the configured Session Runtime. It owns only weak
    /// host wiring plus Resource/credential SPIs, so installing it cannot create
    /// an `Arc<SharedHost>` cycle or a second Vault/materialization path.
    pub(crate) session_effects: crate::SessionEffects,
    /// Ordinary Run MCP uses SDK Direct admission over the same physical owner.
    pub(crate) run_mcp_effects: crate::session_effects::RunMcpEffects,
    /// The process-level dispatch pool (O2), spawned once by `mount` when durable
    /// ingress is enabled. It is the sole claimer of the one shared queue and drives
    /// every session's runs by routing each claimed run back to the worker that owns
    /// its thread. Held here so background submit / cross-thread relay nudge it and
    /// so it lives for the process's lifetime.
    pub(crate) dispatch_pool: std::sync::OnceLock<Arc<DispatchPool<AnyDispatchStore>>>,
    /// This process's part in Run execution, selected by its composition
    /// root: ingress, local pool and its sizing, lease owner and schedule.
    pub(crate) execution: super::ExecutionTopology,
    /// The coordinator-only maintenance loop and its terminal scan
    /// (`host::dispatch_reconciliation`).
    pub(crate) terminal_reconciliation:
        awaken_dispatch_execution::session_reconciliation::TerminalReconciliationState,
}

impl DispatchCapability {
    /// Whether foreground Runs are delivered through this capability's queue.
    pub(crate) fn durable_ingress(&self) -> bool {
        self.execution.ingress.is_durable()
    }

    /// The selected execution topology, with no pool yet: `mount` spawns it.
    pub(in crate::host) fn new(
        execution: super::ExecutionTopology,
        host: std::sync::Weak<SharedHost>,
    ) -> Self {
        Self {
            session_effects: crate::SessionEffects { host: host.clone() },
            run_mcp_effects: crate::session_effects::RunMcpEffects { host },
            dispatch_pool: std::sync::OnceLock::new(),
            execution,
            terminal_reconciliation: Default::default(),
        }
    }
}

impl SharedHost {
    pub fn dispatch_store(
        &self,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyDispatchStore>, HostError> {
        self.authority
            .dispatch_binding()
            .map(|binding| binding.queue())
            .ok_or_else(|| {
                HostError::internal("database-less Worker requires an injected dispatch binding")
            })
    }

    /// Existing durable dispatch authority when this topology has one. Direct
    /// foreground Sessions can still recover their root pause from committed
    /// Thread truth without manufacturing a queue merely for discovery.
    pub(crate) fn optional_dispatch_store(
        &self,
    ) -> Option<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>> {
        match self.authority.dispatch_binding()?.placement() {
            awaken_dispatch_erasure::DispatchTopology::Local(owner) => Some(owner.clone()),
            awaken_dispatch_erasure::DispatchTopology::Remote { .. } => None,
        }
    }

    pub fn coordinator_dispatch(
        &self,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>, HostError> {
        self.optional_dispatch_store()
            .ok_or_else(|| HostError::internal("operation requires a Coordinator dispatch owner"))
    }

    /// Whether this process owns a co-located dispatch pool. Process startup uses
    /// the same selected topology that admission uses, so the process cannot
    /// accidentally both advertise coordinator-only behavior and drain locally.
    #[must_use]
    pub fn runs_local_dispatch_pool(&self) -> bool {
        self.dispatch.execution.local.runs_local_pool()
    }

    /// Stable logical identity used by both local dispatch claims and local
    /// Session realization. The process incarnation is deliberately separate.
    #[must_use]
    pub fn dispatch_owner(&self) -> &str {
        self.dispatch.execution.lease_owner.as_str()
    }

    /// Stop local durable admission, await every already-claimed Run, then join
    /// auxiliary work created by those Runs.  The ordering is significant: a
    /// terminal Run may enqueue Memory extraction immediately before releasing
    /// its pool capacity, so draining auxiliary work first can miss that task.
    pub async fn drain_runtime(&self, timeout: std::time::Duration) -> bool {
        let deadline = awaken_time::mono::MonoDeadline::after(timeout);
        let Some(_local_fence) =
            await_runtime_drain_fence_before(deadline.instant(), self.begin_pool_drain()).await
        else {
            return false;
        };
        while self.pool_in_flight() != 0 {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return false;
            }
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(5))).await;
        }
        let remaining = deadline.remaining();
        !remaining.is_zero() && self.resources.memory.drain(remaining).await
    }

    /// Spawn the one process-level [`DispatchPool`] (O2), once, whenever this Host
    /// owns local execution. Called by `mount` — the single seam that owns an
    /// `Arc<SharedHost>` — because the pool's resolver needs a back-reference to
    /// open sessions. Durable and ephemeral Runtime authorities expose the same
    /// dispatch owner; asynchronous Session children use it in either deployment.
    /// A coordinator-only Host ([`crate::LocalExecution::RemoteWorkersOnly`])
    /// has no pool sizing and remains admission-only. Idempotent.
    pub fn ensure_dispatch_pool(self: &Arc<Self>) -> Result<(), awaken_dispatch_execution::Error> {
        let Some(sizing) = self.dispatch.execution.local.pool_sizing() else {
            return Ok(());
        };
        if self.dispatch.dispatch_pool.get().is_some() {
            return Ok(());
        }
        let Some(binding) = self.authority.dispatch_binding() else {
            // Postgres backend not yet initialised — a later `mount` after
            // Coordinator startup supplies the shared dispatch authority and
            // this Host owns only its process-local claimer.
            return Ok(());
        };
        let resolver: Arc<dyn WorkerResolver<AnyDispatchStore>> = Arc::new(SessionWorkerResolver {
            host: Arc::downgrade(self),
        });
        let config = self.dispatch.execution.dispatch_service_config();
        let concurrency = sizing.slots().get();
        // On the Postgres backend with a typed PgNotify or NATS wake selection,
        // spawn the pool with the cross-node wake so a peer's enqueue nudges this pool
        // without busy-poll; otherwise the in-process `LocalWakeSignal` suffices.
        let completion = self.completion.clone() as Arc<dyn CompletionSink>;
        let shared_wake = self
            .authority
            .dispatch_wake()
            .unwrap_or_else(|| Arc::new(awaken_dispatch_execution::LocalWakeSignal::new()));
        let pool = DispatchPool::spawn_bound(
            binding,
            self.clock.clone(),
            self.dispatch_owner().to_owned(),
            DEFAULT_LEASE_MS,
            config,
            resolver,
            concurrency,
            shared_wake,
            Some(completion),
        )?;
        let _ = self.dispatch.dispatch_pool.set(Arc::new(pool));
        Ok(())
    }

    /// Begin a graceful drain of the dispatch pool (scale-in / SIGTERM): stop
    /// claiming new runs while the in-flight ones finish. A no-op when no pool is
    /// running (a direct/non-durable host, or a worker whose pool never started).
    /// Idempotent. The admin surface calls this on `POST /admin/drain`.
    pub async fn begin_pool_drain(&self) -> Option<crate::PoolDrainFence> {
        match self.dispatch.dispatch_pool.get() {
            Some(pool) => Some(crate::PoolDrainFence {
                _ingress: pool.begin_drain().await,
            }),
            None => None,
        }
    }

    /// Pause only new claim admission while retaining all in-flight Run work.
    pub async fn suspend_pool_admission(&self) {
        if let Some(pool) = self.dispatch.dispatch_pool.get() {
            pool.suspend_admission().await;
        }
    }

    /// Resume a temporary authority suspension. Returns false when the pool was
    /// never started or has entered the absorbing graceful-drain state.
    #[must_use]
    pub async fn resume_pool_admission(&self) -> bool {
        match self.dispatch.dispatch_pool.get() {
            Some(pool) => pool.resume_admission().await,
            None => false,
        }
    }

    /// Whether the dispatch pool is up and still claiming work — the worker's
    /// readiness signal. `false` before the pool starts or once it is draining, so a
    /// readiness probe reports 503 in exactly the states where the worker should not
    /// receive (or keep being routed) new work.
    #[must_use]
    pub async fn pool_accepting_work(&self) -> bool {
        match self.dispatch.dispatch_pool.get() {
            Some(pool) => pool.is_accepting().await,
            None => false,
        }
    }

    /// Whether a successful registry heartbeat may make this Worker ready.
    #[must_use]
    pub async fn pool_can_resume_work(&self) -> bool {
        match self.dispatch.dispatch_pool.get() {
            Some(pool) => pool.can_resume().await,
            None => false,
        }
    }

    #[must_use]
    pub fn pool_in_flight(&self) -> u32 {
        self.dispatch
            .dispatch_pool
            .get()
            .map_or(0, |pool| pool.in_flight())
    }

    /// Bind the exact transport/Registry owners before maintenance startup.
    /// One neutral adapter owns two explicit maintenance purposes on the
    /// existing cadence; the Host never imports their Ingress implementation.
    pub fn bind_worker_evidence_reconciliation(
        &self,
        directory: Arc<dyn awaken_dispatch_contract::WorkerDirectory>,
        source: Arc<dyn awaken_dispatch_execution::WorkerTerminalReconciliation>,
    ) -> Result<(), HostError> {
        self.dispatch
            .terminal_reconciliation
            .bind_worker_evidence(directory, source)
    }

    /// Start the one coordinator-side reconciliation loop when this Host owns
    /// durable dispatch/commit truth but intentionally does not run an execution
    /// pool. A local-pool Host already performs the same maintenance in its pool;
    /// a database-less Worker has no global committed reader and must not start it.
    /// Returns whether this call started the loop.
    pub fn ensure_terminal_dispatch_reconciliation(self: &Arc<Self>) -> bool {
        if !self.dispatch.durable_ingress()
            || self.runs_local_dispatch_pool()
            || !self.authority.scans_committed_terminals()
            || self.dispatch.terminal_reconciliation.reconciler_started()
        {
            return false;
        }
        let Some(store) = self.optional_dispatch_store() else {
            return false;
        };
        let resolver: Arc<dyn awaken_dispatch_execution::WorkerResolver<AnyDispatchStore>> =
            Arc::new(SessionWorkerResolver {
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
            self.dispatch.execution.dispatch_service_config(),
            resolver,
            Some(self.completion.clone()),
        );
        self.dispatch
            .terminal_reconciliation
            .install_reconciler(reconciler)
    }
}
