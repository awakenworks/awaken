// @formal surface obligation=work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal surface obligation=work.terminal_lease_clear
// @formal reason: Narrow application roles preserve current lifecycle checks and delegate atomic storage operations.
//! Work use cases handed to adapters by their calling role.
use super::{EnvironmentExecutionApplication, EnvironmentExecutionError, session_access};
use awaken_executable_environment_contract::{
    self as storage, ClaimedWork, ExecutableEnvironmentLifecycle,
    ExecutableEnvironmentLifecycleSource, HeartbeatResult, LeaseHeartbeat, QueueStats, WorkItem,
    WorkQueueError, WorkSessionAccess,
};
use awaken_time::mono::{self, Instant, MonoDeadline};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, watch};

const EMPTY_RECHECK: Duration = Duration::from_millis(50);
type ActiveWaits = awaken_sync::LeafLock<HashMap<String, (Arc<WorkWait>, usize)>>;

/// Process-local prompts to recheck the original Work store.
#[derive(Clone, Default)]
pub(super) struct WorkWaits(Arc<ActiveWaits>);

struct WorkWait {
    generation: watch::Sender<u64>,
    empty: Mutex<Option<EmptyObservation>>,
}

struct EmptyObservation {
    reclaim_age: Option<u64>,
    generation: u64,
    recheck: Instant,
}

struct WorkWaitSubscription {
    owner: WorkWaits,
    environment_id: String,
    entry: Arc<WorkWait>,
}

impl WorkWaits {
    fn subscribe(&self, environment_id: &str) -> WorkWaitSubscription {
        let mut entries = self.0.lock();
        let (entry, subscribers) = entries.entry(environment_id.to_owned()).or_insert_with(|| {
            (
                Arc::new(WorkWait {
                    generation: watch::channel(0).0,
                    empty: Mutex::new(None),
                }),
                0,
            )
        });
        *subscribers += 1;
        WorkWaitSubscription {
            owner: self.clone(),
            environment_id: environment_id.into(),
            entry: entry.clone(),
        }
    }

    pub(super) fn notify(&self, environment_id: &str) {
        if let Some((entry, _)) = self.0.lock().get(environment_id) {
            entry
                .generation
                .send_modify(|value| *value = value.wrapping_add(1));
        }
    }
}

impl Drop for WorkWaitSubscription {
    fn drop(&mut self) {
        let mut entries = self.owner.0.lock();
        if let Some((_, subscribers)) = entries.get_mut(&self.environment_id) {
            *subscribers -= 1;
            if *subscribers == 0 {
                entries.remove(&self.environment_id);
            }
        }
    }
}

impl WorkWaitSubscription {
    async fn claim<T, E, F, Fut>(
        &self,
        deadline: MonoDeadline,
        reclaim_age: Option<u64>,
        probe: F,
    ) -> Result<Option<T>, E>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Option<T>, E>>,
    {
        let mut changes = self.entry.generation.subscribe();
        let mut initial = true;
        loop {
            let generation = *changes.borrow_and_update();
            let Ok(mut cached) =
                tokio::time::timeout_at(deadline.instant(), self.entry.empty.lock()).await
            else {
                return Ok(None);
            };
            let reuse = cached.as_ref().filter(|observation| {
                !initial
                    && observation.generation == generation
                    && observation.reclaim_age == reclaim_age
                    && observation.recheck > mono::now()
            });
            let recheck = if let Some(observation) = reuse {
                observation.recheck
            } else {
                // A request records its own initial poll. Later requests can
                // reuse an empty observation, never another Worker's grant.
                if !initial && deadline.has_passed() {
                    return Ok(None);
                }
                let claimed = probe().await?;
                if claimed.is_some() {
                    *cached = None;
                    return Ok(claimed);
                }
                let recheck = mono::now() + EMPTY_RECHECK;
                *cached = Some(EmptyObservation {
                    reclaim_age,
                    generation,
                    recheck,
                });
                recheck
            };
            initial = false;
            drop(cached);
            if tokio::time::timeout_at(recheck.min(deadline.instant()), changes.changed())
                .await
                .is_err()
                && deadline.has_passed()
            {
                return Ok(None);
            }
        }
    }
}

#[derive(Clone)]
struct WorkContext {
    execution_source: Arc<dyn ExecutableEnvironmentLifecycleSource>,
}
impl WorkContext {
    async fn require_environment(
        &self,
        environment_id: &str,
    ) -> Result<(), EnvironmentExecutionError> {
        match self
            .execution_source
            .current_lifecycle(environment_id)
            .await?
        {
            Some(
                ExecutableEnvironmentLifecycle::Active | ExecutableEnvironmentLifecycle::Archived,
            ) => Ok(()),
            None => Err(EnvironmentExecutionError::EnvironmentNotFound),
        }
    }
}

/// Read Work items and queue statistics without acquiring authority.
///
/// A read-only adapter needs no claim, mutation or registration-write methods.
/// ```
/// use std::sync::Arc;
/// use awaken_environment_application::execution::WorkQueries;
/// use awaken_executable_environment_contract::{WorkQueries as QueryStorage, ExecutableEnvironmentLifecycleSource};
/// fn read_only(storage: Arc<dyn QueryStorage>, environments: Arc<dyn ExecutableEnvironmentLifecycleSource>) -> WorkQueries {
///     WorkQueries::new(storage, environments)
/// }
/// ```
///
/// ```compile_fail
/// use awaken_environment_application::execution::WorkQueries;
/// async fn cannot_stop(queries: WorkQueries) {
///     queries.stop("env", "work", "owner").await;
/// }
/// ```
#[derive(Clone)]
pub struct WorkQueries {
    context: WorkContext,
    port: Arc<dyn storage::WorkQueries>,
}
impl WorkQueries {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::WorkQueries>,
        execution_source: Arc<dyn ExecutableEnvironmentLifecycleSource>,
    ) -> Self {
        Self {
            port,
            context: WorkContext { execution_source },
        }
    }
    pub async fn list(
        &self,
        environment_id: &str,
    ) -> Result<Vec<WorkItem>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.list(environment_id).await?)
    }
    pub async fn stats(
        &self,
        environment_id: &str,
    ) -> Result<QueueStats, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.stats(environment_id).await?)
    }
    pub async fn get(
        &self,
        environment_id: &str,
        work_id: &str,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.get(environment_id, work_id).await?)
    }
}

/// Claim work as an Environment Worker, issue its Session bearer, and mutate by credential owner.
#[derive(Clone)]
pub struct WorkConsumer {
    context: WorkContext,
    port: Arc<dyn storage::WorkConsumer>,
    waits: WorkWaits,
}
impl WorkConsumer {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::WorkConsumer>,
        execution_source: Arc<dyn ExecutableEnvironmentLifecycleSource>,
    ) -> Self {
        Self {
            port,
            waits: Default::default(),
            context: WorkContext { execution_source },
        }
    }
    pub async fn claim(
        &self,
        environment_id: &str,
        lease_owner: &str,
        poller_id: &str,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<ClaimedWork>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        let minted = session_access::mint_session_access();
        let claim = self
            .port
            .claim_with_session_access(
                environment_id,
                lease_owner,
                poller_id,
                reclaim_older_than_ms,
                &minted.digest,
            )
            .await?;
        claim
            .map(|claim| {
                let sessions_token = match claim.session_access {
                    None => None,
                    Some(bound) if bound == minted.digest => Some(minted.cleartext),
                    Some(_) => {
                        return Err(EnvironmentExecutionError::WorkQueue(
                            WorkQueueError::Storage(
                                "WorkQueue bound a Session access digest it was not given".into(),
                            ),
                        ));
                    }
                };
                Ok(ClaimedWork {
                    item: claim.item,
                    sessions_token,
                })
            })
            .transpose()
    }
    /// Wait for a Work grant without holding a store connection idle.
    pub async fn poll(
        &self,
        environment_id: &str,
        lease_owner: &str,
        poller_id: &str,
        reclaim_age: Option<u64>,
        wait: Option<Duration>,
    ) -> Result<Option<ClaimedWork>, EnvironmentExecutionError> {
        let probe = || self.claim(environment_id, lease_owner, poller_id, reclaim_age);
        match wait {
            None => probe().await,
            Some(wait) => {
                self.waits
                    .subscribe(environment_id)
                    .claim(MonoDeadline::after(wait), reclaim_age, probe)
                    .await
            }
        }
    }

    pub async fn acknowledge(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.ack(environment_id, work_id, worker_id).await?)
    }
    pub async fn heartbeat(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self
            .port
            .heartbeat(environment_id, work_id, worker_id, heartbeat)
            .await?)
    }
    pub async fn stop(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context.require_environment(environment_id).await?;
        let result = self.port.stop(environment_id, work_id, worker_id).await?;
        if matches!(result, storage::WorkMutationResult::Accepted(_)) {
            self.waits.notify(environment_id);
        }
        Ok(result)
    }
}

/// Authenticate a Session bearer against the current durable Work lease.
///
/// ```compile_fail
/// use awaken_environment_application::execution::SessionAccessAuthenticator;
/// async fn cannot_claim(authentication: SessionAccessAuthenticator) {
///     authentication.claim("env", "owner", "poller", None).await;
/// }
/// ```
#[derive(Clone)]
pub struct SessionAccessAuthenticator {
    port: Arc<dyn storage::SessionAccessAuthenticator>,
}
impl SessionAccessAuthenticator {
    /// Construct from the authentication capability alone.
    #[must_use]
    pub fn new(port: Arc<dyn storage::SessionAccessAuthenticator>) -> Self {
        Self { port }
    }
    /// Resolve the bearer against the live lease, judged on the Work store's
    /// own clock.
    pub async fn authenticate(
        &self,
        presented: &awaken_agent_contract::RedactedString,
    ) -> Result<Option<WorkSessionAccess>, EnvironmentExecutionError> {
        Ok(self
            .port
            .authenticate_session_access(&session_access::session_access_digest(presented))
            .await?)
    }
}

/// Acknowledge, renew or stop only the exact lease epoch recovered from a Session bearer.
#[derive(Clone)]
pub struct SessionWorkLeaseControl {
    context: WorkContext,
    port: Arc<dyn storage::SessionWorkLeaseControl>,
    waits: WorkWaits,
}
impl SessionWorkLeaseControl {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::SessionWorkLeaseControl>,
        execution_source: Arc<dyn ExecutableEnvironmentLifecycleSource>,
    ) -> Self {
        Self {
            port,
            waits: Default::default(),
            context: WorkContext { execution_source },
        }
    }
    pub async fn acknowledge(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context
            .require_environment(&access.environment_id)
            .await?;
        Ok(self.port.ack_with_session_access(access).await?)
    }
    pub async fn heartbeat(
        &self,
        access: &WorkSessionAccess,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, EnvironmentExecutionError> {
        self.context
            .require_environment(&access.environment_id)
            .await?;
        Ok(self
            .port
            .heartbeat_with_session_access(access, heartbeat)
            .await?)
    }
    pub async fn stop(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context
            .require_environment(&access.environment_id)
            .await?;
        let result = self.port.stop_with_session_access(access).await?;
        if matches!(result, storage::WorkMutationResult::Accepted(_)) {
            self.waits.notify(&access.environment_id);
        }
        Ok(result)
    }
}

/// Apply metadata upserts and removals; no lifecycle or lease mutation is exposed.
#[derive(Clone)]
pub struct WorkMetadataPatch {
    context: WorkContext,
    port: Arc<dyn storage::WorkMetadataPatch>,
}
impl WorkMetadataPatch {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::WorkMetadataPatch>,
        execution_source: Arc<dyn ExecutableEnvironmentLifecycleSource>,
    ) -> Self {
        Self {
            port,
            context: WorkContext { execution_source },
        }
    }
    pub async fn apply(
        &self,
        environment_id: &str,
        work_id: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self
            .port
            .update_metadata(environment_id, work_id, patch)
            .await?)
    }
}

impl EnvironmentExecutionApplication {
    #[must_use]
    pub fn work_queries(&self) -> WorkQueries {
        WorkQueries::new(self.work.queries(), self.execution_source.clone())
    }
    #[must_use]
    pub fn work_consumer(&self) -> WorkConsumer {
        let mut consumer = WorkConsumer::new(self.work.consumer(), self.execution_source.clone());
        consumer.waits = self.work_waits.clone();
        consumer
    }
    #[must_use]
    pub fn session_access(&self) -> SessionAccessAuthenticator {
        SessionAccessAuthenticator::new(self.work.authentication())
    }
    #[must_use]
    pub fn session_work_leases(&self) -> SessionWorkLeaseControl {
        let mut control = SessionWorkLeaseControl::new(
            self.work.session_control(),
            self.execution_source.clone(),
        );
        control.waits = self.work_waits.clone();
        control
    }
    #[must_use]
    pub fn work_metadata(&self) -> WorkMetadataPatch {
        WorkMetadataPatch::new(self.work.metadata(), self.execution_source.clone())
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    #[tokio::test(start_paused = true)]
    async fn idle_waiters_keep_their_initial_poll_and_share_empty_rechecks() {
        let waits = WorkWaits::default();
        let a = waits.subscribe("t");
        let b = waits.subscribe("t");
        let probes = AtomicUsize::new(0);
        let deadline = MonoDeadline::after(Duration::from_millis(120));
        let probe = || async {
            probes.fetch_add(1, Relaxed);
            Ok::<_, ()>(None::<String>)
        };
        assert_eq!(
            tokio::join!(
                a.claim(deadline, None, probe),
                b.claim(deadline, None, probe)
            ),
            (Ok(None), Ok(None))
        );
        assert_eq!(
            probes.load(Relaxed),
            4,
            "two initial polls and two shared rechecks"
        );
        drop(a);
        assert_eq!(waits.0.lock().len(), 1);
        drop(b);
        assert!(waits.0.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn notice_during_empty_probe_rechecks_without_waiting_for_the_timer() {
        let waits = WorkWaits::default();
        let watch = waits.subscribe("t");
        let probes = AtomicUsize::new(0);
        let start = mono::now();
        let result = watch
            .claim(
                MonoDeadline::after(Duration::from_millis(200)),
                None,
                || async {
                    if probes.fetch_add(1, Relaxed) == 0 {
                        waits.notify("t");
                        Ok::<_, ()>(None)
                    } else {
                        Ok(Some("claimed by this caller"))
                    }
                },
            )
            .await;
        assert_eq!(result, Ok(Some("claimed by this caller")));
        assert_eq!(mono::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_notice_recovers_via_a_bounded_recheck() {
        let waits = WorkWaits::default();
        let watch = waits.subscribe("t");
        let start = mono::now();
        let result = watch
            .claim(
                MonoDeadline::after(Duration::from_millis(200)),
                None,
                || async {
                    Ok::<_, ()>((mono::now() >= start + EMPTY_RECHECK).then_some("reclaimable"))
                },
            )
            .await;
        assert_eq!(result, Ok(Some("reclaimable")));
        assert_eq!(mono::now() - start, EMPTY_RECHECK);
    }
}
