// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The port through which the AllInOne launcher supplies its co-located Worker to the
// @formal reason: shared service lifecycle; classified against the role-assembly requirement, not a proof claim.
//! What a process co-locates with its Coordinator: the Worker of an AllInOne
//! process, or the Session execution of a local claim pool.
//!
//! The process startup owns its supervision: readiness validation before the
//! process advertises itself, restart with backoff after a lost registry
//! authority, and the one shutdown order (the Worker drains before the
//! co-located HTTP surfaces stop). How the Worker is prepared and run belongs
//! to the launcher that links the Worker (`awaken-cli`); a Control or
//! Coordinator process supplies none, so this startup links no Worker.

use std::sync::Arc;

use awaken_tool_contract::tool::RawTool;
use awaken_worker_contract::WorkerShutdown;

use crate::config::ResolvedDeployment;

/// What the AllInOne process lends its co-located Worker at every incarnation:
/// capabilities the Coordinator process holds and the Worker composition
/// cannot obtain by itself. Consumer: the launcher's Worker build
/// (`awaken-cli` `PreparedLocalWorker`). They are in-process handles; none of
/// them crosses the Worker transport, so a remote Worker never receives them.
#[derive(Clone, Default)]
pub struct ColocatedWorkerInputs {
    /// The process's administrative tools.
    pub admin_tools: Vec<Arc<dyn RawTool>>,
    /// Whether this process supplies Awaken Cloud brokered models.
    pub cloud_models_enabled: bool,
    /// The process's Awaken Cloud brokered-inference client, authenticated by
    /// the process's Cloud identity (login session or Developer Key). Absent
    /// when Cloud models are disabled or no Cloud identity is configured.
    pub brokered_inference:
        Option<Arc<dyn awaken_inference_endpoint::brokered_inference::BrokeredInferenceClient>>,
}

/// Prepares the co-located Worker of an AllInOne deployment. Consumer: the
/// AllInOne service lifecycle, which calls it once, before serving, on the
/// process root task (the preparation future need not be `Send`: it opens the
/// Control credential stores it shares with the process).
#[async_trait::async_trait(?Send)]
pub trait ColocatedWorkerPreparation: Send + Sync {
    /// Prepare the Worker from the resolved deployment. Preparation may record
    /// the deployment's local ACP observations and disable the anonymous
    /// claim pool the registered Worker replaces; `workspace` is the
    /// installation's platform Workspace and `clock` the process clock.
    ///
    /// The deployment is taken and returned by value so the preparation future
    /// owns everything it awaits across.
    async fn prepare(
        &self,
        deployment: ResolvedDeployment,
        seal_key: [u8; 32],
        workspace: Option<String>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<(ResolvedDeployment, Arc<dyn ColocatedWorker>), String>;
}

/// One prepared co-located Worker. Consumer: the AllInOne service lifecycle,
/// which validates it before readiness and supervises its incarnations.
#[async_trait::async_trait]
pub trait ColocatedWorker: Send + Sync {
    /// Build one incarnation against `upstream` without running it, so a
    /// composition error stops startup before the process is advertised.
    async fn validate(
        &self,
        upstream: String,
        deployment: &ResolvedDeployment,
        inputs: &ColocatedWorkerInputs,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<(), String>;

    /// Build one incarnation and run it until `shutdown` selects a mode or it
    /// ends on its own (lost registry authority, failure). The outer error is a
    /// composition failure, which ends supervision; the inner result is how a
    /// built incarnation ended, after which the supervisor may restart it.
    async fn run_incarnation(
        &self,
        upstream: String,
        deployment: &ResolvedDeployment,
        inputs: &ColocatedWorkerInputs,
        clock: Arc<dyn awaken_time::WallClock>,
        shutdown: tokio::sync::watch::Receiver<Option<WorkerShutdown>>,
    ) -> Result<Result<(), String>, String>;
}
