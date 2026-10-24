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
        admin_tools: &[Arc<dyn RawTool>],
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
        admin_tools: &[Arc<dyn RawTool>],
        clock: Arc<dyn awaken_time::WallClock>,
        shutdown: tokio::sync::watch::Receiver<Option<WorkerShutdown>>,
    ) -> Result<Result<(), String>, String>;
}

/// What a co-located execution installs from, selected by the process
/// startup before the Session executor is built.
pub struct ColocatedExecutionInputs {
    /// The deployment whose Sandbox tier, Hand residency and ACP routes are
    /// installed.
    pub deployment: awaken_deployment_config::DeploymentConfig,
    /// The exact Namespace Hand companion a test-only composition built;
    /// production resolves the installed sibling executable.
    pub namespace_hand_path: Option<std::path::PathBuf>,
    /// The claim-fenced credential materializer ACP launches resolve through.
    pub credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    /// The Resources Memory source the mount data plane projects.
    pub memory_source: awaken_resource_contract::MemoryMountSource,
    /// The Resources invalidation bus the mount data plane subscribes to.
    pub memory_invalidator: Arc<awaken_resource_contract::LocalInvalidator>,
}

/// The Session execution of a process whose Sessions run on its own local
/// claim pool (`LocalWorker` placement): the Session environment, the Hand
/// executor factory, ACP routes, the Memory mount data plane and auxiliary
/// sandboxes. This startup links none of them; the composition that
/// co-locates execution supplies this role and a Coordinator process, whose
/// Sessions run on registered Workers, supplies none. Consumer: the process
/// startup, which calls it once while building its Session executor.
#[async_trait::async_trait]
pub trait ColocatedExecution: Send + Sync {
    /// Install the co-located execution into `host`.
    async fn install(
        &self,
        host: awaken_session_executor::SessionExecutorBuilder,
        inputs: ColocatedExecutionInputs,
    ) -> Result<awaken_session_executor::SessionExecutorBuilder, String>;
}

/// A co-located execution that installs only the Workdir Session environment
/// under the deployment's sandbox root, for the startup's own route,
/// registration and management tests that build a local-placement process
/// and admit Sessions without running a Hand in them.
#[cfg(test)]
pub(crate) struct WorkdirExecution;

#[cfg(test)]
#[async_trait::async_trait]
impl ColocatedExecution for WorkdirExecution {
    async fn install(
        &self,
        host: awaken_session_executor::SessionExecutorBuilder,
        inputs: ColocatedExecutionInputs,
    ) -> Result<awaken_session_executor::SessionExecutorBuilder, String> {
        Ok(awaken_execution_composition::with_workdir_fallback(
            host,
            &inputs.deployment,
        ))
    }
}
