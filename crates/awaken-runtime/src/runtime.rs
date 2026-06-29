use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::capability::RuntimeCapabilitySource;
use awaken_runtime_contract::catalog::{
    InstalledCatalog, RuntimeCatalogInstall, RuntimeCatalogInstaller,
};
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::permission::ToolGateHook;
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
    /// Cancellation tokens for in-flight runs, so live control can steer them.
    active_runs: Mutex<HashMap<RunId, CancellationToken>>,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
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
            // Waiting/resume is not yet implemented; a wake on a live run is a
            // no-op rather than a hard failure.
            LiveCommand::Wake { .. } => Ok(()),
        }
    }
}
