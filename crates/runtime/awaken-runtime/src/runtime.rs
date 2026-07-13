use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime_contract::agent_resolver::AgentResolver;
use awaken_runtime_contract::capability::RuntimeCapabilitySource;
use awaken_runtime_contract::catalog::{
    InstalledCatalog, RuntimeCatalogInstall, RuntimeCatalogInstaller,
};
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::permission::ToolGateHook;
use awaken_runtime_contract::plugin::{MergeError, Plugin, ResolvedExecutionEnv};
use awaken_runtime_contract::resolved::CatalogFingerprint;
use awaken_runtime_contract::snapshot::{ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
use awaken_runtime_contract::tool::RawTool;
use tokio_util::sync::CancellationToken;

/// The runtime core. It installs catalogs, resolves snapshots, and executes
/// runs through injected ports. The model provider, executable tools, and the
/// permission gate are all ports, so the core never names a model SDK or a
/// concrete tool id (G2).
#[derive(Default)]
pub struct Runtime {
    active_catalog: Mutex<Option<RuntimeCatalogInstall>>,
    /// Executable snapshots resolvable by id. In a hosted deployment a config
    /// surface implements `AgentSnapshotResolver`; in-process this registry is
    /// the by-id path so inline and by-id inputs converge (G28).
    snapshots: Mutex<HashMap<ExecutableAgentSnapshotId, ExecutableAgentSnapshot>>,
    /// Model provider, built from the catalog at the composition root.
    llm: Option<Arc<dyn LlmExecutor>>,
    /// Executable tools keyed by id; concrete ids come from extensions.
    tools: HashMap<String, Arc<dyn RawTool>>,
    /// The authorization gate; absent means tools run ungated (test-only).
    gate: Option<Arc<dyn ToolGateHook>>,
    /// The delegation resolver, if any. The engine routes the tool whose id is
    /// `resolver.tool_id()` to this port instead of the tool registry, so a
    /// delegate call runs (or parks) as a first-class kernel concern.
    resolver: Option<Arc<dyn AgentResolver>>,
    /// Installed plugin factories; the active subset for a run is chosen by the
    /// resolved spec's `plugin_ids` and merged under capability bounds (G30).
    plugins: Vec<Arc<dyn Plugin>>,
    /// Cancellation tokens for in-flight runs, so live control can steer them.
    active_runs: Mutex<HashMap<RunId, CancellationToken>>,
    /// Pause signals for in-flight runs, so live control can park them at the next
    /// safe boundary (ADR-0054). Mirrors `active_runs`, keyed the same way.
    active_pauses: Mutex<HashMap<RunId, PauseSignal>>,
    /// How retryable inference failures are retried (attempts and backoff).
    retry_policy: crate::retry::LlmRetryPolicy,
    /// How many continuation rounds a `MaxTokens`-truncated text turn may use
    /// per step before the partial output stands as the turn.
    max_continuation_retries: usize,
    /// The nth consecutive failed inference step ends the run. 1 (the default)
    /// means a single failure is terminal; a higher value absorbs failures at
    /// step granularity so a long-lived agent can ride out a provider outage.
    max_consecutive_inference_failures: usize,
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
            max_consecutive_inference_failures: 1,
            ..Self::default()
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

    /// Set the per-step budget for continuing a `MaxTokens`-truncated turn.
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
        self.max_consecutive_inference_failures = max;
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
        // A derived-default 0 must not mean "never terminal": clamp to 1.
        self.max_consecutive_inference_failures.max(1)
    }

    pub(crate) fn circuit_breaker(&self) -> &crate::circuit_breaker::CircuitBreaker {
        &self.circuit_breaker
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
        self.tools.insert(tool.id().to_string(), tool);
        self
    }

    /// Inject the permission gate consulted before every tool call.
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn ToolGateHook>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Inject the delegation resolver. The tool it backs (`resolver.tool_id()`) is
    /// executed by running a sub-agent (native or remote), not the tool registry.
    #[must_use]
    pub fn with_resolver(mut self, resolver: Arc<dyn AgentResolver>) -> Self {
        self.resolver = Some(resolver);
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
        // Each active plugin resolves against its own config section (by manifest
        // id); a malformed section fails the run closed (G30).
        let mut active = Vec::new();
        for plugin in &self.plugins {
            let manifest = plugin.manifest();
            if !spec.plugin_ids.contains(&manifest.id) {
                continue;
            }
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
    pub(crate) fn active_live_version(&self, plugin_ids: &[String]) -> Option<u64> {
        let mut acc: Option<u64> = None;
        for plugin in &self.plugins {
            if plugin_ids.contains(&plugin.manifest().id)
                && let Some(version) = plugin.live_version()
            {
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

    pub(crate) fn resolver(&self) -> Option<&Arc<dyn AgentResolver>> {
        self.resolver.as_ref()
    }

    /// Track an in-flight run's cancellation token so `LiveRunControl` can reach
    /// it. Called at the start of execution when the context carries a token.
    pub(crate) fn register_run(&self, run_id: &RunId, token: CancellationToken) {
        if let Ok(mut active) = self.active_runs.lock() {
            active.insert(run_id.clone(), token);
        }
    }

    /// Stop tracking a run once it reaches a terminal state.
    pub(crate) fn deregister_run(&self, run_id: &RunId) {
        if let Ok(mut active) = self.active_runs.lock() {
            active.remove(run_id);
        }
    }

    /// Track an in-flight run's pause signal so `LiveRunControl` can park it.
    /// Called at the start of execution when the context carries a signal.
    pub(crate) fn register_pause(&self, run_id: &RunId, pause: PauseSignal) {
        if let Ok(mut active) = self.active_pauses.lock() {
            active.insert(run_id.clone(), pause);
        }
    }

    /// Stop tracking a run's pause signal once it reaches a terminal state.
    pub(crate) fn deregister_pause(&self, run_id: &RunId) {
        if let Ok(mut active) = self.active_pauses.lock() {
            active.remove(run_id);
        }
    }

    /// Register an executable snapshot for by-id resolution. Returns the id so
    /// callers can submit `AgentSnapshotInput::ById`.
    pub fn register_snapshot(
        &self,
        snapshot: ExecutableAgentSnapshot,
    ) -> ExecutableAgentSnapshotId {
        let id = snapshot.id.clone();
        if let Ok(mut snapshots) = self.snapshots.lock() {
            snapshots.insert(id.clone(), snapshot);
        }
        id
    }

    pub(crate) fn active_fingerprint(&self) -> Option<CatalogFingerprint> {
        self.active_catalog
            .lock()
            .ok()
            .and_then(|catalog| catalog.as_ref().map(|install| install.fingerprint.clone()))
    }

    /// Resume a parked run from a validated `ResumeCommand`. The reader supplies
    /// the committed transcript and the active waiting ticket; the resume fails
    /// closed unless every identity in the ticket matches (G5/G28).
    pub async fn resume(
        &self,
        command: awaken_runtime_contract::resume::ResumeCommand,
        reader: &dyn ThreadReader,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<awaken_agent_contract::agent::run::Phase, awaken_runtime_contract::execution::Error>
    {
        crate::engine::resume_run(self, command, reader, context).await
    }

    /// Cancel a not-running run (queued or parked) by committing a terminal
    /// `Cancelled` fact, clearing any waiting ticket. An in-flight run is
    /// cancelled through `LiveRunControl` instead.
    pub async fn cancel_run(
        &self,
        run_id: RunId,
        thread_id: awaken_agent_contract::agent::thread::Id,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<awaken_agent_contract::agent::run::Phase, awaken_runtime_contract::execution::Error>
    {
        crate::engine::cancel_run(run_id, thread_id, context).await
    }

    /// Stop a not-running run with a terminal `Stopped(reason)` fact — a host stop
    /// policy (budget, step ceiling) making the run terminal and clearing its
    /// waiting ticket, so a later resume or scheduled result fails closed
    /// (ADR-0026).
    pub async fn stop_run(
        &self,
        run_id: RunId,
        thread_id: awaken_agent_contract::agent::thread::Id,
        reason: String,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<awaken_agent_contract::agent::run::Phase, awaken_runtime_contract::execution::Error>
    {
        crate::engine::stop_run(run_id, thread_id, reason, context).await
    }

    /// Perform a committed `ScheduledAction` (ADR-0020): run the deferred action
    /// the parked run committed and commit the resumed outcome. Fails closed if
    /// the run is not parked on a `ScheduledAction` ticket.
    pub async fn perform_scheduled_action(
        &self,
        run_id: &RunId,
        reader: &dyn ThreadReader,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
        now_ms: u64,
    ) -> Result<awaken_agent_contract::agent::run::Phase, awaken_runtime_contract::execution::Error>
    {
        crate::engine::perform_scheduled_action(self, run_id, reader, context, now_ms).await
    }

    pub(crate) fn snapshot_by_id(
        &self,
        id: &ExecutableAgentSnapshotId,
    ) -> Option<ExecutableAgentSnapshot> {
        self.snapshots
            .lock()
            .ok()
            .and_then(|snapshots| snapshots.get(id).cloned())
    }

    pub(crate) fn snapshot_ids(&self) -> Vec<ExecutableAgentSnapshotId> {
        self.snapshots
            .lock()
            .map(|snapshots| snapshots.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl RuntimeCatalogInstaller for Runtime {
    fn install_catalog(
        &self,
        install: RuntimeCatalogInstall,
    ) -> Result<InstalledCatalog, awaken_runtime_contract::catalog::Error> {
        // Fail-closed before any state change: validate the install is internally
        // consistent. An empty top-level fingerprint or a mismatch between the
        // install fingerprint and the capabilities catalog fingerprint means the
        // install was assembled incorrectly; reject before the atomic swap (G4/G23).
        if install.fingerprint.0.trim().is_empty() {
            return Err(awaken_runtime_contract::catalog::Error::Rejected(
                "catalog fingerprint is empty".to_string(),
            ));
        }
        if install.fingerprint != install.capabilities.catalog_fingerprint {
            return Err(awaken_runtime_contract::catalog::Error::Rejected(
                "fingerprint mismatch: install fingerprint does not match \
                 capabilities catalog fingerprint"
                    .to_string(),
            ));
        }

        let fingerprint = install.fingerprint.clone();
        let mut active_catalog = self.active_catalog.lock().map_err(|_| {
            awaken_runtime_contract::catalog::Error::Rejected(
                "active catalog lock is poisoned".to_string(),
            )
        })?;
        *active_catalog = Some(install);

        Ok(InstalledCatalog { fingerprint })
    }
}

impl RuntimeCapabilitySource for Runtime {
    fn runtime_capabilities(
        &self,
    ) -> awaken_runtime_contract::capability::RuntimeCapabilityCatalog {
        self.active_catalog
            .lock()
            .ok()
            .and_then(|catalog| catalog.as_ref().map(|install| install.capabilities.clone()))
            .unwrap_or_else(
                || awaken_runtime_contract::capability::RuntimeCapabilityCatalog {
                    catalog_fingerprint: CatalogFingerprint(String::new()),
                    runtime_version: env!("CARGO_PKG_VERSION").to_string(),
                    tools: Vec::new(),
                    plugins: Vec::new(),
                },
            )
    }
}

impl LiveRunControl for Runtime {
    fn deliver(&self, command: LiveCommand) -> Result<(), ControlError> {
        match command {
            // Cancellation is cooperative: signal the token; the loop observes it
            // at the next step boundary and commits a terminal Cancelled outcome.
            LiveCommand::Cancel { run_id } => {
                let active = self.active_runs.lock().map_err(|_| {
                    ControlError::Rejected("active run registry poisoned".to_string())
                })?;
                active.get(&run_id).ok_or(ControlError::NotActive)?.cancel();
                Ok(())
            }
            // Pause is cooperative: signal the pause; the loop observes it at the
            // next safe boundary and commits a durable `ManualPause` park (ADR-0054).
            LiveCommand::Pause { run_id } => {
                let active = self.active_pauses.lock().map_err(|_| {
                    ControlError::Rejected("active pause registry poisoned".to_string())
                })?;
                active
                    .get(&run_id)
                    .ok_or(ControlError::NotActive)?
                    .request();
                Ok(())
            }
            // Wake is a live nudge for an in-flight run: verify a live subscriber
            // (the run is registered active) accepts it, then it is a no-op — durable
            // resume of a PARKED run goes through `Runtime::resume` with a validated
            // `ResumeCommand`, not this live channel. Fail closed when no live run
            // accepts it (G5: a wake with no subscriber is a hard error, not a silent
            // success), so the durable live-control seam surfaces `NoSubscriber`
            // rather than reporting a phantom wake.
            LiveCommand::Wake { run_id, .. } => {
                let active = self.active_runs.lock().map_err(|_| {
                    ControlError::Rejected("active run registry poisoned".to_string())
                })?;
                if active.contains_key(&run_id) {
                    Ok(())
                } else {
                    Err(ControlError::NotActive)
                }
            }
        }
    }
}
