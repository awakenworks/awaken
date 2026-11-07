// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The AllInOne launcher supplies the co-located Worker the shared service lifecycle
// @formal reason: supervises; classified against the role-assembly requirement, not a formal-proof claim.
//! `awaken-cli` library: the `awaken` launcher's AllInOne process, which
//! co-locates a Worker with its Coordinator.
//!
//! The process startup (`awaken-process-startup`) owns every service role and
//! links no Worker. This launcher supplies the one thing only it may link: the
//! co-located Worker an AllInOne process supervises, prepared from the same
//! deployment and its local ACP discovery ([`LocalWorkerPreparation`]), and the
//! Session execution of a process that runs its Sessions on its own local
//! claim pool ([`LocalExecution`]). The Control and Coordinator executables
//! never link either.
#![warn(unreachable_pub)]

mod a2a_attempt;
mod acp_local_credentials;
pub use a2a_attempt::a2a_attempt_executor;
mod coordinator_execution;
mod local_execution;
pub use coordinator_execution::{CoordinatorExecutionError, coordinator_execution};
mod local_model;
#[cfg(feature = "test-support")]
mod scenario_process;
#[cfg(feature = "test-support")]
pub use scenario_process::{build_all_in_one_service_with_host_customizer, colocated_test_worker};
mod memory_data_plane;

use std::sync::Arc;

pub use local_execution::LocalExecution;
pub use memory_data_plane::with_platform_memory_data_plane;

pub use acp_local_credentials::{
    PreparedLocalAcp, PreparedLocalWorker, local_acp_diagnostics, prepare_local_acp,
    prepare_local_worker,
};
use awaken_process_startup::config::ResolvedDeployment;
use awaken_process_startup::console::ServiceArgs;
use awaken_process_startup::{
    ColocatedWorker, ColocatedWorkerPreparation, CoordinatorServiceAdapters, ManagedServiceAdapters,
};
use awaken_tool_contract::tool::RawTool;
use awaken_worker_contract::WorkerShutdown;

/// Run the canonical AllInOne service with its co-located Worker.
pub async fn run_all_in_one(args: ServiceArgs) -> Result<(), String> {
    awaken_process_startup::run_all_in_one(args, Arc::new(LocalWorkerPreparation::default())).await
}

/// Run the canonical AllInOne service with product-supplied infrastructure
/// adapters and its co-located Worker, retaining Awaken's listener,
/// Worker-supervision, drain and shutdown lifecycle.
pub async fn run_all_in_one_with_services(
    args: ServiceArgs,
    managed_services: ManagedServiceAdapters,
    mut coordinator_services: CoordinatorServiceAdapters,
) -> Result<(), String> {
    let inference = coordinator_services.inference_materializer.take();
    awaken_process_startup::run_all_in_one_with_services(
        args,
        managed_services,
        coordinator_services,
        Arc::new(LocalWorkerPreparation { inference }),
    )
    .await
}

/// Prepares the AllInOne process's registered Worker over its local ACP
/// discovery and the Control credential stores it shares with the process.
#[derive(Default)]
struct LocalWorkerPreparation {
    inference: Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
}

#[async_trait::async_trait(?Send)]
impl ColocatedWorkerPreparation for LocalWorkerPreparation {
    async fn prepare(
        &self,
        mut deployment: ResolvedDeployment,
        seal_key: [u8; 32],
        workspace: Option<String>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<(ResolvedDeployment, Arc<dyn ColocatedWorker>), String> {
        let mut prepared = acp_local_credentials::prepare_local_worker_with_workspace(
            &mut deployment,
            &seal_key,
            workspace,
            clock,
        )
        .await?;
        if let Some(selected) = self.inference.clone() {
            prepared = prepared.with_inference_materializer(selected);
        }
        Ok((deployment, Arc::new(prepared)))
    }
}

#[async_trait::async_trait]
impl ColocatedWorker for PreparedLocalWorker {
    async fn validate(
        &self,
        upstream: String,
        deployment: &ResolvedDeployment,
        admin_tools: &[Arc<dyn RawTool>],
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<(), String> {
        self.clone()
            .with_admin_tools(admin_tools.to_vec())
            .build_worker(upstream, deployment, clock)
            .await
            .map(|_| ())
    }

    async fn run_incarnation(
        &self,
        upstream: String,
        deployment: &ResolvedDeployment,
        admin_tools: &[Arc<dyn RawTool>],
        clock: Arc<dyn awaken_time::WallClock>,
        mut shutdown: tokio::sync::watch::Receiver<Option<WorkerShutdown>>,
    ) -> Result<Result<(), String>, String> {
        let worker = self
            .clone()
            .with_admin_tools(admin_tools.to_vec())
            .build_worker(upstream, deployment, clock)
            .await?;
        let result = worker
            .run_until(async move {
                loop {
                    if let Some(mode) = *shutdown.borrow() {
                        return Ok(mode);
                    }
                    shutdown.changed().await.map_err(|_| {
                        Box::new(std::io::Error::other(
                            "AllInOne Worker shutdown owner dropped",
                        )) as Box<dyn std::error::Error + Send + Sync>
                    })?;
                }
            })
            .await;
        Ok(result.map_err(|error| error.to_string()))
    }
}
