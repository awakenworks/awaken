//! Dynamic hand placement (the ADR-0046 successor to the static
//! [`ConfigToolExecutorProvider`](crate::placement)).
//!
//! Placement is a scheduling decision over two attribute sets, decided in code —
//! never a config table:
//!
//! - **agent attributes** — what a run needs (its identity, required capabilities,
//!   an optional affinity key), read off the run's activation. The Tool being
//!   invoked is NOT an input: a tool call is placement-agnostic, so the `Tool` layer
//!   never learns where it runs.
//! - **worker attributes** — what a hand offers (capability tags, zone, health, live
//!   load), reported by the workers themselves into a [`WorkerRegistry`] via a
//!   heartbeat.
//!
//! A [`Placement`] policy matches the two (a predicate: healthy + capabilities +
//! affinity; then a priority: least-loaded, affinity-preferred) and yields a worker.
//! [`DynamicToolExecutorProvider`] resolves that worker to its `ToolExecutor` channel
//! — the seam demoted to a pure binding→channel resolver, with the decision upstream.
//! A run no worker matches returns `None`, so the kernel's in-process
//! `LocalToolExecutor` runs its tools (the same fail-safe as the static provider).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::tool::{ToolExecutor, ToolExecutorProvider};

/// What a run needs, for placement. Extracted from the activation by an
/// [`AgentAttrsExtractor`]; the Tool being invoked is deliberately absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentAttrs {
    /// The run's root agent identity.
    pub agent_id: String,
    /// Capability tags the placed worker MUST offer (a subset check). Empty means
    /// "no special requirement" — any healthy worker qualifies.
    pub required_capabilities: BTreeSet<String>,
    /// An optional affinity key (e.g. a tenant/zone/session pin). When set, only a
    /// worker whose `zone` matches is eligible.
    pub affinity: Option<String>,
}

/// What a hand offers, for placement. Reported by the worker into the registry and
/// refreshed by its heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerAttrs {
    /// Stable worker/hand id.
    pub worker_id: String,
    /// Capability tags this worker can serve.
    pub capabilities: BTreeSet<String>,
    /// The worker's zone/locality, matched against an agent's `affinity`.
    pub zone: Option<String>,
    /// Whether the worker is currently healthy (heartbeat fresh, not draining).
    pub healthy: bool,
    /// Live in-flight run count — the least-loaded priority signal.
    pub in_flight: u32,
}

/// A registered worker: its attributes and the channel that reaches it.
#[derive(Clone)]
pub struct WorkerEntry {
    pub attrs: WorkerAttrs,
    pub executor: Arc<dyn ToolExecutor>,
}

impl std::fmt::Debug for WorkerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerEntry")
            .field("attrs", &self.attrs)
            .finish_non_exhaustive()
    }
}

/// Extracts [`AgentAttrs`] from a run's activation. Pluggable so a host with richer
/// capability metadata can supply its own; the default reads the root agent id and
/// leaves requirements empty (every healthy worker qualifies).
pub type AgentAttrsExtractor = Arc<dyn Fn(&RunActivation) -> AgentAttrs + Send + Sync>;

/// The default extractor: identity only, no capability/affinity requirement. A run
/// is placeable on any healthy worker.
pub fn default_agent_attrs(activation: &RunActivation) -> AgentAttrs {
    AgentAttrs {
        agent_id: activation.snapshot.root_agent_id.0.clone(),
        required_capabilities: BTreeSet::new(),
        affinity: None,
    }
}

/// The live roster of workers eligible for placement. Workers self-register and
/// heartbeat; a placement policy reads a snapshot of the current roster. Thread-safe.
#[derive(Default)]
pub struct WorkerRegistry {
    workers: Mutex<Vec<WorkerEntry>>,
}

impl WorkerRegistry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register (or replace) a worker and the channel that reaches it.
    pub fn register(&self, attrs: WorkerAttrs, executor: Arc<dyn ToolExecutor>) {
        let mut workers = self.workers.lock().expect("registry poisoned");
        workers.retain(|w| w.attrs.worker_id != attrs.worker_id);
        workers.push(WorkerEntry { attrs, executor });
    }

    /// Refresh a worker's liveness signals (health + load) from its heartbeat.
    /// A heartbeat for an unknown worker is ignored (it must `register` first).
    pub fn heartbeat(&self, worker_id: &str, healthy: bool, in_flight: u32) {
        let mut workers = self.workers.lock().expect("registry poisoned");
        if let Some(w) = workers.iter_mut().find(|w| w.attrs.worker_id == worker_id) {
            w.attrs.healthy = healthy;
            w.attrs.in_flight = in_flight;
        }
    }

    /// Remove a worker (deregistered / lease lapsed / drained away).
    pub fn deregister(&self, worker_id: &str) {
        self.workers
            .lock()
            .expect("registry poisoned")
            .retain(|w| w.attrs.worker_id != worker_id);
    }

    /// A snapshot of the current roster, for a placement decision.
    #[must_use]
    pub fn snapshot(&self) -> Vec<WorkerEntry> {
        self.workers.lock().expect("registry poisoned").clone()
    }
}

/// Decides which worker a run is placed on, from the agent's attributes and the live
/// worker roster. Pure and synchronous — the decision, in code. Returns the chosen
/// worker id, or `None` when no worker is eligible (→ in-process default).
pub trait Placement: Send + Sync {
    fn place(&self, agent: &AgentAttrs, workers: &[WorkerEntry]) -> Option<String>;
}

/// The default policy: a capability/affinity/health **predicate**, then a
/// least-loaded **priority** (fewest in-flight; ties broken by preferring a
/// zone-affinity match, then the lexically-smallest worker id for determinism).
#[derive(Debug, Default, Clone, Copy)]
pub struct LeastLoadedPlacement;

impl LeastLoadedPlacement {
    /// Whether `worker` satisfies `agent`'s hard requirements.
    fn eligible(agent: &AgentAttrs, worker: &WorkerAttrs) -> bool {
        worker.healthy
            // required capabilities ⊆ offered capabilities
            && agent
                .required_capabilities
                .iter()
                .all(|cap| worker.capabilities.contains(cap))
            // affinity, when set, pins to the matching zone
            && agent
                .affinity
                .as_ref()
                .is_none_or(|a| worker.zone.as_deref() == Some(a.as_str()))
    }
}

impl Placement for LeastLoadedPlacement {
    fn place(&self, agent: &AgentAttrs, workers: &[WorkerEntry]) -> Option<String> {
        workers
            .iter()
            .filter(|w| Self::eligible(agent, &w.attrs))
            .min_by(|a, b| {
                // Priority key: (in_flight asc, then worker_id asc for a stable tie-break).
                a.attrs
                    .in_flight
                    .cmp(&b.attrs.in_flight)
                    .then_with(|| a.attrs.worker_id.cmp(&b.attrs.worker_id))
            })
            .map(|w| w.attrs.worker_id.clone())
    }
}

/// The dynamic [`ToolExecutorProvider`]: on each run it extracts the agent's
/// attributes, asks the [`Placement`] policy to choose a worker from the live
/// registry, and resolves that worker to its `ToolExecutor` channel. The decision is
/// upstream (in `Placement`); this provider is the binding→channel resolver. The
/// Tool layer never learns any of it.
pub struct DynamicToolExecutorProvider {
    registry: Arc<WorkerRegistry>,
    placement: Arc<dyn Placement>,
    extractor: AgentAttrsExtractor,
}

impl DynamicToolExecutorProvider {
    /// Build a provider over `registry` using `placement`, with the default
    /// agent-attribute extractor (identity only).
    #[must_use]
    pub fn new(registry: Arc<WorkerRegistry>, placement: Arc<dyn Placement>) -> Self {
        Self {
            registry,
            placement,
            extractor: Arc::new(default_agent_attrs),
        }
    }

    /// Override how agent attributes are read from a run (e.g. a host that carries
    /// capability requirements on its snapshots).
    #[must_use]
    pub fn with_extractor(mut self, extractor: AgentAttrsExtractor) -> Self {
        self.extractor = extractor;
        self
    }
}

#[async_trait]
impl ToolExecutorProvider for DynamicToolExecutorProvider {
    async fn provide(&self, activation: &RunActivation) -> Option<Arc<dyn ToolExecutor>> {
        let agent = (self.extractor)(activation);
        let workers = self.registry.snapshot();
        let chosen = self.placement.place(&agent, &workers)?;
        // Resolve the placement decision to the worker's channel (binding→channel).
        workers
            .into_iter()
            .find(|w| w.attrs.worker_id == chosen)
            .map(|w| w.executor)
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

    /// A `ToolExecutor` tagged so a test can tell which placed worker was resolved.
    struct TaggedExecutor(&'static str);
    #[async_trait]
    impl ToolExecutor for TaggedExecutor {
        async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id.clone(), self.0))
        }
    }

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn worker(id: &str, capabilities: &[&str], zone: Option<&str>, in_flight: u32) -> WorkerAttrs {
        WorkerAttrs {
            worker_id: id.into(),
            capabilities: caps(capabilities),
            zone: zone.map(str::to_string),
            healthy: true,
            in_flight,
        }
    }

    fn entry(attrs: WorkerAttrs, tag: &'static str) -> WorkerEntry {
        WorkerEntry {
            attrs,
            executor: Arc::new(TaggedExecutor(tag)),
        }
    }

    fn activation_for(agent_id: &str) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("t".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                root_agent_id: AgentId(agent_id.into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
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
            trace: Default::default(),
            model_access: Default::default(),
        }
    }

    #[test]
    fn least_loaded_wins_among_eligible_workers() {
        let policy = LeastLoadedPlacement;
        let workers = vec![
            entry(worker("w-busy", &[], None, 9), "BUSY"),
            entry(worker("w-idle", &[], None, 1), "IDLE"),
        ];
        let agent = AgentAttrs {
            agent_id: "a".into(),
            ..Default::default()
        };
        assert_eq!(policy.place(&agent, &workers).as_deref(), Some("w-idle"));
    }

    #[test]
    fn a_required_capability_filters_out_workers_that_lack_it() {
        let policy = LeastLoadedPlacement;
        let workers = vec![
            entry(worker("w-cpu", &["cpu"], None, 0), "CPU"),
            entry(worker("w-gpu", &["cpu", "gpu"], None, 5), "GPU"),
        ];
        // The run needs "gpu": only w-gpu qualifies, even though it is busier.
        let agent = AgentAttrs {
            agent_id: "a".into(),
            required_capabilities: caps(&["gpu"]),
            affinity: None,
        };
        assert_eq!(policy.place(&agent, &workers).as_deref(), Some("w-gpu"));
    }

    #[test]
    fn affinity_pins_to_the_matching_zone_and_unhealthy_workers_are_skipped() {
        let policy = LeastLoadedPlacement;
        let mut eu = worker("w-eu", &[], Some("eu"), 0);
        let us = worker("w-us", &[], Some("us"), 0);
        let workers = vec![entry(eu.clone(), "EU"), entry(us, "US")];
        let pinned = AgentAttrs {
            agent_id: "a".into(),
            required_capabilities: BTreeSet::new(),
            affinity: Some("us".into()),
        };
        assert_eq!(policy.place(&pinned, &workers).as_deref(), Some("w-us"));

        // An unhealthy worker is never chosen even if otherwise eligible.
        eu.healthy = false;
        let eu_only = vec![entry(eu, "EU")];
        let any = AgentAttrs {
            agent_id: "a".into(),
            ..Default::default()
        };
        assert_eq!(policy.place(&any, &eu_only), None);
    }

    #[tokio::test]
    async fn provider_places_a_run_and_resolves_the_workers_channel() {
        let registry = WorkerRegistry::new();
        registry.register(
            worker("w1", &[], None, 3),
            Arc::new(TaggedExecutor("W1")),
        );
        registry.register(
            worker("w2", &[], None, 0),
            Arc::new(TaggedExecutor("W2")),
        );
        let provider =
            DynamicToolExecutorProvider::new(registry.clone(), Arc::new(LeastLoadedPlacement));

        // The least-loaded worker (w2) is chosen and its channel resolved.
        let placed = provider.provide(&activation_for("agent")).await.unwrap();
        let out = placed
            .invoke(&ToolCall {
                call_id: "c".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(out.content, "W2");

        // A heartbeat that flips the load makes the SAME run place elsewhere next
        // time — the decision is dynamic, read from the live registry per run.
        registry.heartbeat("w2", true, 50);
        registry.heartbeat("w1", true, 0);
        let placed = provider.provide(&activation_for("agent")).await.unwrap();
        let out = placed
            .invoke(&ToolCall {
                call_id: "c".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(out.content, "W1", "placement followed the live load");
    }

    #[tokio::test]
    async fn no_eligible_worker_returns_none_so_the_kernel_runs_in_process() {
        let registry = WorkerRegistry::new();
        // One worker, but the run requires a capability it lacks.
        registry.register(worker("w1", &["cpu"], None, 0), Arc::new(TaggedExecutor("W1")));
        let extractor: AgentAttrsExtractor = Arc::new(|_a: &RunActivation| AgentAttrs {
            agent_id: "a".into(),
            required_capabilities: caps(&["gpu"]),
            affinity: None,
        });
        let provider =
            DynamicToolExecutorProvider::new(registry, Arc::new(LeastLoadedPlacement))
                .with_extractor(extractor);
        assert!(
            provider.provide(&activation_for("agent")).await.is_none(),
            "no worker offers gpu → None → in-process LocalToolExecutor"
        );

        // An empty registry also yields None.
        let empty =
            DynamicToolExecutorProvider::new(WorkerRegistry::new(), Arc::new(LeastLoadedPlacement));
        assert!(empty.provide(&activation_for("agent")).await.is_none());
    }
}
