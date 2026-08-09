use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::SharedHost;

/// Resolve the scenario-only process fixture into the same typed deployment
/// consumed by every scenario composition. Production binaries never use these
/// environment variables; their sole boundary is `ResolvedDeployment`.
pub fn scenario_deployment() -> awaken_runtime_host::DeploymentConfig {
    scenario_deployment_from(|name| std::env::var(name).ok())
}

fn scenario_deployment_from(
    mut read: impl FnMut(&str) -> Option<String>,
) -> awaken_runtime_host::DeploymentConfig {
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.storage_dir = read("SESSION_DEPLOYMENT_STORAGE_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from);
    deployment.durable = read("SESSION_DEPLOYMENT_INGRESS").as_deref() == Some("durable");
    deployment.disable_local_pool =
        read("SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL").as_deref() == Some("1");
    deployment.database_url =
        read("SESSION_DEPLOYMENT_DATABASE_URL").filter(|value| !value.trim().is_empty());
    if let Some(value) = read("SESSION_DEPLOYMENT_POSTGRES_MAX_CONNECTIONS") {
        deployment.postgres_max_connections = value
            .parse()
            .ok()
            .and_then(std::num::NonZeroU32::new)
            .expect("SESSION_DEPLOYMENT_POSTGRES_MAX_CONNECTIONS must be a positive u32");
    }
    if read("SESSION_DEPLOYMENT_DISPATCH_BACKEND").as_deref() == Some("postgres") {
        deployment.dispatch_backend = awaken_runtime_host::DispatchBackend::Postgres;
    }
    if read("SESSION_DEPLOYMENT_STORE").as_deref() == Some("postgres") {
        deployment.store = awaken_runtime_host::StoreKind::Postgres;
    }
    deployment.wake = match read("SESSION_DEPLOYMENT_WAKE").as_deref() {
        None | Some("") | Some("none") => awaken_runtime_host::Wake::None,
        Some("pg-notify") => awaken_runtime_host::Wake::PgNotify,
        Some("nats") => awaken_runtime_host::Wake::Nats,
        Some(value) => {
            panic!("SESSION_DEPLOYMENT_WAKE must be none, pg-notify, or nats (got {value})")
        }
    };
    if let Some(value) =
        read("SESSION_DEPLOYMENT_WAKE_CHANNEL").filter(|value| !value.trim().is_empty())
    {
        deployment.wake_channel = value;
    }
    deployment.nats_url =
        read("SESSION_DEPLOYMENT_NATS_URL").filter(|value| !value.trim().is_empty());
    if let Some(value) =
        read("SESSION_DEPLOYMENT_DISPATCH_OWNER").filter(|value| !value.trim().is_empty())
    {
        deployment.dispatch_owner = value;
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
    let resources = deployment.storage_dir.clone().map_or_else(
        awaken_coordinator::ephemeral_resources_application,
        |storage_dir| awaken_coordinator::embedded_resources_application(&storage_dir),
    );
    // Resource durability and runtime policy are orthogonal. Even an ephemeral
    // resource application must preserve the caller's complete typed Deployment;
    // rebuilding through the convenience constructor here silently replaced an
    // explicit Local sandbox tier with the fail-closed Namespace default.
    let extraction_repository =
        SharedHost::test_memory_extraction_repository(deployment.storage_dir.as_deref());
    let host = SharedHost::new_with_resource_component_and_deployment(
        llm,
        model_ref,
        resources.ports(),
        extraction_repository,
        deployment,
    )
    .with_file_application(
        resources.files(),
        resources.file_content_source(),
        resources.artifact_publisher(),
    )
    .with_skill_bundle_source(resources.skill_bundle_source());
    let scenario_workspace = std::env::var("AWAKEN_SCENARIO_WORKSPACE")
        .ok()
        .filter(|workspace| !workspace.trim().is_empty());
    match scenario_workspace {
        Some(workspace) => host.with_local_workspace(workspace),
        None => host,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn from(entries: &[(&str, &str)]) -> awaken_runtime_host::DeploymentConfig {
        let values = entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<BTreeMap<_, _>>();
        scenario_deployment_from(|name| values.get(name).cloned())
    }

    #[test]
    fn omitted_scenario_inputs_keep_ephemeral_defaults() {
        // Cause/effect rule SD1: no scenario inputs -> no durable/store/wake or
        // remote-owner effects. This is the negative rule for every cluster axis.
        assert_eq!(
            from(&[]),
            awaken_runtime_host::DeploymentConfig::ephemeral()
        );
    }

    #[test]
    fn distributed_scenario_inputs_build_one_typed_deployment() {
        // Cause/effect decision rule SD2: durable + Postgres store/dispatch + NATS
        // wake + exact pool/owner inputs -> every typed field changes together;
        // there is no legacy AWAKEN_* compatibility path to merge or override it.
        let deployment = from(&[
            ("SESSION_DEPLOYMENT_INGRESS", "durable"),
            ("SESSION_DEPLOYMENT_STORAGE_DIR", "/tmp/runtime"),
            ("SESSION_DEPLOYMENT_DISPATCH_BACKEND", "postgres"),
            ("SESSION_DEPLOYMENT_STORE", "postgres"),
            ("SESSION_DEPLOYMENT_DATABASE_URL", "postgres://fixture"),
            ("SESSION_DEPLOYMENT_POSTGRES_MAX_CONNECTIONS", "8"),
            ("SESSION_DEPLOYMENT_WAKE", "nats"),
            ("SESSION_DEPLOYMENT_WAKE_CHANNEL", "wake.test"),
            ("SESSION_DEPLOYMENT_NATS_URL", "nats://fixture"),
            ("SESSION_DEPLOYMENT_DISPATCH_OWNER", "brain-2"),
            ("SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL", "1"),
        ]);

        assert!(deployment.durable);
        assert_eq!(
            deployment.storage_dir.as_deref(),
            Some(std::path::Path::new("/tmp/runtime"))
        );
        assert_eq!(
            deployment.dispatch_backend,
            awaken_runtime_host::DispatchBackend::Postgres
        );
        assert_eq!(deployment.store, awaken_runtime_host::StoreKind::Postgres);
        assert_eq!(
            deployment.database_url.as_deref(),
            Some("postgres://fixture")
        );
        assert_eq!(deployment.postgres_max_connections.get(), 8);
        assert_eq!(deployment.wake, awaken_runtime_host::Wake::Nats);
        assert_eq!(deployment.wake_channel, "wake.test");
        assert_eq!(deployment.nats_url.as_deref(), Some("nats://fixture"));
        assert_eq!(deployment.dispatch_owner, "brain-2");
        assert!(deployment.disable_local_pool);
    }

    #[test]
    #[should_panic(expected = "SESSION_DEPLOYMENT_WAKE must be none, pg-notify, or nats")]
    fn unknown_wake_fails_closed() {
        // Cause/effect rule SD3: an unknown wake selector -> startup failure before
        // the scenario can silently degrade to local polling.
        let _ = from(&[("SESSION_DEPLOYMENT_WAKE", "maybe")]);
    }
}
