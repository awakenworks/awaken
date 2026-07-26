/// Resolve the scenario-only process fixture into the same typed deployment
/// consumed by every scenario composition. Production binaries never use these
/// environment variables; their sole boundary is `ResolvedDeployment`.
pub fn scenario_deployment() -> awaken_runtime_host::DeploymentConfig {
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.storage_dir = std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from);
    deployment.durable = std::env::var("AWAKEN_INGRESS").as_deref() == Ok("durable");
    deployment.disable_local_pool =
        std::env::var("AWAKEN_DISABLE_LOCAL_POOL").as_deref() == Ok("1");
    deployment.database_url = std::env::var("AWAKEN_DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() == Ok("postgres") {
        deployment.dispatch_backend = awaken_runtime_host::DispatchBackend::Postgres;
    }
    if std::env::var("AWAKEN_STORE").as_deref() == Ok("postgres") {
        deployment.store = awaken_runtime_host::StoreKind::Postgres;
    }
    deployment
}
