use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime_contract::capability::RuntimeCapabilitySource;
use awaken_runtime_contract::catalog::{
    InstalledCatalog, RuntimeCatalogInstall, RuntimeCatalogInstaller,
};
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::llm::LlmExecutor;
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
    /// Installed plugin factories; the active subset for a run is chosen by the
    /// resolved spec's `plugin_ids` and merged under capability bounds (G30).
    plugins: Vec<Arc<dyn Plugin>>,
    /// Cancellation tokens for in-flight runs, so live control can steer them.
    active_runs: Mutex<HashMap<RunId, CancellationToken>>,
    /// How many extra attempts to make on a transient inference failure.
    infer_retries: usize,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            infer_retries: 2,
            ..Self::default()
        }
    }

    /// Set how many extra attempts to make on a transient inference failure.
    #[must_use]
    pub fn with_infer_retries(mut self, retries: usize) -> Self {
        self.infer_retries = retries;
        self
    }

    pub(crate) fn infer_retries(&self) -> usize {
        self.infer_retries
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
        plugin_ids: &[String],
    ) -> std::result::Result<ResolvedExecutionEnv, MergeError> {
        let active: Vec<_> = self
            .plugins
            .iter()
            .map(|plugin| plugin.manifest())
            .zip(self.plugins.iter())
            .filter(|(manifest, _)| plugin_ids.contains(&manifest.id))
            .map(|(manifest, plugin)| (manifest, plugin.resolve()))
            .collect();
        ResolvedExecutionEnv::merge(active)
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
    ) -> Result<
        awaken_runtime_contract::execution::RunOutcome,
        awaken_runtime_contract::execution::Error,
    > {
        crate::engine::resume_run(self, command, reader, context).await
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
        if install.fingerprint.0.trim().is_empty() {
            return Err(awaken_runtime_contract::catalog::Error::Rejected(
                "catalog fingerprint is empty".to_string(),
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
        let run_id = match &command {
            LiveCommand::Cancel { run_id } | LiveCommand::Wake { run_id, .. } => run_id.clone(),
        };
        let active = self
            .active_runs
            .lock()
            .map_err(|_| ControlError::Rejected("active run registry poisoned".to_string()))?;
        let token = active.get(&run_id).ok_or(ControlError::NotActive)?;
        match command {
            // Cancellation is cooperative: signal the token; the loop observes it
            // at the next step boundary and commits a terminal Cancelled outcome.
            LiveCommand::Cancel { .. } => {
                token.cancel();
                Ok(())
            }
            // Wake is a live nudge for an in-flight run. Durable resume of a
            // parked run goes through `Runtime::resume` with a validated
            // `ResumeCommand`, not this live channel, so wake stays a no-op.
            LiveCommand::Wake { .. } => Ok(()),
        }
    }
}
