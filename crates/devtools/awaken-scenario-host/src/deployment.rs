/// Resolve the scenario-only process fixture into the same typed deployment
/// consumed by every scenario composition. Production binaries never use these
/// environment variables; their sole boundary is `ResolvedDeployment`.
pub fn scenario_deployment() -> awaken_runtime_host::DeploymentConfig {
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.storage_dir = std::env::var("SESSION_DEPLOYMENT_STORAGE_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from);
    deployment.durable = std::env::var("SESSION_DEPLOYMENT_INGRESS").as_deref() == Ok("durable");
    deployment.disable_local_pool =
        std::env::var("SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL").as_deref() == Ok("1");
    deployment.database_url = std::env::var("SESSION_DEPLOYMENT_DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if std::env::var("SESSION_DEPLOYMENT_DISPATCH_BACKEND").as_deref() == Ok("postgres") {
        deployment.dispatch_backend = awaken_runtime_host::DispatchBackend::Postgres;
    }
    if std::env::var("SESSION_DEPLOYMENT_STORE").as_deref() == Ok("postgres") {
        deployment.store = awaken_runtime_host::StoreKind::Postgres;
    }
    deployment
}

/// The one scenario-only storage input. Scenario compositions must consume this
/// helper or the complete typed deployment instead of rediscovering process state.
pub(crate) fn scenario_storage_dir() -> Option<std::path::PathBuf> {
    scenario_deployment().storage_dir
}

pub(crate) fn resource_host(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> SharedHost {
    resource_host_with_deployment(llm, model_ref, scenario_deployment())
}

pub(crate) fn resource_host_with_deployment(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    deployment: awaken_runtime_host::DeploymentConfig,
) -> SharedHost {
    let host = if let Some(storage_dir) = deployment.storage_dir.clone() {
        let resources = awaken_server::embedded_resource_plane(&storage_dir);
        let host = SharedHost::new_with_resource_plane_and_deployment(
            llm, model_ref, resources, deployment,
        );
        awaken_server::install_platform_memory_data_plane(&host);
        host
    } else {
        SharedHost::new(llm, model_ref).with_resource_lifecycle(Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open scenario resource lifecycle sqlite"),
        ))
    };
    let scenario_workspace = std::env::var("AWAKEN_SCENARIO_WORKSPACE")
        .ok()
        .filter(|workspace| !workspace.trim().is_empty());
    match scenario_workspace {
        Some(workspace) => host.with_local_workspace(workspace),
        None => host,
    }
}
use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::SharedHost;
