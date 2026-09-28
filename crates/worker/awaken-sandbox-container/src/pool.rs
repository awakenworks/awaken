// @formal surface obligation=sandbox_continuation.distinct_mutable_roots_share_one_retained_claim_without_aliasing
// @formal surface obligation=worker.sandbox_continuity_requires_a_binding
// @formal reason: Container pool and continuation claim reuse.
//! Warm container pool (ADR-0056 §warm-pool): pre-provisioned reusable capacity in
//! front of the container tier, addressing BOTH cold-start latency and resource
//! reuse. A pool keeps up to `size` **empty live environments** ready per container
//! shape; a matching Session binds one and execs its own Native/ACP processes.
//!
//! Safety: only **mount-less Ephemeral** specs are pooled by the single
//! [`poolable_shape`] admission below. A retained
//! Session requires receipt-fenced stable physical identity; `bind_scope` changes
//! only the in-process logical id and is not an ownership transfer. A per-session
//! mount also bakes session-specific bytes into the container
//! at create, so a pre-warmed container could not serve a different session. Each
//! physical warm container is fresh (never ran a prior session) and serves exactly
//! one session, so there is no cross-session/tenant contamination — the *pool* is
//! reused, not a used container. Reusing a container after a session would require a
//! workspace reset and is deliberately out of scope.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use awaken_lease_contract::mono::{self, Instant};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::{
    ContainerEffectFence, ContainerEnvironment, ContainerEnvironmentCapacity,
    ContainerEnvironmentProvider, ContainerProvider, ContainerRuntime, ContainerSandbox,
};

pub mod lease;
use lease::{LeaseStore, WARM_LEASE_RECONCILER, WarmLeaseLedger};
pub use lease::{WarmCapacityLeasePolicy, WarmCapacityLeases, WarmCapacityReclamation};

/// A process-global sequence for warm container scopes, so names are unique across
/// every pool in this process (a per-pool counter would collide when two pools warm
/// the same shape). Paired with the pool incarnation below, warm names are unique
/// across processes too.
static WARM_SEQ: AtomicU64 = AtomicU64::new(0);

/// The one warm-capacity admission rule. `SandboxCapacityShapeId` remains the
/// neutral placement identity; only this owner decides whether physical
/// capacity may be transferred by rebinding a logical scope.
fn poolable_shape(spec: &pc::SandboxSpec) -> Option<pc::SandboxCapacityShapeId> {
    (spec.filesystem_continuity == pc::FilesystemContinuity::Ephemeral)
        .then(|| pc::SandboxCapacityShapeId::from_spec(spec))
        .flatten()
}

/// A globally-unique scope (container name) for a warm container.
///
/// `incarnation` is minted by [`crate::runtime_owner_id`] — the same authority
/// that stamps `RUNTIME_OWNER_LABEL` on every container this Worker creates —
/// rather than by the bare pid it replaced. What that buys, exactly:
///
/// * A bare pid is recycled by the kernel. The physical container name is a
///   pure function of namespace+scope, so a restarted Worker that drew the dead
///   Worker's pid would address the dead Worker's leaked warm container. Warm
///   creates are unfenced, and `existing_realization_decision` refuses to
///   recover *any* existing realization without a durable effect fence, so that
///   collision does not adopt or replace — it fails the create permanently. The
///   incarnation (pid + nanosecond epoch + sequence) cannot be drawn twice.
/// * The leaked container carries that same incarnation in its owner label, so
///   a leak is attributable to the dead Worker rather than anonymous.
///
/// What a name does **not** buy is reclamation: the fenced disposal path needs a
/// durable receipt warm capacity deliberately never has, and a deterministic
/// cross-process name would collide with a container already checked out to a
/// Session. Reclamation is instead owned by the lease in [`lease`]: when the
/// pool runs with [`WarmContainerPool::with_capacity_leases`], every warm scope
/// minted here gets an enumerable host record *before* its create can be
/// polled, and a later incarnation removes the scope's container through the
/// unreceipted-capacity port once — and only once — that record has expired
/// without being checked out. What that covers and what it does not (a runtime
/// with no lease domain such as Kubernetes; a daemon create that lands more than
/// one TTL after its creator's last renewal) is stated in [`lease`].
fn next_warm_scope(incarnation: &str) -> String {
    format!(
        "warmpool-{incarnation}-{}",
        WARM_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A freshly minted warm scope. When the pool is leased its record already
/// exists; this value is the only way to reach [`WarmContainerPool::claimed_warm_create`],
/// so no create can be built for a scope whose record was not published first.
struct WarmScope {
    scope: String,
    leased: bool,
}

fn mint_warm_scope(
    leases: Option<&WarmLeaseLedger>,
    incarnation: &str,
) -> Result<WarmScope, pc::SandboxError> {
    let scope = next_warm_scope(incarnation);
    if let Some(leases) = leases {
        leases.mint(&scope)?;
    }
    Ok(WarmScope {
        scope,
        leased: leases.is_some(),
    })
}

/// Dispose one never-checked-out warm container and settle its record. A leased
/// pool first claims its own record (L5), so a container whose record a
/// reclaimer already took is left to that reclaimer; the claim is deleted once
/// the runtime confirmed removal, otherwise left to expire so reclamation
/// (which treats absence as success) finishes the job.
async fn retire<R: ContainerRuntime + 'static>(
    leases: Option<&WarmLeaseLedger>,
    sandbox: ContainerSandbox<R>,
) {
    let Some(leases) = leases else {
        let _ = sandbox.dispose().await;
        return;
    };
    let Some(claim) = leases.begin_disposal(&sandbox.id) else {
        return;
    };
    let removed = sandbox.dispose().await.is_ok();
    leases.finish_disposal(&claim, removed);
}

/// L5/L6: renew every record this pool holds, forget ready containers whose
/// record a reclaimer took, and retire ready containers the runtime no longer
/// reports running.
async fn renew_capacity<R: ContainerRuntime + 'static>(
    leases: &WarmLeaseLedger,
    warm: &Mutex<HashMap<pc::SandboxCapacityShapeId, WarmEntry<R>>>,
    inner: &ContainerProvider<R>,
) {
    let lost = leases.renew_held();
    let probes = {
        let mut map = warm.lock().expect("warm pool mutex");
        let mut probes = Vec::new();
        for entry in map.values_mut() {
            // A lost record belongs to the reclaimer that claimed it; this pool
            // must neither hand the container out nor remove it a second time.
            entry.ready.retain(|sandbox| !lost.contains(&sandbox.id));
            probes.extend(
                entry
                    .ready
                    .iter()
                    .map(|sandbox| (sandbox.id.clone(), sandbox.container_id.clone())),
            );
        }
        probes
    };
    for (scope, container_id) in probes {
        if inner.runtime.touch_lease(&container_id).await.is_ok() {
            continue;
        }
        let vanished = {
            let mut map = warm.lock().expect("warm pool mutex");
            map.values_mut().find_map(|entry| {
                let index = entry.ready.iter().position(|sandbox| sandbox.id == scope)?;
                Some(entry.ready.remove(index))
            })
        };
        if let Some(sandbox) = vanished {
            retire(Some(leases), sandbox).await;
        }
    }
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
            let _ = sandbox.dispose().await;
            Err(pc::SandboxError::backend(
                "ready_candidate",
                "prewarmed container did not become ready",
            ))
        }
        Err(error) => {
            let _ = sandbox.dispose().await;
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

/// A warm-capacity adapter around the canonical [`ContainerEnvironmentProvider`].
/// It owns only never-used physical capacity; a checked-out Session Environment
/// remains owned by the caller through the same provider contract as a cold create.
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
    /// Warm scopes whose create was started and whose caller then went away.
    ///
    /// A scope lands here only through an [`pc::EffectClaim::in_process`]
    /// retention, which runs on exactly one transition: the claimed create
    /// dropped unsettled. A create that returned — `Ok` or `Err` — settles and
    /// never records, because a returned container is in hand and a returned
    /// error owns whatever it half-created.
    unreceipted_scopes: Arc<Mutex<BTreeSet<String>>>,
    /// This pool's incarnation, mixed into every warm scope. See
    /// [`next_warm_scope`].
    incarnation: String,
    /// Host lease records over this pool's warm capacity, when enabled by
    /// [`Self::with_capacity_leases`].
    leases: Option<Arc<WarmLeaseLedger>>,
    /// The renewal/reclamation loop, stopped at the end of [`Self::shutdown`].
    maintenance: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Completed maintenance rounds, observable by tests without timing.
    maintenance_rounds: tokio::sync::watch::Sender<u64>,
}

impl<R: ContainerRuntime + 'static> WarmContainerPool<R> {
    /// Wrap `inner`, keeping up to `size` warm containers ready per shape. `size == 0`
    /// is a pass-through (every Environment checkout creates fresh) — the safe
    /// default when a deployment opts out of the warm cost.
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
            unreceipted_scopes: Arc::new(Mutex::new(BTreeSet::new())),
            incarnation: crate::runtime_owner_id(),
            leases: None,
            maintenance: Mutex::new(None),
            maintenance_rounds: tokio::sync::watch::channel(0).0,
        }
    }

    /// Put this pool's warm capacity under host lease records (see [`lease`]).
    ///
    /// Before returning it runs one reclamation pass, so a restarted Worker
    /// removes whatever a dead incarnation left behind whose lease has already
    /// expired; and unless the configuration opted out it spawns the loop that
    /// renews this pool's records every renew interval and reclaims expired
    /// ones every reclaim interval. A runtime that names no lease domain cannot
    /// participate and is refused rather than silently left unleased.
    pub async fn with_capacity_leases(
        mut self,
        config: WarmCapacityLeases,
    ) -> Result<Self, pc::SandboxError> {
        let domain = self.inner.runtime.capacity_lease_domain().ok_or_else(|| {
            pc::SandboxError::unsupported(
                "this container runtime names no warm capacity lease domain",
            )
        })?;
        let store = LeaseStore::open(&config, &domain, &self.incarnation)?;
        let leases = Arc::new(WarmLeaseLedger::new(store));
        self.leases = Some(leases.clone());
        leases.reclaim(&self.inner).await?;
        if config.maintained() {
            let handle = self.spawn_lease_maintenance(&leases, config.policy());
            *self
                .maintenance
                .lock()
                .expect("warm lease maintenance mutex") = Some(handle);
        }
        Ok(self)
    }

    /// L5: the loop that keeps this pool's records alive. It holds only weak
    /// references, so a pool dropped without `shutdown` stops renewing and its
    /// capacity becomes reclaimable one TTL later.
    fn spawn_lease_maintenance(
        &self,
        leases: &Arc<WarmLeaseLedger>,
        policy: WarmCapacityLeasePolicy,
    ) -> tokio::task::JoinHandle<()> {
        let leases = Arc::downgrade(leases);
        let warm = Arc::downgrade(&self.warm);
        let inner = Arc::downgrade(&self.inner);
        let closed = self.closed.clone();
        let rounds = self.maintenance_rounds.clone();
        let renew_interval = policy.renew_interval();
        let reclaim_every = policy
            .reclaim_interval()
            .as_millis()
            .div_ceil(renew_interval.as_millis().max(1))
            .max(1);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(renew_interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut round: u128 = 0;
            loop {
                ticks.tick().await;
                if closed.load(Ordering::SeqCst) {
                    return;
                }
                let (Some(leases), Some(warm), Some(inner)) =
                    (leases.upgrade(), warm.upgrade(), inner.upgrade())
                else {
                    return;
                };
                renew_capacity(&leases, &warm, &inner).await;
                round += 1;
                if round.is_multiple_of(reclaim_every) {
                    let _ = leases.reclaim(&inner).await;
                }
                rounds.send_modify(|completed| *completed += 1);
            }
        })
    }

    /// Renew this pool's lease records now (L5), and drop ready capacity whose
    /// record was lost or whose container stopped (L6). The maintenance loop
    /// calls this every renew interval; a caller that opted out of it must.
    pub async fn renew_capacity_leases(&self) -> Result<(), pc::SandboxError> {
        let leases = self.lease_ledger()?;
        renew_capacity(leases, &self.warm, &self.inner).await;
        Ok(())
    }

    /// Reclaim every expired, never-checked-out warm scope in this pool's lease
    /// domain — whichever incarnation minted it (L1/L2/L6).
    pub async fn reclaim_expired_capacity(
        &self,
    ) -> Result<WarmCapacityReclamation, pc::SandboxError> {
        self.lease_ledger()?.reclaim(&self.inner).await
    }

    fn lease_ledger(&self) -> Result<&WarmLeaseLedger, pc::SandboxError> {
        self.leases
            .as_deref()
            .ok_or_else(|| pc::SandboxError::unsupported("warm capacity is not under host leases"))
    }

    #[cfg(test)]
    pub(crate) fn maintenance_rounds(&self) -> tokio::sync::watch::Receiver<u64> {
        self.maintenance_rounds.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn lease_ledger_for_test(&self) -> &WarmLeaseLedger {
        self.leases.as_deref().expect("leased pool")
    }

    /// Start one warm create whose responsibility record exists before the
    /// create can be polled.
    ///
    /// [`pc::ClaimedEffect::new`] takes the claim by value, so the retention —
    /// which knows the exact scope this create is about to realize — is minted
    /// strictly before the future is built. There is no ordering left for a
    /// reviewer to get wrong: the only way to reach the runtime is through the
    /// awaited effect, and by then the record is already able to name what the
    /// runtime is about to create.
    ///
    /// Both the readiness probe and the create live inside the claim, because
    /// both windows strand the same container: the probe's own disposal is an
    /// await, and a task dropped there leaves a running container the pool has
    /// otherwise never heard of.
    ///
    /// A leased scope's claim is durable: its host record was published by
    /// [`mint_warm_scope`] before `warm` existed, and [`WARM_LEASE_RECONCILER`]
    /// reads it back after this process is gone. An unleased pool keeps the
    /// in-process claim, which covers cancellation but not process death.
    fn claimed_warm_create<'a>(
        inner: &'a ContainerProvider<R>,
        unreceipted_scopes: &Arc<Mutex<BTreeSet<String>>>,
        warm: WarmScope,
        warm_spec: &'a pc::SandboxSpec,
    ) -> pc::ClaimedEffect<'a, ContainerSandbox<R>> {
        debug_assert_eq!(
            warm.scope, warm_spec.scope,
            "the spec realizes the minted scope"
        );
        let scopes = unreceipted_scopes.clone();
        let scope = warm.scope;
        let in_flight = move || {
            scopes
                .lock()
                .expect("warm unreceipted scope mutex")
                .insert(scope);
        };
        let claim = if warm.leased {
            pc::EffectClaim::durable(WARM_LEASE_RECONCILER, in_flight)
        } else {
            pc::EffectClaim::in_process(in_flight)
        };
        pc::ClaimedEffect::new(claim, async move {
            ready_candidate(inner.create_container(warm_spec).await?).await
        })
    }

    /// Warm scopes currently on record as created-by-nobody. Read by
    /// [`Self::shutdown`] and by the cancellation tests.
    pub(crate) fn unreceipted_warm_scopes(&self) -> Vec<String> {
        self.unreceipted_scopes
            .lock()
            .expect("warm unreceipted scope mutex")
            .iter()
            .cloned()
            .collect()
    }

    /// Dispose every warm scope on record, clearing each only once the runtime
    /// reports its object gone. A scope whose removal fails stays on record, so
    /// the drain below keeps it visible instead of reporting a cleanup that did
    /// not happen.
    async fn dispose_unreceipted_scopes(&self) {
        for scope in self.unreceipted_warm_scopes() {
            let claim = match self.leases.as_deref() {
                None => None,
                Some(leases) => match leases.begin_disposal(&scope) {
                    Some(claim) => Some(claim),
                    // A reclaimer holds the record: the removal is its own.
                    None => {
                        self.unreceipted_scopes
                            .lock()
                            .expect("warm unreceipted scope mutex")
                            .remove(&scope);
                        continue;
                    }
                },
            };
            let removed = self
                .inner
                .dispose_unreceipted_capacity(&scope)
                .await
                .is_ok();
            if removed {
                self.unreceipted_scopes
                    .lock()
                    .expect("warm unreceipted scope mutex")
                    .remove(&scope);
            }
            if let (Some(leases), Some(claim)) = (self.leases.as_deref(), claim) {
                leases.finish_disposal(&claim, removed);
            }
        }
    }

    fn begin_create(&self) -> Result<InFlightCreate, pc::SandboxError> {
        self.in_flight_creates.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.in_flight_creates.fetch_sub(1, Ordering::SeqCst);
            self.create_changed.notify_waiters();
            return Err(pc::SandboxError::gone(
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
        let Some(key) = poolable_shape(spec) else {
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
                let now = mono::now();
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
                retire(self.leases.as_deref(), sandbox).await;
            }
            if ready >= target {
                return Ok(ready);
            }
            let create = self.begin_create()?;
            let warm = mint_warm_scope(self.leases.as_deref(), &self.incarnation)?;
            let mut warm_spec = spec.clone();
            warm_spec.scope = warm.scope.clone();
            let in_flight =
                Self::claimed_warm_create(&self.inner, &self.unreceipted_scopes, warm, &warm_spec);
            let sandbox = match in_flight.await {
                Ok(sandbox) => sandbox,
                Err(error) => {
                    // A returned error owns what it half-created; the record
                    // is left to expire so reclamation removes any remainder.
                    if let Some(leases) = &self.leases {
                        leases.abandon(&warm_spec.scope);
                    }
                    return Err(error);
                }
            };
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
                    last_desired: mono::now(),
                });
                entry.last_desired = mono::now();
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
                retire(self.leases.as_deref(), sandbox).await;
            }
            if let Some(excess) = excess {
                retire(self.leases.as_deref(), excess).await;
                if self.closed.load(Ordering::SeqCst) {
                    return Err(pc::SandboxError::gone(
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
        poolable_shape(spec)
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
        let Some(key) = poolable_shape(spec) else {
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
            retire(self.leases.as_deref(), sandbox).await;
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
                retire(self.leases.as_deref(), sandbox).await;
            }
            // The ready set is only the capacity that arrived. A create whose
            // caller was cancelled never reaches it, so draining `warm` alone is
            // what let a running container survive the drain that was supposed
            // to be a complete teardown.
            self.dispose_unreceipted_scopes().await;
            let changed = self.create_changed.notified();
            if self.in_flight_creates.load(Ordering::SeqCst) == 0 {
                // No create can begin after `closed`; drain once more to cover the
                // insertion that happened immediately before the count reached zero.
                // A scope whose removal failed stays on record and is
                // deliberately not part of this condition: the drain is
                // best-effort per container, and blocking teardown on a backend
                // that keeps refusing would hang the Worker instead of
                // reporting the leak. `unreceipted_warm_scopes` is what still
                // names it afterwards.
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
        // L5: every record for a container this drain disposed is already
        // released. What is still held names capacity whose removal failed; it
        // stops being renewed so a later reclaimer finishes it.
        if let Some(handle) = self
            .maintenance
            .lock()
            .expect("warm lease maintenance mutex")
            .take()
        {
            handle.abort();
        }
        if let Some(leases) = &self.leases {
            leases.abandon_all();
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
        let unreceipted_scopes = self.unreceipted_scopes.clone();
        let incarnation = self.incarnation.clone();
        let leases = self.leases.clone();
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
                let Ok(minted) = mint_warm_scope(leases.as_deref(), &incarnation) else {
                    // No record, no create (L4): fail closed, no spin.
                    return;
                };
                let mut warm_spec = template;
                warm_spec.scope = minted.scope.clone();
                // The same claim as the request path. A detached task is not
                // cancelled by its caller, but it is still dropped when the
                // runtime it was spawned onto shuts down, and that drop strands
                // the identical container.
                let in_flight =
                    Self::claimed_warm_create(&inner, &unreceipted_scopes, minted, &warm_spec);
                match in_flight.await {
                    Ok(sandbox) => {
                        // Re-check the cap under the lock; dispose an over-cap create.
                        let over = {
                            let mut map = warm.lock().expect("warm pool mutex");
                            let total = map.values().map(|entry| entry.ready.len()).sum::<usize>();
                            match map.get_mut(&key) {
                                Some(e) if e.ready.len() < size && total < total_size => {
                                    e.ready.push(sandbox);
                                    None
                                }
                                _ => Some(sandbox),
                            }
                        };
                        if let Some(excess) = over {
                            retire(leases.as_deref(), excess).await;
                            return;
                        }
                    }
                    // A create or readiness failure stops replenishing this key
                    // (fail closed, no spin); its record is left to expire.
                    Err(_) => {
                        if let Some(leases) = &leases {
                            leases.abandon(&warm_spec.scope);
                        }
                        return;
                    }
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
impl<R: ContainerRuntime + 'static> ContainerEnvironmentProvider for WarmContainerPool<R> {
    async fn prewarm_images(&self, specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        self.inner.prewarm_images(specs).await
    }

    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        self.inner.sandbox_capabilities()
    }

    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.inner.install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.inner.install_secret_broker(broker);
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.inner.probe_ready().await
    }

    async fn create_unattached_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        let Some(key) = poolable_shape(spec) else {
            return self.inner.create_unattached_environment(spec).await;
        };
        let environment = loop {
            let warm = {
                let mut map = self.warm.lock().expect("warm pool mutex");
                let entry = map.entry(key.clone()).or_insert_with(|| WarmEntry {
                    template: spec.clone(),
                    ready: Vec::new(),
                    last_desired: mono::now(),
                });
                entry.last_desired = mono::now();
                entry.ready.pop()
            };
            let Some(candidate) = warm else {
                break self.inner.create_container(spec).await?;
            };
            // L3: the container leaves warm capacity only by consuming its
            // lease path. A candidate whose record a reclaimer already claimed
            // is the claimant's to remove, so it is forgotten, never handed out.
            if self
                .leases
                .as_deref()
                .is_none_or(|leases| leases.check_out(&candidate.id))
            {
                break candidate.bind_scope(spec.scope.clone());
            }
        };
        self.spawn_replenish(key);
        Ok(Arc::new(environment))
    }

    fn adoption_layout(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<awaken_container_contract::ContainerAdoptionLayout, pc::SandboxError> {
        self.inner.adoption_layout(handle)
    }

    fn physical_incarnation(&self, handle: &pc::SandboxHandle) -> Result<String, pc::SandboxError> {
        self.inner.physical_incarnation(handle)
    }

    async fn create_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: &ContainerEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
        intent: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        // Cause/effect rule: a durable fence requires exact provider
        // realization identity. A warm candidate has none and `bind_scope`
        // cannot rewrite its physical labels/name/UID, so a fenced effect always
        // delegates without touching ready capacity. Unattached Ephemeral
        // Create (`create_unattached_environment`) is the one safe checkout row.
        self.inner
            .create_environment_for_effect(spec, effect_fence, authority, intent)
            .await
    }

    async fn observe_environment(
        &self,
        adoption: crate::ContainerEnvironmentAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.inner.observe_environment(adoption, basis).await
    }

    async fn adopt_unattached_environment(
        &self,
        adoption: crate::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        self.inner.adopt_unattached_environment(adoption).await
    }

    async fn adopt_environment_for_effect(
        &self,
        adoption: crate::ContainerEnvironmentAdoption<'_>,
        effect_fence: &ContainerEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        self.inner
            .adopt_environment_for_effect(adoption, effect_fence, authority)
            .await
    }

    async fn prepare_terminal_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&ContainerEffectFence>,
        terminal_effect_fence: &ContainerEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        // Terminal cleanup is never a capacity checkout: preserve the frozen
        // handle and aggregate-owned fence by delegating to the exact provider
        // that owns observation, participant reconstruction, and removal.
        self.inner
            .prepare_terminal_environment_for_effect(
                spec,
                handle,
                expected_effect_fence,
                terminal_effect_fence,
                authority,
            )
            .await
    }

    fn restore_targets(
        &self,
    ) -> Option<&dyn pc::RestoreTargetProvider<Arc<dyn ContainerEnvironment>>> {
        Some(self)
    }

    fn restorable(&self) -> Option<&dyn pc::RestorableProvider<Arc<dyn ContainerEnvironment>>> {
        // Warm capacity never changes what the inner provider can restore.
        // The inner ContainerProvider owns restore targets but not byte
        // restoration, so the pool advertises exactly that.
        None
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::RestoreTargetProvider<Arc<dyn ContainerEnvironment>>
    for WarmContainerPool<R>
{
    async fn acquire_restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        // Exact restore delegation table: acquisition, byte restoration, and
        // disposal all bypass warm capacity and preserve the same immutable
        // request. The inner provider remains the sole physical target owner.
        self.inner.acquire_restore_environment(spec, request).await
    }

    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.inner
            .dispose_restored_environment(spec, request, authorization, authority)
            .await
    }
}

/// Cancellation cases live beside the shared fake in [`tests`] rather than in
/// it, so the drop-mid-create scenarios stay readable next to each other.
#[cfg(test)]
mod cancellation_tests;

/// Warm-capacity lease cases (rules L1–L7 in [`lease`]), against a fake daemon
/// shared by several pools so a restart is two pools, not one.
#[cfg(test)]
mod lease_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RealizationPhase {
        scope: String,
        adoption: String,
        effect_operation: Option<String>,
        attempt: String,
        has_physical_fingerprint: bool,
    }

    pub(super) struct RecordingRuntime {
        pub(super) creates: AtomicUsize,
        image_demands: Mutex<Vec<Vec<String>>>,
        fenced_creates: AtomicUsize,
        realization_phases: Mutex<Vec<RealizationPhase>>,
        pub(super) removes: AtomicUsize,
        pub(super) ready: AtomicBool,
        pub(super) block_create: AtomicBool,
        pub(super) create_started: tokio::sync::Notify,
        pub(super) allow_create: tokio::sync::Semaphore,
        /// Scopes removed through the unreceipted-capacity port, in order.
        /// A fake must record this separately from `removes`: the two ports
        /// differ in exactly the fact under test — one needs a container in
        /// hand, the other only the name of one nobody ever held.
        pub(super) unreceipted_removals: Mutex<Vec<String>>,
    }

    impl RecordingRuntime {
        pub(super) fn ready() -> Self {
            Self {
                creates: AtomicUsize::new(0),
                image_demands: Mutex::new(Vec::new()),
                fenced_creates: AtomicUsize::new(0),
                realization_phases: Mutex::new(Vec::new()),
                removes: AtomicUsize::new(0),
                ready: AtomicBool::new(true),
                block_create: AtomicBool::new(false),
                create_started: tokio::sync::Notify::new(),
                allow_create: tokio::sync::Semaphore::new(0),
                unreceipted_removals: Mutex::new(Vec::new()),
            }
        }
    }

    #[tokio::test]
    async fn image_preparation_passes_retained_specs_through_pool_without_allocating_capacity() {
        // Causes: C1 ephemeral/retained; C2 duplicated specs; C3 pending packages
        // or customer mounts. Effects W1 C1 doesn't gate image demand (unlike
        // capacity); W2 duplicates dedup; W3 unprepared/packages/mounts do not
        // trigger alternate building/materialization; W4 zero physical creates.
        let runtime = Arc::new(RecordingRuntime::ready());
        let provider = Arc::new(ContainerProvider::for_test(
            runtime.clone(),
            "sandbox:default",
        ));
        let pool = WarmContainerPool::new(provider, 2);
        for continuity in [
            pc::FilesystemContinuity::Ephemeral,
            pc::FilesystemContinuity::Retained,
        ] {
            let mut input = spec("image-only", &[]);
            input.filesystem_continuity = continuity;
            pool.prewarm_images(&[input.clone(), input.clone()])
                .await
                .unwrap();
            assert_eq!(
                runtime.image_demands.lock().unwrap().last().unwrap(),
                &["sandbox:default"]
            );
            input
                .packages
                .managers
                .insert("pip".into(), vec!["package==1".into()]);
            pool.prewarm_images(&[input]).await.unwrap();
            assert!(
                runtime
                    .image_demands
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0, "W4");
    }

    #[async_trait]
    impl ContainerRuntime for RecordingRuntime {
        async fn prewarm_images(&self, images: &[String]) -> Result<(), crate::RuntimeError> {
            self.image_demands.lock().unwrap().push(images.to_vec());
            Ok(())
        }
        async fn probe_ready(&self) -> Result<(), crate::RuntimeError> {
            self.ready
                .load(Ordering::SeqCst)
                .then_some(())
                .ok_or_else(|| crate::RuntimeError::Backend("not ready".into()))
        }

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

        async fn preflight_create_for_effect(
            &self,
            context: &crate::ContainerRealizationContext<'_>,
            _plan: &crate::ContainerPlan,
            realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
        ) -> Result<(), crate::RuntimeError> {
            // This fake explicitly represents a runtime whose fenced
            // admission succeeds; R3 then measures whether the pool delegates
            // to its one effect-aware create path without consuming capacity.
            self.realization_phases
                .lock()
                .unwrap()
                .push(RealizationPhase {
                    scope: context.scope.to_owned(),
                    adoption: context.adoption_fingerprint.to_string(),
                    effect_operation: context
                        .effect_fence
                        .map(|fence| fence.operation_id().to_owned()),
                    attempt: context.attempt.as_str().to_owned(),
                    has_physical_fingerprint: realization_fingerprint.is_some(),
                });
            Ok(())
        }

        async fn prepare_for_effect(
            &self,
            context: &crate::ContainerRealizationContext<'_>,
            plan: &crate::ContainerPlan,
            _realization_fingerprint: &pc::SandboxRealizationFingerprint,
        ) -> Result<crate::PreparedContainer, crate::RuntimeError> {
            self.realization_phases
                .lock()
                .unwrap()
                .push(RealizationPhase {
                    scope: context.scope.to_owned(),
                    adoption: context.adoption_fingerprint.to_string(),
                    effect_operation: context
                        .effect_fence
                        .map(|fence| fence.operation_id().to_owned()),
                    attempt: context.attempt.as_str().to_owned(),
                    has_physical_fingerprint: true,
                });
            if context.effect_fence.is_some() {
                self.fenced_creates.fetch_add(1, Ordering::SeqCst);
            }
            self.create(context.scope, plan).await.map(|locator| {
                crate::PreparedContainer::fresh(locator, context.attempt.as_str().to_owned())
            })
        }

        async fn activate_for_effect(
            &self,
            _context: &crate::ContainerRealizationContext<'_>,
            _plan: &crate::ContainerPlan,
            _realization_fingerprint: &pc::SandboxRealizationFingerprint,
            prepared_locator: &str,
        ) -> Result<String, crate::RuntimeError> {
            Ok(prepared_locator.to_owned())
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

        async fn remove_unreceipted_scope(&self, scope: &str) -> Result<(), crate::RuntimeError> {
            self.unreceipted_removals
                .lock()
                .unwrap()
                .push(scope.to_owned());
            Ok(())
        }
    }

    pub(super) fn recording_pool(
        runtime: Arc<RecordingRuntime>,
        size: usize,
    ) -> WarmContainerPool<RecordingRuntime> {
        WarmContainerPool::new(
            Arc::new(ContainerProvider::for_test(runtime, "agent:test")),
            size,
        )
    }

    #[test]
    fn pool_advertises_exactly_the_inner_restore_ladder() {
        // Restore capability ladder (ADR-0081): C1 the inner ContainerProvider
        // owns restore targets but not checkpoint-byte restoration; C2 warm
        // capacity wraps it. R1 both rungs are typed on the pool: restore
        // targets present, restorable absent, so a composition root refuses
        // byte restore at selection instead of at the physical effect; R2 the
        // ladder law holds (restorable implies restore targets) for inner and
        // pool alike; R3 the pool's rung is the pool itself, never a leaked
        // inner reference, so warm-capacity bypass stays the pool's decision.
        let runtime = Arc::new(RecordingRuntime::ready());
        let inner = Arc::new(ContainerProvider::for_test(runtime, "agent:test"));
        assert!(inner.restore_targets().is_some(), "R1 inner targets");
        assert!(inner.restorable().is_none(), "R1 inner bytes");
        let pool = WarmContainerPool::new(inner.clone(), 1);
        assert!(pool.restore_targets().is_some(), "R1 pool targets");
        assert!(pool.restorable().is_none(), "R1 pool bytes");
        assert!(
            crate::restore_ladder_is_consistent(inner.as_ref()),
            "R2 inner"
        );
        assert!(crate::restore_ladder_is_consistent(&pool), "R2 pool");
        let rung: *const dyn pc::RestoreTargetProvider<Arc<dyn ContainerEnvironment>> =
            pool.restore_targets().unwrap();
        assert!(std::ptr::addr_eq(rung, &pool as *const _), "R3");
    }

    #[cfg(feature = "podman")]
    #[test]
    fn pool_preserves_inner_provider_capability_evidence() {
        let runtime = Arc::new(crate::podman::PodmanRuntime::new(7600));
        let inner = Arc::new(ContainerProvider::for_test(runtime, "agent:test"));
        let expected = inner.sandbox_capabilities();
        let pool = WarmContainerPool::new(inner, 1);
        assert_eq!(pool.sandbox_capabilities(), expected);
        assert!(pool.sandbox_capabilities().network_isolation);
        assert!(!pool.sandbox_capabilities().enforced_network_allowlist);
    }

    pub(super) fn spec(scope: &str, cmd: &[&str]) -> pc::SandboxSpec {
        pc::SandboxSpec {
            process_tree_supervision: false,
            scope: scope.into(),
            isolation: pc::IsolationClass::Container,
            environment: None,
            command: cmd.iter().map(|part| (*part).to_owned()).collect(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Ephemeral,
            control_services: Default::default(),
            lease_ttl_secs: None,
        }
    }

    fn retained_spec(scope: &str) -> pc::SandboxSpec {
        let mut spec = spec(scope, &[]);
        spec.filesystem_continuity = pc::FilesystemContinuity::Retained;
        spec
    }

    #[tokio::test]
    async fn retained_and_fenced_realizations_never_consume_warm_capacity() {
        // Cause/effect table: C1 continuity Ephemeral/Retained; C2 mountless;
        // C3 effect fence absent/present; C4 physical fingerprint is unavailable
        // before package resolution, then available after resolution and required
        // at create. R1 only Ephemeral+mountless+unfenced
        // Create may prewarm/checkout; R2 Retained capacity remains zero and
        // cold-creates; R3 any fenced create delegates exactly once and leaves
        // the ready candidate untouched; R4 both preflights and create observe
        // one stable scope/adoption/fence/intent-attempt context while the typed
        // physical argument progresses None -> Some -> required.
        let runtime = Arc::new(RecordingRuntime::ready());
        let pool = recording_pool(runtime.clone(), 1);
        let ephemeral = spec("ephemeral", &[]);
        assert_eq!(pool.prewarm(&ephemeral, 1).await.unwrap(), 1, "R1");

        let retained = retained_spec("retained");
        assert_eq!(pool.prewarm(&retained, 1).await.unwrap(), 0, "R2");
        let retained_environment = pool.create_unattached_environment(&retained).await.unwrap();
        assert_eq!(pool.ready_len(&retained), 0, "R2");
        assert_eq!(pool.ready_len(&ephemeral), 1, "R2 preserves capacity");
        retained_environment.dispose_unattached().await.unwrap();

        let fence =
            crate::ContainerEffectFence::new("operation-1", "owner-1", "runtime-1", 1, u64::MAX)
                .unwrap();
        // R3's durable fence identifies the operation; its current proof is
        // independently required and must survive the pool's delegation.
        let authority = pc::test_support::EffectAuthority::new(&fence);
        let fenced = pool
            .create_environment_for_effect(
                &ephemeral,
                &fence,
                &authority,
                awaken_container_contract::EnvironmentRealizationIntent::Create,
            )
            .await
            .unwrap();
        assert_eq!(runtime.fenced_creates.load(Ordering::SeqCst), 1, "R3");
        assert_eq!(pool.ready_len(&ephemeral), 1, "R3");
        {
            let phases = runtime.realization_phases.lock().unwrap();
            let fenced_phases = &phases[phases.len() - 3..];
            assert_eq!(
                fenced_phases
                    .iter()
                    .map(|phase| phase.has_physical_fingerprint)
                    .collect::<Vec<_>>(),
                vec![false, true, true],
                "R4 physical phase"
            );
            assert!(
                fenced_phases.windows(2).all(|pair| {
                    pair[0].scope == pair[1].scope
                        && pair[0].adoption == pair[1].adoption
                        && pair[0].effect_operation == pair[1].effect_operation
                        && pair[0].attempt == pair[1].attempt
                }),
                "R4 stable context"
            );
        }
        fenced.dispose_unattached().await.unwrap();
        pool.shutdown().await;
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
            Arc::new(ContainerProvider::for_test(runtime.clone(), "agent:test")),
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
            Arc::new(ContainerProvider::for_test(
                runtime_one.clone(),
                "agent:test",
            )),
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
            Arc::new(ContainerProvider::for_test(
                runtime_two.clone(),
                "agent:test",
            )),
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

        let environment = pool
            .create_unattached_environment(&spec)
            .await
            .expect("E1 cold path");
        for _ in 0..100 {
            if runtime.creates.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.removes.load(Ordering::SeqCst), 1, "E2");
        assert_eq!(pool.ready_len(&spec), 0, "E3");
        environment.dispose_unattached().await.unwrap();
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
