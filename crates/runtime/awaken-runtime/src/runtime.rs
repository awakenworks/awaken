use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Weak};

use awaken_agent_contract::agent::delegation::{ChildRunCancellation, DelegationId};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::delegation::{DelegationExecutionError, RunDelegationService};
use awaken_runtime_contract::live_inbox::LiveInbox;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::permission::ToolGateHook;
use awaken_runtime_contract::plugin::{
    MergeError, Plugin, PluginActivationDecision, PluginConfigError, ResolvedExecutionEnv,
    exact_plugin_selection,
};
use awaken_runtime_contract::snapshot::{ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry};
use awaken_runtime_contract::{AttemptOwnershipVerifier, RuntimeRunContext};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// The run-ending consecutive-failure ceiling, guaranteed `>= 1` by construction:
/// a 0 would mean "never terminal", which the loop must never allow. The clamp
/// lives here (once, at the type boundary) instead of at every read, so the stored
/// value is always valid and `Default` is a meaningful `1`.
#[derive(Debug, Clone, Copy)]
struct FailureCeiling(NonZeroUsize);

impl FailureCeiling {
    fn from_usize(n: usize) -> Self {
        Self(NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN))
    }
    fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for FailureCeiling {
    fn default() -> Self {
        Self(NonZeroUsize::MIN)
    }
}

/// Opaque identity for one process-local attempt-control registration.
///
/// The generation prevents a stale attempt's RAII guard from removing a newer
/// claim for the same Run after ownership has moved.
struct AttemptControlRegistration {
    run_id: RunId,
    generation: u64,
}

/// RAII ownership of one process-local attempt-control registration.
///
/// Every execution topology uses this same lifetime boundary, so cancellation,
/// pause, wake, and live-inbox handles disappear on every executor return path.
pub struct ActiveAttemptTracking<'a> {
    runtime: &'a Runtime,
    registration: AttemptControlRegistration,
}

impl Drop for ActiveAttemptTracking<'_> {
    fn drop(&mut self) {
        self.runtime
            .active_attempt_controls
            .lock()
            .deregister(&self.registration);
    }
}

struct ActiveAttemptControl {
    generation: u64,
    thread_id: ThreadId,
    cancellation: Option<CancellationToken>,
    pause: Option<PauseSignal>,
    live_inbox: Option<LiveInbox>,
    ownership: Option<Arc<dyn AttemptOwnershipVerifier>>,
}

impl ActiveAttemptControl {
    fn can_receive_wake(&self) -> bool {
        self.cancellation.is_some() || self.pause.is_some() || self.live_inbox.is_some()
    }
}

#[derive(Default)]
struct ActiveAttemptControls {
    next_generation: u64,
    by_run: HashMap<RunId, ActiveAttemptControl>,
}

impl ActiveAttemptControls {
    fn register(
        &mut self,
        run_id: &RunId,
        thread_id: &ThreadId,
        context: &RuntimeRunContext,
    ) -> AttemptControlRegistration {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("active-attempt registration generation exhausted");
        let generation = self.next_generation;
        self.by_run.insert(
            run_id.clone(),
            ActiveAttemptControl {
                generation,
                thread_id: thread_id.clone(),
                cancellation: context.cancellation.clone(),
                pause: context.pause.clone(),
                live_inbox: context.live_inbox.clone(),
                ownership: context.ownership.clone(),
            },
        );
        AttemptControlRegistration {
            run_id: run_id.clone(),
            generation,
        }
    }

    fn deregister(&mut self, registration: &AttemptControlRegistration) {
        let current = self
            .by_run
            .get(&registration.run_id)
            .is_some_and(|entry| entry.generation == registration.generation);
        if current {
            self.by_run.remove(&registration.run_id);
        }
    }
}

#[derive(Clone)]
struct ActiveAttemptSnapshot {
    run_id: RunId,
    generation: u64,
    live_inbox: Option<LiveInbox>,
    ownership: Option<Arc<dyn AttemptOwnershipVerifier>>,
}

type ThreadExecutionGates = Arc<Mutex<HashMap<ThreadId, Weak<tokio::sync::Mutex<()>>>>>;

struct ThreadExecutionGuard {
    thread_id: ThreadId,
    gates: ThreadExecutionGates,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for ThreadExecutionGuard {
    fn drop(&mut self) {
        // Release the owned gate before testing whether any waiter still holds
        // its Arc. Removing only the matching dead Weak cannot split live
        // callers onto two mutexes.
        self.guard.take();
        let mut gates = self.gates.lock();
        if gates
            .get(&self.thread_id)
            .is_some_and(|gate| gate.upgrade().is_none())
        {
            gates.remove(&self.thread_id);
        }
    }
}

/// The runtime core resolves snapshots and executes runs through injected ports.
/// The model provider, executable tools, and the
/// permission gate are all ports, so the core never names a model SDK or a
/// concrete tool id (G2).
#[derive(Default)]
pub struct Runtime {
    /// Executable snapshots resolvable by id. In a hosted deployment a config
    /// surface implements `AgentSnapshotResolver`; in-process this registry is
    /// the by-id path so inline and by-id inputs converge (G28).
    snapshots: Mutex<HashMap<ExecutableAgentSnapshotId, ExecutableAgentSnapshot>>,
    /// Model provider, built from the catalog at the composition root.
    llm: Option<Arc<dyn LlmExecutor>>,
    /// Executable tools keyed by id; concrete ids come from extensions.
    tools: RawToolRegistry,
    /// The authorization gate; absent means tools run ungated (test-only).
    gate: Option<Arc<dyn ToolGateHook>>,
    /// The delegation executor, if any. The engine routes the tool whose id is
    /// `run_delegation.tool_id()` to this interface instead of the tool registry, so a
    /// delegate call runs (or awaits) as a first-class kernel concern.
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    /// Process-local delivery receipts for durable child-cancellation intents.
    /// They suppress repeated network calls in one process; a restart clears
    /// them and therefore redelivers the still-durable outbox entry once.
    delivered_child_cancellations: Mutex<std::collections::HashSet<DelegationId>>,
    /// Installed plugin factories; the active subset for a run is chosen by the
    /// resolved spec's `plugin_ids` and merged under capability bounds (G30).
    plugins: Vec<Arc<dyn Plugin>>,
    /// One process-local registry for every capability of an executing attempt.
    /// It is observation/control only: the dispatch claim and Thread commit remain
    /// the durable authorities.
    active_attempt_controls: Mutex<ActiveAttemptControls>,
    /// The one process-local admission gate for every execution path.
    /// Durable workers additionally use the dispatch row's physical-attempt
    /// slot for cross-process recovery; direct callers have no durable claim.
    /// Sharing this gate means an accidentally duplicated delivery of the same
    /// exact claim still cannot cross an executor boundary concurrently inside
    /// one Runtime.
    ///
    /// The returned guard removes a dead Weak entry after the final caller, so
    /// completed Threads do not become a permanent registry. This gate is
    /// deliberately separate from
    /// `active_attempt_controls`: those entries remain observation/control and
    /// never become an alternate execution-authority store.
    thread_execution: ThreadExecutionGates,
    /// How retryable inference failures are retried (attempts and backoff).
    retry_policy: crate::retry::LlmRetryPolicy,
    /// How many continuation rounds a `MaxTokens`-truncated text step may use
    /// per step before the last partial is committed with an explicit terminal
    /// incomplete-output failure.
    max_continuation_retries: usize,
    /// The nth consecutive failed inference step ends the run. 1 (the default)
    /// means a single failure is terminal; a higher value absorbs failures at
    /// step granularity so a long-lived agent can ride out a provider outage.
    /// A `NonZeroUsize` by construction, so a derived-default 0 can never mean
    /// "never terminal".
    max_consecutive_inference_failures: FailureCeiling,
    /// Per-model circuit breaker shared by every run on this runtime.
    circuit_breaker: crate::circuit_breaker::CircuitBreaker,
    /// Structure-only metrics sink for the model/tool chokepoints (#2). Absent
    /// means the no-op recorder; a host injects an OpenTelemetry-backed one.
    metrics: Option<Arc<dyn awaken_runtime_contract::metrics::MetricsRecorder>>,
}

/// The process-wide no-op recorder handed out when none is injected, so the
/// accessor can return a `&dyn` without allocating or storing one per runtime.
static NOOP_METRICS: awaken_runtime_contract::metrics::NoopRecorder =
    awaken_runtime_contract::metrics::NoopRecorder;

impl Runtime {
    pub fn new() -> Self {
        Self {
            max_continuation_retries: 2,
            // `FailureCeiling::default()` is already 1.
            ..Self::default()
        }
    }

    /// Acquire the sole process-local execution slot for `thread_id`.
    ///
    /// The returned owned guard is held across the complete injected executor
    /// future, including model, tool, and Sandbox calls. Durable ingress also
    /// uses its persisted claim-bound slot; neither authority replaces the
    /// other because their failure domains differ.
    pub async fn acquire_thread_execution(&self, thread_id: &ThreadId) -> impl Drop + use<> {
        let gate = {
            let mut gates = self.thread_execution.lock();
            match gates.get(thread_id).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(tokio::sync::Mutex::new(()));
                    gates.insert(thread_id.clone(), Arc::downgrade(&gate));
                    gate
                }
            }
        };
        let guard = gate.lock_owned().await;
        ThreadExecutionGuard {
            thread_id: thread_id.clone(),
            gates: self.thread_execution.clone(),
            guard: Some(guard),
        }
    }

    /// Set how many extra attempts to make on a retryable inference failure.
    #[must_use]
    pub fn with_infer_retries(mut self, retries: usize) -> Self {
        self.retry_policy.max_retries = retries;
        self
    }

    /// Replace the whole inference retry policy (attempts and backoff pacing).
    #[must_use]
    pub fn with_retry_policy(mut self, policy: crate::retry::LlmRetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Set the per-step budget for continuing a `MaxTokens`-truncated step.
    #[must_use]
    pub fn with_max_continuation_retries(mut self, retries: usize) -> Self {
        self.max_continuation_retries = retries;
        self
    }

    /// Set how many consecutive failed inference steps end the run. The
    /// default 1 makes a single (post-retry) failure terminal; `n` lets the
    /// loop absorb `n - 1` consecutive failures, retrying at step granularity,
    /// and end with the nth failure's classification. A success resets the
    /// count.
    #[must_use]
    pub fn with_max_consecutive_inference_failures(mut self, max: usize) -> Self {
        // Clamp once, here at the type boundary: a 0 would mean "never terminal".
        self.max_consecutive_inference_failures = FailureCeiling::from_usize(max);
        self
    }

    /// Replace the per-model circuit breaker's tuning (threshold, cooldown,
    /// half-open probes).
    #[must_use]
    pub fn with_circuit_breaker(
        mut self,
        config: crate::circuit_breaker::CircuitBreakerConfig,
    ) -> Self {
        self.circuit_breaker = crate::circuit_breaker::CircuitBreaker::new(config);
        self
    }

    pub(crate) fn retry_policy(&self) -> &crate::retry::LlmRetryPolicy {
        &self.retry_policy
    }

    pub(crate) fn max_continuation_retries(&self) -> usize {
        self.max_continuation_retries
    }

    pub(crate) fn max_consecutive_inference_failures(&self) -> usize {
        // Valid by construction (>= 1): the ceiling owns its own invariant.
        self.max_consecutive_inference_failures.get()
    }

    pub(crate) fn circuit_breaker(&self) -> &crate::circuit_breaker::CircuitBreaker {
        &self.circuit_breaker
    }

    pub(crate) async fn deliver_child_cancellation(
        &self,
        cancellation: ChildRunCancellation,
    ) -> Result<bool, DelegationExecutionError> {
        if self
            .delivered_child_cancellations
            .lock()
            .contains(&cancellation.delegation_id)
        {
            return Ok(false);
        }
        let service = self.run_delegation().ok_or_else(|| {
            DelegationExecutionError::new("delegation cancellation has no configured service")
        })?;
        service.cancel(cancellation.clone()).await?;
        self.delivered_child_cancellations
            .lock()
            .insert(cancellation.delegation_id);
        Ok(true)
    }

    /// Inject the structure-only metrics recorder consulted at the model/tool
    /// chokepoints (composition-root wiring). The default is a no-op.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<dyn awaken_runtime_contract::metrics::MetricsRecorder>,
    ) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The injected structure-only metrics recorder (or the no-op default). Public
    /// so a durable dispatch worker holding this `Runtime` records its own
    /// `awaken.dispatch.*` counters onto the SAME recorder that meters model/tool
    /// calls — one sink, one OTLP pipeline, no extra wiring.
    pub fn metrics(&self) -> &dyn awaken_runtime_contract::metrics::MetricsRecorder {
        match &self.metrics {
            Some(m) => m.as_ref(),
            None => &NOOP_METRICS,
        }
    }

    /// Inject the model provider used by execution (composition root wiring).
    #[must_use]
    pub fn with_llm(mut self, llm: Arc<dyn LlmExecutor>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Register one executable tool, keyed by its id.
    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn RawTool>) -> Self {
        self.tools.insert(tool);
        self
    }

    /// Inject the permission gate consulted before every tool call.
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn ToolGateHook>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Inject Run delegation. The tool it backs (`service.tool_id()`) is
    /// executed by running a sub-agent (native or remote), not the tool registry.
    #[must_use]
    pub fn with_run_delegation(mut self, service: Arc<dyn RunDelegationService>) -> Self {
        self.run_delegation = Some(service);
        self
    }

    /// Install a plugin factory. It only contributes to a run whose resolved
    /// spec selects its id in `plugin_ids`.
    #[must_use]
    pub fn with_plugin(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Resolve the active plugins for a run into one merged execution
    /// environment, enforcing capability bounds and ordering (G30). Plugins not
    /// listed in `plugin_ids` are inert.
    pub(crate) fn resolve_plugin_env(
        &self,
        spec: &awaken_runtime_contract::resolved::ResolvedSpec,
    ) -> std::result::Result<ResolvedExecutionEnv, MergeError> {
        self.resolve_plugin_env_with(spec, &[])
    }

    /// Resolve authored plugins plus live plugins supplied by the realized
    /// Session. Session plugins are active by construction, but otherwise pass
    /// through the exact same configuration, bound, duplicate-id, dependency,
    /// and ordering checks as authored plugins.
    pub(crate) fn resolve_plugin_env_with(
        &self,
        spec: &awaken_runtime_contract::resolved::ResolvedSpec,
        session_plugins: &[Arc<dyn Plugin>],
    ) -> std::result::Result<ResolvedExecutionEnv, MergeError> {
        // Inline activations do not necessarily pass through snapshot-file
        // preflight.  Enforce exact presence and uniqueness here, on the actual
        // execution path, so an unknown or repeated selected id cannot become an
        // inert, silently ignored authority request.
        for selected in &spec.plugin_ids {
            if exact_plugin_selection(&spec.plugin_ids, selected)
                == PluginActivationDecision::RejectDuplicate
            {
                return Err(MergeError::DuplicatePlugin {
                    id: selected.clone(),
                });
            }
            let installed = self
                .plugins
                .iter()
                .chain(session_plugins.iter())
                .any(|plugin| plugin.manifest().id == *selected);
            if !installed {
                return Err(PluginConfigError::new(
                    selected,
                    "selected plugin is not installed in this Runtime",
                )
                .into());
            }
        }

        // Each active plugin resolves against its own config section (by manifest
        // id); a malformed section fails the run closed (G30).
        let mut active = Vec::new();
        for plugin in &self.plugins {
            let manifest = plugin.manifest();
            match exact_plugin_selection(&spec.plugin_ids, &manifest.id) {
                PluginActivationDecision::Inactive => continue,
                PluginActivationDecision::Active => {}
                PluginActivationDecision::RejectDuplicate => {
                    return Err(MergeError::DuplicatePlugin { id: manifest.id });
                }
                PluginActivationDecision::RejectMissingDependency
                | PluginActivationDecision::RejectCapability => {
                    unreachable!("selection checks only identity presence and uniqueness")
                }
            }
            let contributions = plugin.resolve_configured(spec.plugin_config.get(&manifest.id))?;
            active.push((manifest, contributions));
        }
        for plugin in session_plugins {
            let manifest = plugin.manifest();
            let contributions = plugin.resolve_configured(spec.plugin_config.get(&manifest.id))?;
            active.push((manifest, contributions));
        }
        ResolvedExecutionEnv::merge(active)
    }

    /// Dry-run plugin resolution for a candidate spec — the same resolve the loop
    /// runs, exposed so a config publisher can validate an agent's plugin config
    /// before publish and fail closed on a malformed section (validator = applier).
    pub fn validate_plugins(
        &self,
        spec: &awaken_runtime_contract::resolved::ResolvedSpec,
    ) -> std::result::Result<(), MergeError> {
        self.resolve_plugin_env(spec).map(|_| ())
    }

    /// The combined live version of the active plugins, or `None` if every
    /// active plugin is static. A change signals the drive loop to re-resolve the
    /// execution environment at the next step boundary (dynamic tool refresh).
    pub(crate) fn active_live_version_with(
        &self,
        plugin_ids: &[String],
        session_plugins: &[Arc<dyn Plugin>],
    ) -> Option<u64> {
        let mut acc: Option<u64> = None;
        for plugin in &self.plugins {
            if plugin_ids.contains(&plugin.manifest().id)
                && let Some(version) = plugin.live_version()
            {
                acc = Some(acc.unwrap_or(0).wrapping_add(version));
            }
        }
        for plugin in session_plugins {
            if let Some(version) = plugin.live_version() {
                acc = Some(acc.unwrap_or(0).wrapping_add(version));
            }
        }
        acc
    }

    pub(crate) fn llm(&self) -> Option<&Arc<dyn LlmExecutor>> {
        self.llm.as_ref()
    }

    pub(crate) fn tool(&self, id: &str) -> Option<&Arc<dyn RawTool>> {
        self.tools.get(id)
    }

    pub(crate) fn gate(&self) -> Option<&Arc<dyn ToolGateHook>> {
        self.gate.as_ref()
    }

    pub(crate) fn run_delegation(&self) -> Option<&Arc<dyn RunDelegationService>> {
        self.run_delegation.as_ref()
    }

    /// Register every neutral live handle for one executing attempt.
    ///
    /// Native execution and the durable Worker share this boundary, so cancel,
    /// pause, wake and live-inbox discovery cannot drift into separate registries.
    /// The returned generation must be supplied to deregistration; an older claim
    /// is then unable to erase a replacement claim's handles for the same Run.
    #[must_use]
    pub fn track_active_attempt(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
        context: &RuntimeRunContext,
    ) -> ActiveAttemptTracking<'_> {
        let registration = self
            .active_attempt_controls
            .lock()
            .register(run_id, thread_id, context);
        ActiveAttemptTracking {
            runtime: self,
            registration,
        }
    }

    fn active_attempt_snapshot(&self, run_id: &RunId) -> Option<ActiveAttemptSnapshot> {
        self.active_attempt_controls
            .lock()
            .by_run
            .get(run_id)
            .map(|entry| ActiveAttemptSnapshot {
                run_id: run_id.clone(),
                generation: entry.generation,
                live_inbox: entry.live_inbox.clone(),
                ownership: entry.ownership.clone(),
            })
    }

    async fn ownership_is_current(snapshot: &ActiveAttemptSnapshot) -> bool {
        match &snapshot.ownership {
            Some(ownership) => ownership.verify_current().await.is_ok(),
            None => true,
        }
    }

    fn registration_is_current(&self, snapshot: &ActiveAttemptSnapshot) -> bool {
        self.active_attempt_controls
            .lock()
            .by_run
            .get(&snapshot.run_id)
            .is_some_and(|entry| entry.generation == snapshot.generation)
    }

    async fn active_attempt_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Option<ActiveAttemptSnapshot> {
        let candidates = self
            .active_attempt_controls
            .lock()
            .by_run
            .iter()
            .filter(|(_, entry)| entry.thread_id == *thread_id)
            .map(|(run_id, entry)| ActiveAttemptSnapshot {
                run_id: run_id.clone(),
                generation: entry.generation,
                live_inbox: entry.live_inbox.clone(),
                ownership: entry.ownership.clone(),
            })
            .collect::<Vec<_>>();

        let mut current = Vec::new();
        for candidate in candidates {
            if Self::ownership_is_current(&candidate).await
                && self.registration_is_current(&candidate)
            {
                current.push(candidate);
            }
        }
        if current.len() != 1 {
            return None;
        }
        let candidate = current.pop().expect("one current candidate");
        self.registration_is_current(&candidate)
            .then_some(candidate)
    }

    /// Resolve the single locally executing, currently owned Run for a Thread.
    /// An idle Thread, a remote attempt, an ambiguous overlap, or a stale claim
    /// all fail closed to `None`.
    pub async fn active_attempt_run_id(&self, thread_id: &ThreadId) -> Option<RunId> {
        self.active_attempt_for_thread(thread_id)
            .await
            .map(|attempt| attempt.run_id)
    }

    /// Resolve the single locally executing, currently owned attempt inbox for a
    /// Thread. Callers fall back to durable Session events when the exact local
    /// attempt has no inbox.
    pub async fn active_attempt_live_inbox(&self, thread_id: &ThreadId) -> Option<LiveInbox> {
        self.active_attempt_for_thread(thread_id)
            .await
            .and_then(|attempt| attempt.live_inbox)
    }

    /// Deliver only when the registered attempt still owns its dispatch claim.
    /// Direct attempts have no claim verifier and are current while registered.
    pub async fn deliver_to_current_attempt(
        &self,
        command: LiveCommand,
    ) -> Result<(), ControlError> {
        let run_id = match &command {
            LiveCommand::Cancel { run_id }
            | LiveCommand::Pause { run_id }
            | LiveCommand::Wake { run_id, .. } => run_id,
        };
        let snapshot = self
            .active_attempt_snapshot(run_id)
            .ok_or(ControlError::NotActive)?;
        if !Self::ownership_is_current(&snapshot).await {
            return Err(ControlError::NotActive);
        }
        let controls = self.active_attempt_controls.lock();
        let entry = controls
            .by_run
            .get(&snapshot.run_id)
            .filter(|entry| entry.generation == snapshot.generation)
            .ok_or(ControlError::NotActive)?;
        Self::deliver_registered(entry, command)
    }

    fn deliver_registered(
        attempt: &ActiveAttemptControl,
        command: LiveCommand,
    ) -> Result<(), ControlError> {
        match command {
            LiveCommand::Cancel { .. } => attempt
                .cancellation
                .as_ref()
                .ok_or(ControlError::NotActive)
                .map(CancellationToken::cancel),
            LiveCommand::Pause { .. } => attempt
                .pause
                .as_ref()
                .ok_or(ControlError::NotActive)
                .map(PauseSignal::request),
            LiveCommand::Wake { .. } if attempt.can_receive_wake() => {
                if let Some(inbox) = &attempt.live_inbox {
                    inbox.wake();
                }
                Ok(())
            }
            LiveCommand::Wake { .. } => Err(ControlError::NotActive),
        }
    }

    /// Register an executable snapshot for by-id resolution. Returns the id so
    /// callers can submit `AgentSnapshotInput::ById`.
    pub fn register_snapshot(
        &self,
        snapshot: ExecutableAgentSnapshot,
    ) -> ExecutableAgentSnapshotId {
        let id = snapshot.id.clone();
        self.snapshots.lock().insert(id.clone(), snapshot);
        id
    }

    /// Resume an awaiting run from a validated `ResumeCommand`. The reader supplies
    /// the committed transcript and the active awaiting ticket; the resume fails
    /// closed unless every identity in the ticket matches (G5/G28).
    pub async fn resume(
        &self,
        command: awaken_runtime_contract::resume::ResumeCommand,
        reader: &dyn CommittedThreadView,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<
        awaken_agent_contract::agent::run::RunState,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::run_commands::resume_run(self, command, reader, context).await
    }

    /// Commit the authoritative terminal `Cancelled` fact and clear any awaiting
    /// ticket. Durable ingress invokes this after claiming a cancellation intent;
    /// live control is only the best-effort signal that stops an old in-flight
    /// owner, whose epoch has already been fenced by the dispatch store.
    pub async fn cancel_run(
        &self,
        run_id: RunId,
        thread_id: awaken_agent_contract::agent::thread::Id,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<
        awaken_agent_contract::agent::run::RunState,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::run_commands::cancel_run(self, run_id, thread_id, context).await
    }

    /// Commit a claimed activation's accepted input and terminal cancellation
    /// together. Durable dispatch uses this when cancellation fences a fresh
    /// attempt before that attempt can publish its first commit.
    pub async fn cancel_activation(
        &self,
        activation: awaken_runtime_contract::activation::RunActivation,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<
        awaken_agent_contract::agent::run::RunState,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::run_commands::cancel_activation(self, activation, context).await
    }

    /// Resolve an Awaiting tool Run's authoritative ToolBatch with fixed
    /// interruption errors and end it without sampling the model.
    pub async fn interrupt_awaiting_tools(
        &self,
        run_id: RunId,
        thread_id: awaken_agent_contract::agent::thread::Id,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<
        awaken_agent_contract::agent::run::RunState,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::run_commands::interrupt_awaiting_tools(self, run_id, thread_id, context)
            .await
    }

    /// Retry every durable child-cancellation intent retained on `thread`.
    /// Hosts call this when rebuilding or re-entering a session, so a process
    /// crash between the parent's terminal commit and remote delivery cannot
    /// permanently orphan an awaiting child.
    pub async fn reconcile_delegation_cancellations(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        reader: &dyn CommittedThreadView,
    ) -> Result<usize, awaken_runtime_contract::execution::Error> {
        crate::engine::reconcile_delegation_cancellations(self, thread_id, reader).await
    }

    /// Stop a not-running run with a terminal `Stopped(reason)` fact — a host stop
    /// policy (budget, step ceiling) making the run terminal and clearing its
    /// awaiting ticket, so a later resume or scheduled result fails closed
    /// (ADR-0026).
    pub async fn stop_run(
        &self,
        run_id: RunId,
        thread_id: awaken_agent_contract::agent::thread::Id,
        reason: String,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<
        awaken_agent_contract::agent::run::RunState,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::run_commands::stop_run(self, run_id, thread_id, reason, context).await
    }

    /// Perform a committed `ScheduledAction` (ADR-0020): run the deferred action
    /// the awaiting Run committed and commit the resumed result. Fails closed if
    /// the Run is not awaiting on a `ScheduledAction` ticket.
    pub async fn perform_scheduled_action(
        &self,
        run_id: &RunId,
        reader: &dyn CommittedThreadView,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
        now_ms: u64,
    ) -> Result<
        awaken_agent_contract::agent::run::RunState,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::run_commands::perform_scheduled_action(self, run_id, reader, context, now_ms)
            .await
    }

    pub(crate) fn snapshot_by_id(
        &self,
        id: &ExecutableAgentSnapshotId,
    ) -> Option<ExecutableAgentSnapshot> {
        self.snapshots.lock().get(id).cloned()
    }

    pub(crate) fn snapshot_ids(&self) -> Vec<ExecutableAgentSnapshotId> {
        self.snapshots.lock().keys().cloned().collect()
    }
}

impl LiveRunControl for Runtime {
    fn deliver(&self, command: LiveCommand) -> Result<(), ControlError> {
        let run_id = match &command {
            LiveCommand::Cancel { run_id }
            | LiveCommand::Pause { run_id }
            | LiveCommand::Wake { run_id, .. } => run_id,
        };
        let controls = self.active_attempt_controls.lock();
        let attempt = controls.by_run.get(run_id).ok_or(ControlError::NotActive)?;
        Self::deliver_registered(attempt, command)
    }
}

#[cfg(test)]
mod thread_execution_tests {
    use super::*;

    #[tokio::test]
    async fn final_guard_removes_the_thread_gate_without_splitting_waiters() {
        // Decision rules: G1 first acquisition creates one entry; G2 a queued
        // waiter retains that same entry after the first guard drops; G3 the
        // final guard drops with no waiter and removes the key. Effects are
        // respectively size 1, size 1, and size 0—no leak and no parallel gate.
        let runtime = Arc::new(Runtime::new());
        let thread = ThreadId("thread-gate-lifecycle".into());
        let first = runtime.acquire_thread_execution(&thread).await;
        assert_eq!(runtime.thread_execution.lock().len(), 1, "G1");
        let waiter = tokio::spawn({
            let runtime = runtime.clone();
            let thread = thread.clone();
            async move { runtime.acquire_thread_execution(&thread).await }
        });
        tokio::task::yield_now().await;
        drop(first);
        let second = waiter.await.expect("G2 waiter joins");
        assert_eq!(runtime.thread_execution.lock().len(), 1, "G2");
        drop(second);
        assert!(runtime.thread_execution.lock().is_empty(), "G3");
    }
}
