//! Dynamic hand placement over the authoritative DWR worker directory.
//!
//! Worker identity, incarnation, liveness, load, capabilities and eligibility
//! are owned by `awaken-worker-contract` and `awaken-worker-registry`. This
//! module deliberately stores only process-local executor channels. Keeping the
//! two concerns separate prevents a channel cache from becoming a second,
//! contradictory worker registry.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::tool::{
    ToolExecutor, ToolExecutorProvider, ToolExecutorSelectionError,
};
use awaken_worker_registry::{
    ExecutionLocation, PlacementContext, PlacementError, PlacementPolicy, RankedWorker,
    WorkerDirectory, WorkerIdentity, WorkerSnapshot, place,
};

/// Process-local channel cache keyed by a complete worker incarnation.
/// Re-registering a logical worker never lets an old channel serve the new
/// generation because generation and incarnation are part of the key.
#[derive(Default)]
pub struct WorkerExecutorDirectory {
    channels: RwLock<HashMap<WorkerIdentity, Arc<dyn ToolExecutor>>>,
}

impl WorkerExecutorDirectory {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, identity: WorkerIdentity, executor: Arc<dyn ToolExecutor>) {
        self.channels
            .write()
            .expect("worker executor directory poisoned")
            .insert(identity, executor);
    }

    pub fn deregister(&self, identity: &WorkerIdentity) -> Option<Arc<dyn ToolExecutor>> {
        self.channels
            .write()
            .expect("worker executor directory poisoned")
            .remove(identity)
    }

    #[must_use]
    pub fn resolve(&self, identity: &WorkerIdentity) -> Option<Arc<dyn ToolExecutor>> {
        self.channels
            .read()
            .expect("worker executor directory poisoned")
            .get(identity)
            .cloned()
    }
}

/// Atomically replaceable policy slot. The immutable eligibility kernel remains
/// outside the extension: a replacement may only rank the candidates already
/// admitted by [`place`]. Replacing the slot affects the next decision; an
/// executor already returned for a run remains pinned by its `Arc`.
pub struct ReplaceablePlacementPolicy {
    policy: RwLock<Arc<dyn PlacementPolicy>>,
}

impl PlacementPolicy for ReplaceablePlacementPolicy {
    fn id(&self) -> &str {
        "replaceable"
    }

    fn rank(
        &self,
        context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError> {
        self.policy
            .read()
            .expect("placement policy slot poisoned")
            .rank(context, eligible)
    }
}

static SHARED_POLICY: OnceLock<Arc<ReplaceablePlacementPolicy>> = OnceLock::new();

/// Process-wide DWR policy slot used by the worker dispatch transport. Extension
/// composition may replace it; the next ordinary claim observes the new policy.
#[must_use]
pub fn shared_worker_placement_policy() -> Arc<ReplaceablePlacementPolicy> {
    SHARED_POLICY
        .get_or_init(|| {
            ReplaceablePlacementPolicy::new(Arc::new(awaken_worker_registry::LeastLoadedPolicy))
        })
        .clone()
}

impl ReplaceablePlacementPolicy {
    #[must_use]
    pub fn new(policy: Arc<dyn PlacementPolicy>) -> Arc<Self> {
        Arc::new(Self {
            policy: RwLock::new(policy),
        })
    }

    #[must_use]
    pub fn active_id(&self) -> String {
        self.policy
            .read()
            .expect("placement policy slot poisoned")
            .id()
            .to_string()
    }

    pub fn replace(&self, policy: Arc<dyn PlacementPolicy>) -> Arc<dyn PlacementPolicy> {
        std::mem::replace(
            &mut *self.policy.write().expect("placement policy slot poisoned"),
            policy,
        )
    }

    fn select(
        &self,
        context: &PlacementContext,
        workers: &[WorkerSnapshot],
        now_ms: u64,
    ) -> Result<RankedWorker, PlacementError> {
        let policy = self.policy.read().expect("placement policy slot poisoned");
        place(policy.as_ref(), context, workers, now_ms)
    }
}

pub type PlacementContextExtractor = Arc<dyn Fn(&RunActivation) -> PlacementContext + Send + Sync>;
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[must_use]
pub fn default_placement_context(activation: &RunActivation) -> PlacementContext {
    PlacementContext {
        run_id: activation.run_id.0.clone(),
        workspace_id: String::new(),
        requirements: Default::default(),
        recovered: false,
        previous_worker: None,
        attributes: Default::default(),
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Dynamic provider composed from the one durable worker directory, a thin
/// executor-channel cache, and a replaceable ranking policy.
pub struct DynamicToolExecutorProvider {
    workers: Arc<dyn WorkerDirectory>,
    executors: Arc<WorkerExecutorDirectory>,
    policy: Arc<ReplaceablePlacementPolicy>,
    extractor: PlacementContextExtractor,
    clock: Clock,
}

impl DynamicToolExecutorProvider {
    #[must_use]
    pub fn new(
        workers: Arc<dyn WorkerDirectory>,
        executors: Arc<WorkerExecutorDirectory>,
        policy: Arc<ReplaceablePlacementPolicy>,
    ) -> Self {
        Self {
            workers,
            executors,
            policy,
            extractor: Arc::new(default_placement_context),
            clock: Arc::new(unix_time_ms),
        }
    }

    #[must_use]
    pub fn with_context_extractor(mut self, extractor: PlacementContextExtractor) -> Self {
        self.extractor = extractor;
        self
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }
}

#[async_trait]
impl ToolExecutorProvider for DynamicToolExecutorProvider {
    async fn provide(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<Arc<dyn ToolExecutor>>, ToolExecutorSelectionError> {
        let context = (self.extractor)(activation);
        if matches!(context.requirements.location, ExecutionLocation::LocalOnly) {
            return Ok(None);
        }

        let workers = self
            .workers
            .list()
            .await
            .map_err(|error| ToolExecutorSelectionError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|worker| worker.snapshot)
            .collect::<Vec<_>>();
        let selected = match self.policy.select(&context, &workers, (self.clock)()) {
            Ok(selected) => selected,
            Err(PlacementError::NoEligibleWorker)
                if matches!(
                    context.requirements.location,
                    ExecutionLocation::RemotePreferred
                ) =>
            {
                return Ok(None);
            }
            Err(PlacementError::NoEligibleWorker) => {
                return Err(ToolExecutorSelectionError::Unavailable(
                    "no eligible remote worker".to_string(),
                ));
            }
            Err(error) => {
                return Err(ToolExecutorSelectionError::Policy(error.to_string()));
            }
        };

        self.executors
            .resolve(&selected.identity)
            .map(Some)
            .ok_or_else(|| {
                ToolExecutorSelectionError::Unavailable(format!(
                    "selected worker channel is absent: {}:{}:{}",
                    selected.identity.worker_id,
                    selected.identity.generation,
                    selected.identity.incarnation_id
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_contract::tool::{ToolError, ToolOutput};
    use awaken_worker_registry::{
        LeastLoadedPolicy, MemoryWorkerDirectory, PlacementRequirements, RegistryMutation,
        WorkerHeartbeat, WorkerManifest, WorkerRegistration,
    };

    struct TaggedExecutor(&'static str);

    #[async_trait]
    impl ToolExecutor for TaggedExecutor {
        async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id.clone(), self.0))
        }
    }

    struct MostLoadedPolicy;

    impl PlacementPolicy for MostLoadedPolicy {
        fn id(&self) -> &str {
            "test-most-loaded"
        }

        fn rank(
            &self,
            _context: &PlacementContext,
            eligible: &[WorkerSnapshot],
        ) -> Result<Vec<RankedWorker>, PlacementError> {
            let mut eligible = eligible.to_vec();
            eligible.sort_by_key(|worker| std::cmp::Reverse(worker.in_flight));
            Ok(eligible
                .into_iter()
                .map(|worker| RankedWorker {
                    identity: worker.identity,
                    score: i64::from(worker.in_flight),
                    reason: "test preference".to_string(),
                })
                .collect())
        }
    }

    fn activation() -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("t".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("p", "m", "echo"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            delegation_origin: None,
            model_ref_override: None,
        }
    }

    async fn ready_worker(
        directory: &MemoryWorkerDirectory,
        id: &str,
        incarnation: &str,
        in_flight: u32,
    ) -> WorkerIdentity {
        let mut manifest = WorkerManifest::default();
        manifest.capacity.max_concurrent = 100;
        let registered = directory
            .register(
                WorkerRegistration {
                    worker_id: id.to_string(),
                    incarnation_id: incarnation.to_string(),
                    manifest,
                },
                10,
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(
            directory
                .heartbeat(
                    &registered.snapshot.identity,
                    WorkerHeartbeat {
                        sequence: 1,
                        ready: true,
                        in_flight,
                    },
                    11,
                    1_000,
                )
                .await
                .unwrap(),
            RegistryMutation::Applied
        );
        registered.snapshot.identity
    }

    async fn output(executor: Arc<dyn ToolExecutor>) -> String {
        executor
            .invoke(&ToolCall {
                call_id: "c".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap()
            .content
    }

    #[tokio::test]
    async fn hot_replacement_changes_future_placement_not_an_existing_binding() {
        let workers = Arc::new(MemoryWorkerDirectory::new());
        let w1 = ready_worker(&workers, "w1", "i1", 1).await;
        let w2 = ready_worker(&workers, "w2", "i2", 9).await;
        let executors = WorkerExecutorDirectory::new();
        executors.register(w1, Arc::new(TaggedExecutor("least")));
        executors.register(w2, Arc::new(TaggedExecutor("most")));
        let policy = ReplaceablePlacementPolicy::new(Arc::new(LeastLoadedPolicy));
        let provider =
            DynamicToolExecutorProvider::new(workers, executors, policy.clone()).with_clock(|| 12);

        let pinned = provider.provide(&activation()).await.unwrap().unwrap();
        assert_eq!(output(pinned.clone()).await, "least");
        let old = policy.replace(Arc::new(MostLoadedPolicy));
        assert_eq!(old.id(), "least-loaded");
        assert_eq!(policy.active_id(), "test-most-loaded");
        assert_eq!(output(pinned).await, "least", "existing binding changed");
        assert_eq!(
            output(provider.provide(&activation()).await.unwrap().unwrap()).await,
            "most"
        );
    }

    #[tokio::test]
    async fn required_remote_placement_fails_closed_but_preferred_may_fall_back() {
        let workers = Arc::new(MemoryWorkerDirectory::new());
        let executors = WorkerExecutorDirectory::new();
        let policy = ReplaceablePlacementPolicy::new(Arc::new(LeastLoadedPolicy));
        let required: PlacementContextExtractor = Arc::new(|activation| {
            let mut context = default_placement_context(activation);
            context.requirements.location = ExecutionLocation::RemoteRequired;
            context
        });
        let provider =
            DynamicToolExecutorProvider::new(workers.clone(), executors.clone(), policy.clone())
                .with_context_extractor(required)
                .with_clock(|| 12);
        assert!(matches!(
            provider.provide(&activation()).await,
            Err(ToolExecutorSelectionError::Unavailable(_))
        ));

        let preferred =
            DynamicToolExecutorProvider::new(workers, executors, policy).with_clock(|| 12);
        assert!(preferred.provide(&activation()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_incarnation_channel_cannot_serve_current_worker() {
        let workers = Arc::new(MemoryWorkerDirectory::new());
        let current = ready_worker(&workers, "w", "current", 0).await;
        let stale = WorkerIdentity::new("w", "stale", current.generation.saturating_sub(1));
        let executors = WorkerExecutorDirectory::new();
        executors.register(stale, Arc::new(TaggedExecutor("stale")));
        let provider = DynamicToolExecutorProvider::new(
            workers,
            executors,
            ReplaceablePlacementPolicy::new(Arc::new(LeastLoadedPolicy)),
        )
        .with_clock(|| 12);
        assert!(matches!(
            provider.provide(&activation()).await,
            Err(ToolExecutorSelectionError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn local_only_never_consults_or_resolves_a_remote_worker() {
        let context: PlacementContextExtractor = Arc::new(|activation| {
            let mut context = default_placement_context(activation);
            context.requirements = PlacementRequirements {
                location: ExecutionLocation::LocalOnly,
                ..Default::default()
            };
            context
        });
        let provider = DynamicToolExecutorProvider::new(
            Arc::new(MemoryWorkerDirectory::new()),
            WorkerExecutorDirectory::new(),
            ReplaceablePlacementPolicy::new(Arc::new(LeastLoadedPolicy)),
        )
        .with_context_extractor(context);
        assert!(provider.provide(&activation()).await.unwrap().is_none());
    }
}
