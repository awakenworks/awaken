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
    pub(crate) dispatch_session_runtime: crate::DispatchSessionRuntime,
    /// The process-level dispatch pool (O2), spawned once by `mount` when durable
    /// ingress is enabled. It is the sole claimer of the one shared queue and drives
    /// every session's runs by routing each claimed run back to the worker that owns
    /// its thread. Held here so background submit / cross-thread relay nudge it and
    /// so it lives for the process's lifetime.
    pub(crate) dispatch_pool: std::sync::OnceLock<Arc<DispatchPool<AnyDispatchStore>>>,
    /// How the one process-level dispatch pool is sized. The registered and the
    /// unregistered path are distinct variants rather than one integer with a
    /// silent initializer, so "a Worker registered a capacity nobody installed"
    /// has no representation here — see [`crate::DispatchPoolSizing`].
    pub(crate) dispatch_pool_sizing: DispatchPoolSizing,
    /// The coordinator-only maintenance loop and its terminal scan; owned by
    /// [`super::super::terminal_reconciliation`], the only module that reads them.
    pub(crate) terminal_reconciliation:
        crate::host::terminal_reconciliation::TerminalReconciliationState,
}

/// The dispatch transport and pool sizing a composition selects before the
/// Host exists. Consumer: [`crate::HostDependencies`], which turns it into the
/// Host's [`DispatchCapability`].
pub(crate) struct DispatchSelection {
    /// See [`DispatchCapability::dispatch_pool_sizing`].
    pub(crate) dispatch_pool_sizing: DispatchPoolSizing,
}

impl Default for DispatchSelection {
    /// No transport and the CPU-derived sizing; a registered Worker replaces
    /// the sizing.
    fn default() -> Self {
        Self {
            dispatch_pool_sizing: DispatchPoolSizing::local_parallelism(),
        }
    }
}

impl DispatchCapability {
    /// The selected transport and sizing, with no pool yet: `mount` spawns it.
    pub(in crate::host) fn new(
        selection: DispatchSelection,
        host: std::sync::Weak<SharedHost>,
    ) -> Self {
        Self {
            dispatch_session_runtime: crate::DispatchSessionRuntime { host },
            dispatch_pool: std::sync::OnceLock::new(),
            dispatch_pool_sizing: selection.dispatch_pool_sizing,
            terminal_reconciliation: Default::default(),
        }
    }
}

/// How the one process-level [`DispatchPool`] got its slot count.
///
/// The two paths are deliberately distinct variants instead of one integer with
/// a default baked into the constructor. A registered Worker publishes
/// `WorkerCapacity::max_concurrent` to the registry and the scheduler places
/// against it; installing a locally guessed number instead is exactly the
/// "registered value nobody consumed" divergence, and it used to be invisible
/// because both numbers had the same type. Now the CPU guess is a named branch
/// that only an unregistered composition produces, and [`Registered`] is
/// reachable only by moving a [`RegisteredCapacity`] into
/// [`crate::HostDependencies::for_worker`].
///
/// [`Registered`]: DispatchPoolSizing::Registered
/// [`RegisteredCapacity`]: awaken_dispatch_contract::RegisteredCapacity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPoolSizing {
    /// No Worker registration in this topology (a direct or coordinator-embedded
    /// Host). No registry ever published a capacity for this process, so local
    /// parallelism is the whole truth and cannot contradict anything.
    LocalParallelism(std::num::NonZeroUsize),
    /// A registered Worker installed its validated manifest capacity. This is
    /// the number the registry handed the scheduler, so it is also the exact
    /// number of execution slots the pool installs.
    Registered(std::num::NonZeroUsize),
}

impl DispatchPoolSizing {
    /// Slots to install. Both branches are resolved here so no caller can read
    /// one variant and forget the other.
    #[must_use]
    pub const fn slots(self) -> std::num::NonZeroUsize {
        match self {
            Self::LocalParallelism(value) | Self::Registered(value) => value,
        }
    }

    /// The registered capacity when this process is a registered Worker.
    #[must_use]
    pub const fn registered(self) -> Option<std::num::NonZeroUsize> {
        match self {
            Self::Registered(value) => Some(value),
            Self::LocalParallelism(_) => None,
        }
    }

    /// The CPU-derived sizing used by a Host that registered no capacity.
    fn local_parallelism() -> Self {
        Self::LocalParallelism(
            std::thread::available_parallelism()
                .unwrap_or(std::num::NonZeroUsize::new(4).expect("nonzero local fallback")),
        )
    }
}

impl SharedHost {
    pub fn dispatch_store(
        &self,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyDispatchStore>, HostError> {
        self.optional_dispatch_store().ok_or_else(|| {
            HostError::internal("database-less Worker requires an injected dispatch transport")
        })
    }

    /// Existing durable dispatch authority when this topology has one. Direct
    /// foreground Sessions can still recover their root pause from committed
    /// Thread truth without manufacturing a queue merely for discovery.
    pub(crate) fn optional_dispatch_store(
        &self,
    ) -> Option<Arc<awaken_dispatch_erasure::AnyDispatchStore>> {
        self.authority.dispatch_store()
    }

    /// Whether this process owns a co-located dispatch pool. Process startup uses
    /// the same parsed deployment value that admission uses, so the process cannot
    /// accidentally both advertise coordinator-only behavior and drain locally.
    #[must_use]
    pub fn runs_local_dispatch_pool(&self) -> bool {
        !self.deployment.disable_local_pool
    }

    /// How this process's dispatch pool is sized, and therefore whether the
    /// number came from a Worker registration or from local parallelism. Reading
    /// this observes the installed execution side; it is not a second copy of
    /// the registry.
    #[must_use]
    pub fn dispatch_pool_sizing(&self) -> crate::DispatchPoolSizing {
        self.dispatch.dispatch_pool_sizing
    }

    /// Stable logical identity used by both local dispatch claims and local
    /// Session realization. The process incarnation is deliberately separate.
    #[must_use]
    pub fn dispatch_owner(&self) -> &str {
        &self.deployment.dispatch_owner
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
    /// A coordinator-only Host remains admission-only through the existing
    /// `disable_local_pool` topology fence. Idempotent.
    pub fn ensure_dispatch_pool(self: &Arc<Self>) {
        if !self.runs_local_dispatch_pool() || self.dispatch.dispatch_pool.get().is_some() {
            return;
        }
        let Ok(store) = self.dispatch_store() else {
            // Postgres backend not yet initialised — a later `mount` after
            // Coordinator startup supplies the shared dispatch authority and
            // this Host owns only its process-local claimer.
            return;
        };
        let resolver: Arc<dyn WorkerResolver<AnyDispatchStore>> = Arc::new(HostWorkerResolver {
            host: Arc::downgrade(self),
        });
        let config = self.deployment.dispatch_service_config();
        let concurrency = self.dispatch.dispatch_pool_sizing.slots().get();
        // On the Postgres backend with a typed PgNotify or NATS wake selection,
        // spawn the pool with the cross-node wake so a peer's enqueue nudges this pool
        // without busy-poll; otherwise the in-process `LocalWakeSignal` suffices.
        let completion = self.completion.clone() as Arc<dyn CompletionSink>;
        let shared_wake = self.authority.dispatch_wake();
        let pool = match shared_wake {
            Some(wake) => DispatchPool::spawn_with_wake_and_completion(
                store,
                self.clock.clone(),
                self.deployment.dispatch_owner.clone(),
                DEFAULT_LEASE_MS,
                config,
                resolver,
                concurrency,
                wake,
                completion,
            ),
            None => DispatchPool::spawn_with_completion(
                store,
                self.clock.clone(),
                self.deployment.dispatch_owner.clone(),
                DEFAULT_LEASE_MS,
                config,
                resolver,
                concurrency,
                completion,
            ),
        };
        let _ = self.dispatch.dispatch_pool.set(Arc::new(pool));
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
}
