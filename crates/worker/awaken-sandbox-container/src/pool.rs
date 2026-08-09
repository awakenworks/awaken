//! Warm container pool (ADR-0056 §warm-pool): pre-provisioned reusable capacity in
//! front of the container tier, addressing BOTH cold-start latency and resource
//! reuse. A pool keeps up to `size` **empty live environments** ready per container
//! shape; a matching Session binds one and execs its own Native/ACP processes.
//!
//! Safety: only **mount-less** specs are pooled ([`pc::SandboxCapacityShapeId::from_spec`]
//! returns `None`
//! otherwise) — a per-session mount bakes session-specific bytes into the container
//! at create, so a pre-warmed container could not serve a different session. Each
//! physical warm container is fresh (never ran a prior session) and serves exactly
//! one session, so there is no cross-session/tenant contamination — the *pool* is
//! reused, not a used container. Reusing a container after a session would require a
//! workspace reset and is deliberately out of scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::{
    AgentContainerProvider, AgentContainerSession, ContainerEnvironment,
    ContainerEnvironmentCapacity, ContainerEnvironmentProvider, ContainerProvider,
    ContainerRuntime, ContainerSandbox, EnvironmentOwnedProcess, RuntimeAgentProcess, command_of,
};

/// A process-global sequence for warm container scopes, so names are unique across
/// every pool in this process (a per-pool counter would collide when two pools warm
/// the same shape). Paired with the pid, warm names are also unique across processes,
/// so a fresh run never collides with a prior run's leaked warm container.
static WARM_SEQ: AtomicU64 = AtomicU64::new(0);

/// A globally-unique scope (container name) for a warm container.
fn next_warm_scope() -> String {
    format!(
        "warmpool-{}-{}",
        std::process::id(),
        WARM_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// One shape's warm capacity: the template spec to replenish from and the ready set.
struct WarmEntry<R: ContainerRuntime> {
    template: pc::SandboxSpec,
    ready: Vec<ContainerSandbox<R>>,
    last_desired: Instant,
}

/// The one admission gate for explicit startup warmup and adaptive replenishment.
/// A candidate is never published into capacity until the runtime reports Ready;
/// every rejected candidate is disposed here.
async fn ready_candidate<R: ContainerRuntime + 'static>(
    sandbox: ContainerSandbox<R>,
) -> Result<ContainerSandbox<R>, pc::SandboxError> {
    match pc::Sandbox::status(&sandbox).await {
        Ok(pc::SandboxStatus::Ready) => Ok(sandbox),
        Ok(_) => {
            let _ = pc::Sandbox::dispose(&sandbox).await;
            Err(pc::SandboxError::new(
                "prewarmed container did not become ready",
            ))
        }
        Err(error) => {
            let _ = pc::Sandbox::dispose(&sandbox).await;
            Err(error)
        }
    }
}

struct InFlightCreate {
    count: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
}

impl Drop for InFlightCreate {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

/// A warm pool wrapping a concrete [`ContainerProvider`]. Implements
/// [`AgentContainerProvider`], so it drops into the host's container channel source
/// exactly where a bare provider would (`build_docker_source`/`build_k8s_source`).
pub struct WarmContainerPool<R: ContainerRuntime> {
    inner: Arc<ContainerProvider<R>>,
    size: usize,
    total_size: usize,
    idle_ttl: Duration,
    warm: Arc<Mutex<HashMap<pc::SandboxCapacityShapeId, WarmEntry<R>>>>,
    /// Serialize explicit startup warmup per exact shape. Adaptive replenishment
    /// still remains off-path; its cap check disposes any concurrent excess.
    prewarm_gates: Mutex<HashMap<pc::SandboxCapacityShapeId, Arc<tokio::sync::Mutex<()>>>>,
    /// Set by [`shutdown`](Self::shutdown) so in-flight replenishment stops adding
    /// containers after the drain — otherwise a replenish that lands mid-drain would
    /// leak a warm container past teardown.
    closed: Arc<AtomicBool>,
    in_flight_creates: Arc<AtomicUsize>,
    create_changed: Arc<tokio::sync::Notify>,
}

impl<R: ContainerRuntime + 'static> WarmContainerPool<R> {
    /// Wrap `inner`, keeping up to `size` warm containers ready per shape. `size == 0`
    /// is a pass-through (every `open_agent` creates fresh) — the safe default a
    /// deployment opts out of the warm cost with.
    #[must_use]
    pub fn new(inner: Arc<ContainerProvider<R>>, size: usize) -> Self {
        Self::with_limits(inner, size, usize::MAX, Duration::MAX)
    }

    /// Bounded multi-shape pool. `size` caps one exact shape; `total_size` caps
    /// all ready containers. `idle_ttl` opportunistically reaps shapes no longer
    /// requested, while explicit desired-state reconciliation removes them
    /// immediately.
    #[must_use]
    pub fn with_limits(
        inner: Arc<ContainerProvider<R>>,
        size: usize,
        total_size: usize,
        idle_ttl: Duration,
    ) -> Self {
        Self {
            inner,
            size,
            total_size,
            idle_ttl,
            warm: Arc::new(Mutex::new(HashMap::new())),
            prewarm_gates: Mutex::new(HashMap::new()),
            closed: Arc::new(AtomicBool::new(false)),
            in_flight_creates: Arc::new(AtomicUsize::new(0)),
            create_changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// A unique per-warm container scope, so pre-warmed names never collide with a
    /// session's thread-scoped name or each other.
    fn warm_scope(&self) -> String {
        next_warm_scope()
    }

    fn begin_create(&self) -> Result<InFlightCreate, pc::SandboxError> {
        self.in_flight_creates.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.in_flight_creates.fetch_sub(1, Ordering::SeqCst);
            self.create_changed.notify_waiters();
            return Err(pc::SandboxError::new(
                "warm container capacity is shut down",
            ));
        }
        Ok(InFlightCreate {
            count: self.in_flight_creates.clone(),
            changed: self.create_changed.clone(),
        })
    }

    /// Ensure `target` warm containers exist for `spec`'s exact shape. This is a
    /// target, not an increment: repeated and concurrent startup calls are
    /// idempotent. Every candidate must report `Ready` before it enters the pool.
    pub async fn prewarm(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError> {
        let target = target.min(self.size).min(self.total_size);
        if target == 0 {
            return Ok(self.ready_len(spec));
        }
        let Some(key) = pc::SandboxCapacityShapeId::from_spec(spec) else {
            return Ok(0);
        };
        let gate = self
            .prewarm_gates
            .lock()
            .expect("warm prewarm gate mutex")
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = gate.lock().await;
        loop {
            let (ready, expired) = {
                let now = Instant::now();
                let mut map = self.warm.lock().expect("warm pool mutex");
                let expired_keys = map
                    .iter()
                    .filter(|(shape, entry)| {
                        *shape != &key && now.duration_since(entry.last_desired) >= self.idle_ttl
                    })
                    .map(|(shape, _)| shape.clone())
                    .collect::<Vec<_>>();
                let expired = expired_keys
                    .into_iter()
                    .filter_map(|shape| map.remove(&shape))
                    .flat_map(|entry| entry.ready)
                    .collect::<Vec<_>>();
                let ready = map.get_mut(&key).map_or(0, |entry| {
                    entry.last_desired = now;
                    entry.ready.len()
                });
                (ready, expired)
            };
            for sandbox in expired {
                let _ = pc::Sandbox::dispose(&sandbox).await;
            }
            if ready >= target {
                return Ok(ready);
            }
            let create = self.begin_create()?;
            let mut warm_spec = spec.clone();
            warm_spec.scope = self.warm_scope();
            let sandbox = ready_candidate(self.inner.create_container(&warm_spec).await?).await?;
            let (excess, evicted) = {
                let mut map = self.warm.lock().expect("warm pool mutex");
                let mut evicted = Vec::new();
                let mut total = map.values().map(|entry| entry.ready.len()).sum::<usize>();
                while total >= self.total_size {
                    let oldest = map
                        .iter()
                        .filter(|(shape, entry)| *shape != &key && !entry.ready.is_empty())
                        .min_by_key(|(_, entry)| entry.last_desired)
                        .map(|(shape, _)| shape.clone());
                    let Some(oldest) = oldest else {
                        break;
                    };
                    if let Some(candidate) =
                        map.get_mut(&oldest).and_then(|entry| entry.ready.pop())
                    {
                        evicted.push(candidate);
                        total -= 1;
                    }
                }
                let entry = map.entry(key.clone()).or_insert_with(|| WarmEntry {
                    template: spec.clone(),
                    ready: Vec::new(),
                    last_desired: Instant::now(),
                });
                entry.last_desired = Instant::now();
                if self.closed.load(Ordering::SeqCst)
                    || entry.ready.len() >= target
                    || total >= self.total_size
                {
                    (Some(sandbox), evicted)
                } else {
                    entry.ready.push(sandbox);
                    (None, evicted)
                }
            };
            for sandbox in evicted {
                let _ = pc::Sandbox::dispose(&sandbox).await;
            }
            if let Some(excess) = excess {
                let _ = pc::Sandbox::dispose(&excess).await;
                if self.closed.load(Ordering::SeqCst) {
                    return Err(pc::SandboxError::new(
                        "warm container capacity is shut down",
                    ));
                }
            }
            drop(create);
        }
    }

    /// How many warm containers are ready for `spec`'s shape (0 when not poolable).
    #[must_use]
    pub fn ready_len(&self, spec: &pc::SandboxSpec) -> usize {
        pc::SandboxCapacityShapeId::from_spec(spec)
            .and_then(|k| {
                self.warm
                    .lock()
                    .expect("warm pool mutex")
                    .get(&k)
                    .map(|e| e.ready.len())
            })
            .unwrap_or(0)
    }

    /// Remove one exact unused shape without closing the pool. A subsequent
    /// current demand can recreate it through the same prewarm path.
    pub async fn discard(&self, spec: &pc::SandboxSpec) {
        let Some(key) = pc::SandboxCapacityShapeId::from_spec(spec) else {
            return;
        };
        let ready = self
            .warm
            .lock()
            .expect("warm pool mutex")
            .remove(&key)
            .map(|entry| entry.ready)
            .unwrap_or_default();
        self.prewarm_gates
            .lock()
            .expect("warm prewarm gate mutex")
            .remove(&key);
        for sandbox in ready {
            let _ = pc::Sandbox::dispose(&sandbox).await;
        }
    }

    /// Dispose every warm container (a graceful drain). Stops replenishment first, then
    /// drains repeatedly so a create that was already in flight when the drain started
    /// is disposed too rather than leaked. Best-effort per container.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        loop {
            let batch: Vec<ContainerSandbox<R>> = {
                let mut map = self.warm.lock().expect("warm pool mutex");
                map.drain().flat_map(|(_, e)| e.ready).collect()
            };
            for sandbox in batch {
                let _ = pc::Sandbox::dispose(&sandbox).await;
            }
            let changed = self.create_changed.notified();
            if self.in_flight_creates.load(Ordering::SeqCst) == 0 {
                // No create can begin after `closed`; drain once more to cover the
                // insertion that happened immediately before the count reached zero.
                if self
                    .warm
                    .lock()
                    .expect("warm pool mutex")
                    .values()
                    .all(|entry| entry.ready.is_empty())
                {
                    break;
                }
                continue;
            }
            changed.await;
        }
    }

    /// Replenish `key` back to `size` off the request path (bounded). Concurrent
    /// replenishers for the same key may briefly overshoot; the cap check discards
    /// (and disposes) the excess, so the steady state is exactly `size`.
    fn spawn_replenish(&self, key: pc::SandboxCapacityShapeId) {
        if self.size == 0 || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        let warm = self.warm.clone();
        let closed = self.closed.clone();
        let in_flight_creates = self.in_flight_creates.clone();
        let create_changed = self.create_changed.clone();
        let size = self.size;
        let total_size = self.total_size;
        in_flight_creates.fetch_add(1, Ordering::SeqCst);
        if closed.load(Ordering::SeqCst) {
            in_flight_creates.fetch_sub(1, Ordering::SeqCst);
            create_changed.notify_waiters();
            return;
        }
        tokio::spawn(async move {
            let _create = InFlightCreate {
                count: in_flight_creates,
                changed: create_changed,
            };
            loop {
                if closed.load(Ordering::SeqCst) {
                    return;
                }
                // How many are missing + the shape to build, read under the lock.
                let template = {
                    let map = warm.lock().expect("warm pool mutex");
                    match map.get(&key) {
                        Some(e) if e.ready.len() < size => e.template.clone(),
                        _ => return,
                    }
                };
                let mut warm_spec = template;
                warm_spec.scope = next_warm_scope();
                match inner.create_container(&warm_spec).await {
                    Ok(sandbox) => match ready_candidate(sandbox).await {
                        Ok(sandbox) => {
                            // Re-check the cap under the lock; dispose an over-cap create.
                            let over = {
                                let mut map = warm.lock().expect("warm pool mutex");
                                let total =
                                    map.values().map(|entry| entry.ready.len()).sum::<usize>();
                                match map.get_mut(&key) {
                                    Some(e) if e.ready.len() < size && total < total_size => {
                                        e.ready.push(sandbox);
                                        None
                                    }
                                    _ => Some(sandbox),
                                }
                            };
                            if let Some(excess) = over {
                                let _ = pc::Sandbox::dispose(&excess).await;
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    // A create failure stops replenishing this key (fail closed, no spin).
                    Err(_) => return,
                }
            }
        });
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> ContainerEnvironmentCapacity for WarmContainerPool<R> {
    async fn prewarm_to(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError> {
        self.prewarm(spec, target).await
    }

    fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize {
        self.ready_len(spec)
    }

    async fn discard_shape(&self, spec: &pc::SandboxSpec) {
        self.discard(spec).await;
    }

    async fn shutdown_capacity(&self) {
        self.shutdown().await;
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> AgentContainerProvider for WarmContainerPool<R> {
    async fn open_agent(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<AgentContainerSession, pc::SandboxError> {
        let environment = self.create_environment(spec).await?;
        let argv = command_of(spec);
        if argv.is_empty() {
            return Err(pc::SandboxError::new("agent command argv is empty"));
        }
        let RuntimeAgentProcess { process, channel } = environment
            .spawn_agent_process(pc::Command {
                argv,
                cwd: String::new(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await?;
        Ok(AgentContainerSession {
            channel,
            process: Box::new(EnvironmentOwnedProcess {
                inner: process,
                environment: environment.clone(),
            }),
            handle: environment.handle(),
        })
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> ContainerEnvironmentProvider for WarmContainerPool<R> {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        self.inner.sandbox_capabilities()
    }

    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.inner.install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.inner.install_secret_broker(broker);
    }

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        let Some(key) = pc::SandboxCapacityShapeId::from_spec(spec) else {
            return self.inner.create_environment(spec).await;
        };
        let warm = {
            let mut map = self.warm.lock().expect("warm pool mutex");
            let entry = map.entry(key.clone()).or_insert_with(|| WarmEntry {
                template: spec.clone(),
                ready: Vec::new(),
                last_desired: Instant::now(),
            });
            entry.last_desired = Instant::now();
            entry.ready.pop()
        };
        let environment = match warm {
            Some(environment) => environment.bind_scope(spec.scope.clone()),
            None => self.inner.create_container(spec).await?,
        };
        self.spawn_replenish(key);
        Ok(Arc::new(environment))
    }

    async fn adopt_environment(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        self.inner.adopt_environment(handle).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingRuntime {
        creates: AtomicUsize,
        removes: AtomicUsize,
        ready: AtomicBool,
        block_create: AtomicBool,
        create_started: tokio::sync::Notify,
        allow_create: tokio::sync::Semaphore,
    }

    impl RecordingRuntime {
        fn ready() -> Self {
            Self {
                creates: AtomicUsize::new(0),
                removes: AtomicUsize::new(0),
                ready: AtomicBool::new(true),
                block_create: AtomicBool::new(false),
                create_started: tokio::sync::Notify::new(),
                allow_create: tokio::sync::Semaphore::new(0),
            }
        }
    }

    #[async_trait]
    impl ContainerRuntime for RecordingRuntime {
        fn enforces_network_none(&self) -> bool {
            true
        }

        async fn create(
            &self,
            id: &str,
            _plan: &crate::ContainerPlan,
        ) -> Result<String, crate::RuntimeError> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            if self.block_create.load(Ordering::SeqCst) {
                self.create_started.notify_waiters();
                self.allow_create
                    .acquire()
                    .await
                    .expect("test create semaphore remains open")
                    .forget();
            }
            Ok(format!("container-{id}"))
        }

        async fn open_channel(
            &self,
            _container_id: &str,
        ) -> Result<Box<dyn crate::AgentChannel>, crate::RuntimeError> {
            Err(crate::RuntimeError::Backend(
                "not used by warmup test".into(),
            ))
        }

        async fn inspect(
            &self,
            _container_id: &str,
        ) -> Result<crate::ContainerState, crate::RuntimeError> {
            Ok(if self.ready.load(Ordering::SeqCst) {
                crate::ContainerState::Running
            } else {
                crate::ContainerState::Gone
            })
        }

        async fn wait(&self, _container_id: &str) -> Result<pc::ExitStatus, crate::RuntimeError> {
            Ok(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }

        async fn poll(
            &self,
            _container_id: &str,
        ) -> Result<Option<pc::ExitStatus>, crate::RuntimeError> {
            Ok(None)
        }

        async fn signal(
            &self,
            _container_id: &str,
            _signal: pc::Signal,
        ) -> Result<(), crate::RuntimeError> {
            Ok(())
        }

        async fn artifacts(
            &self,
            _container_id: &str,
        ) -> Result<Vec<pc::Artifact>, crate::RuntimeError> {
            Ok(Vec::new())
        }

        async fn read_artifact(
            &self,
            _container_id: &str,
            _artifact_id: &str,
        ) -> Result<Vec<u8>, crate::RuntimeError> {
            Err(crate::RuntimeError::Backend(
                "not used by warmup test".into(),
            ))
        }

        async fn touch_lease(&self, _container_id: &str) -> Result<(), crate::RuntimeError> {
            Ok(())
        }

        async fn remove(&self, _container_id: &str) -> Result<(), crate::RuntimeError> {
            self.removes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn recording_pool(
        runtime: Arc<RecordingRuntime>,
        size: usize,
    ) -> WarmContainerPool<RecordingRuntime> {
        WarmContainerPool::new(
            Arc::new(ContainerProvider::new(runtime, "agent:test")),
            size,
        )
    }

    #[cfg(feature = "podman")]
    #[test]
    fn pool_preserves_inner_provider_capability_evidence() {
        let runtime = Arc::new(crate::podman::PodmanRuntime::new(7600));
        let inner = Arc::new(ContainerProvider::new(runtime, "agent:test"));
        let expected = inner.sandbox_capabilities();
        let pool = WarmContainerPool::new(inner, 1);
        assert_eq!(pool.sandbox_capabilities(), expected);
        assert!(pool.sandbox_capabilities().network_isolation);
        assert!(!pool.sandbox_capabilities().enforced_network_allowlist);
    }

    fn spec(scope: &str, cmd: &[&str]) -> pc::SandboxSpec {
        pc::SandboxSpec {
            scope: scope.into(),
            isolation: pc::IsolationClass::Container,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: Some(serde_json::json!({ "command": cmd })),
        }
    }

    /// Cause/effect design:
    /// C1=poolable exact shape, C2=target exceeds ready count, C3=repeated call,
    /// C4=runtime reports Ready. E1=only the deficit is created, E2=result reaches
    /// target, E3=repeated warmup is idempotent.
    /// Rules: (C1,C2,C4)->(E1,E2); (C1,!C2,C3)->E3.
    #[tokio::test]
    async fn prewarm_uses_a_ready_target_instead_of_an_increment() {
        let runtime = Arc::new(RecordingRuntime::ready());
        let pool = recording_pool(runtime.clone(), 2);
        let spec = spec("startup", &[]);

        assert_eq!(pool.prewarm(&spec, 2).await.unwrap(), 2);
        assert_eq!(pool.prewarm(&spec, 2).await.unwrap(), 2);
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 2);
        assert_eq!(pool.ready_len(&spec), 2);

        pool.shutdown().await;
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn multi_shape_capacity_obeys_global_budget_and_desired_removal() {
        // FMECA: F1 N Environment revisions each consume per-shape capacity
        // without a global bound (S7 O6 D3, RPN126); F2 evict active Session
        // environment (S10 O2 D3, RPN60); F3 removed desired shape keeps unused
        // containers (S5 O5 D2, RPN50). Mitigations: budget applies only to the
        // ready set, consumed environments leave it before eviction, and explicit
        // discard removes only never-used capacity.
        // Cause graph: C1=new shape requested; C2=global budget full;
        // C3=old shape removed from desired state. E1=oldest unused entry evicted;
        // E2=total remains bounded; E3=removed shape reaches zero.
        // | Rule | C1 | C2 | C3 | Effect |
        // | B1   | 1  | 0  | 0  | add     |
        // | B2   | 1  | 1  | 0  | E1,E2   |
        // | B3   | -  | -  | 1  | E3      |
        let runtime = Arc::new(RecordingRuntime::ready());
        let pool = WarmContainerPool::with_limits(
            Arc::new(ContainerProvider::new(runtime.clone(), "agent:test")),
            2,
            2,
            Duration::from_secs(300),
        );
        let first = spec("first", &[]);
        let mut second = spec("second", &[]);
        second.network = pc::NetworkPolicy::Unrestricted;
        assert_eq!(pool.prewarm(&first, 2).await.unwrap(), 2, "B1");
        assert_eq!(pool.prewarm(&second, 1).await.unwrap(), 1, "B2");
        assert_eq!(pool.ready_len(&first) + pool.ready_len(&second), 2, "B2");
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 1, "B2 E1");
        pool.discard(&second).await;
        assert_eq!(pool.ready_len(&second), 0, "B3");
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 2, "B3");
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn unified_plan_targets_do_not_create_then_evict_capacity() {
        // FMECA: independent default/Environment producers each see a valid
        // per-shape target but oversubscribe the real global pool, causing an
        // immediate create→evict cycle (S6,O6,D4,RPN144). Cause graph:
        // C1=Environment demand; C2=default demand; C3=global budget one;
        // C4=global budget two. Effects: E1=Environment gets the sole priority
        // slot; E2=both get one slot; E3=no just-created container is removed.
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | P1   | 1  | 1  | 1  | 0  | E1,E3  |
        // | P2   | 1  | 1  | 0  | 1  | E2,E3  |
        let environment = spec("environment", &[]);
        let mut default = spec("default", &[]);
        default.network = pc::NetworkPolicy::Unrestricted;

        let runtime_one = Arc::new(RecordingRuntime::ready());
        let pool_one = WarmContainerPool::with_limits(
            Arc::new(ContainerProvider::new(runtime_one.clone(), "agent:test")),
            1,
            1,
            Duration::from_secs(300),
        );
        // The Worker plan assigns Environment first and therefore emits no
        // default target once the single global slot is exhausted.
        assert_eq!(pool_one.prewarm(&environment, 1).await.unwrap(), 1, "P1");
        assert_eq!(pool_one.ready_len(&default), 0, "P1 E1");
        assert_eq!(runtime_one.creates.load(Ordering::SeqCst), 1, "P1");
        assert_eq!(runtime_one.removes.load(Ordering::SeqCst), 0, "P1 E3");
        pool_one.shutdown().await;

        let runtime_two = Arc::new(RecordingRuntime::ready());
        let pool_two = WarmContainerPool::with_limits(
            Arc::new(ContainerProvider::new(runtime_two.clone(), "agent:test")),
            1,
            2,
            Duration::from_secs(300),
        );
        assert_eq!(pool_two.prewarm(&environment, 1).await.unwrap(), 1, "P2");
        assert_eq!(pool_two.prewarm(&default, 1).await.unwrap(), 1, "P2");
        assert_eq!(pool_two.ready_len(&environment), 1, "P2 E2");
        assert_eq!(pool_two.ready_len(&default), 1, "P2 E2");
        assert_eq!(runtime_two.creates.load(Ordering::SeqCst), 2, "P2");
        assert_eq!(runtime_two.removes.load(Ordering::SeqCst), 0, "P2 E3");
        pool_two.shutdown().await;
    }

    /// Cause/effect design:
    /// C1=spec contains any creation-time mount. E1=shape is non-poolable,
    /// E2=no container is created, E3=reported capacity is zero. This preserves
    /// the never-used/no-cross-session-bytes invariant.
    #[tokio::test]
    async fn mounted_specs_never_enter_startup_capacity() {
        let runtime = Arc::new(RecordingRuntime::ready());
        let pool = recording_pool(runtime.clone(), 2);
        let mut mounted = spec("mounted", &[]);
        mounted.mounts.push(pc::MountRequirement {
            mount_id: "cache".into(),
            source: pc::MountSource::CacheVolume {
                location: pc::CacheVolumeLocation::HostPath {
                    path: "/tmp/cache".into(),
                },
                key: "cache-v1".into(),
            },
            mount_path: "/workspace/cache".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        });

        assert_eq!(pool.prewarm(&mounted, 2).await.unwrap(), 0);
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(pool.ready_len(&mounted), 0);
    }

    /// Cause/effect design:
    /// C1=create is in flight, C2=shutdown closes capacity before create returns.
    /// E1=shutdown waits for the create, E2=the late container is disposed,
    /// E3=no ready capacity survives, E4=future warmup is rejected.
    /// Rule: (C1,C2)->(E1,E2,E3,E4).
    #[tokio::test]
    async fn shutdown_fences_and_disposes_an_in_flight_prewarm() {
        let runtime = Arc::new(RecordingRuntime::ready());
        runtime.block_create.store(true, Ordering::SeqCst);
        let pool = Arc::new(recording_pool(runtime.clone(), 1));
        let spec = spec("shutdown-race", &[]);
        let started = runtime.create_started.notified();
        let warming = {
            let pool = pool.clone();
            let spec = spec.clone();
            tokio::spawn(async move { pool.prewarm(&spec, 1).await })
        };
        started.await;
        let shutting_down = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.shutdown().await })
        };
        runtime.allow_create.add_permits(1);

        assert!(warming.await.unwrap().is_err());
        shutting_down.await.unwrap();
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 1);
        assert_eq!(pool.ready_len(&spec), 0);
        assert!(pool.prewarm(&spec, 1).await.is_err());
    }

    /// Cause/effect design:
    /// C1=runtime creates a candidate, C2=readiness probe reports terminated.
    /// E1=warmup fails, E2=candidate is disposed, E3=it is never advertised ready.
    /// Rule: (C1,C2)->(E1,E2,E3).
    #[tokio::test]
    async fn an_unready_candidate_is_disposed_and_never_published() {
        let runtime = Arc::new(RecordingRuntime::ready());
        runtime.ready.store(false, Ordering::SeqCst);
        let pool = recording_pool(runtime.clone(), 1);
        let spec = spec("not-ready", &[]);

        assert!(pool.prewarm(&spec, 1).await.is_err());
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 1);
        assert_eq!(pool.ready_len(&spec), 0);
    }

    /// Cause/effect design:
    /// C1=a cold Session consumes no ready capacity, C2=adaptive replenish creates
    /// a candidate, C3=its readiness probe reports terminated. E1=the Session cold
    /// path still returns, E2=the candidate is disposed, E3=capacity stays empty.
    /// Rule: (C1,C2,C3)->(E1,E2,E3).
    #[tokio::test]
    async fn adaptive_replenishment_uses_the_same_readiness_gate() {
        let runtime = Arc::new(RecordingRuntime::ready());
        runtime.ready.store(false, Ordering::SeqCst);
        let pool = recording_pool(runtime.clone(), 1);
        let spec = spec("adaptive-not-ready", &[]);

        let environment = pool.create_environment(&spec).await.expect("E1 cold path");
        for _ in 0..100 {
            if runtime.creates.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 1, "E2");
        assert_eq!(pool.ready_len(&spec), 0, "E3");
        environment.dispose().await.unwrap();
    }

    #[test]
    fn a_mount_less_spec_is_poolable_and_keys_on_shape_not_scope() {
        // Two sessions (different scope) of the same mount-less agent share a key —
        // so a warm container for one can serve the other.
        let a = spec("thread-a", &["agent", "--acp"]);
        let b = spec("thread-b", &["agent", "--acp"]);
        assert_eq!(
            pc::SandboxCapacityShapeId::from_spec(&a),
            pc::SandboxCapacityShapeId::from_spec(&b)
        );
        assert!(pc::SandboxCapacityShapeId::from_spec(&a).is_some());
    }
}
