//! Worker-plane composition of Session execution environments.
//!
//! The Runtime Host holds only the backend-erased container ports
//! (`awaken-container-contract`). This crate selects and builds the concrete
//! container tier (Docker, Podman, Kubernetes) a deployment names and installs
//! it into the Host before the Host's own deployment-driven environment
//! selection runs. Process roots — `awaken-worker`, `awaken-cli`'s all-in-one
//! path, and the scenario host — call [`with_session_environment_from_deployment`]
//! instead of the Host method, so a container tier is never assembled inside the
//! shared crate.

use std::sync::Arc;

use awaken_session_executor::{HandExecutorFactory, SharedHost};

mod container;

pub use container::{
    ContainerTierPolicy, build_container_environment_for_realization, package_image_provisioner,
};

/// Install the container-tier Session provider a container deployment names.
///
/// A non-container tier, or a Host whose startup already installed its Session
/// provider, is returned unchanged. The realization namespace is the direct
/// Host installation's stable identity: the same parts the Host used when it
/// assembled this tier itself.
pub async fn install_container_environment(
    host: SharedHost,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    policy: &ContainerTierPolicy,
) -> Result<SharedHost, String> {
    let deployment = host.deployment_config();
    if !deployment.sandbox_tier.is_container() || host.session_environment_provider_installed() {
        return Ok(host);
    }
    deployment.validate_local_execution()?;
    let hand_factory =
        hand_factory.ok_or("container Session environments require a hand executor factory")?;
    let realization_namespace =
        awaken_sandbox_container::ContainerRealizationNamespace::from_stable_parts([
            "direct-host-installation",
            host.local_workspace(),
        ])
        .map_err(|error| format!("configure Session container identity: {error}"))?;
    let components = build_container_environment_for_realization(
        realization_namespace,
        deployment.sandbox_tier,
        deployment.container_image.as_deref(),
        &deployment.sandbox,
        policy,
        host.clock(),
    )
    .await
    .map_err(|error| format!("configure the Session sandbox tier: {error}"))?;
    Ok(host.with_session_container_environment_components(components, hand_factory))
}

/// Select the Session environment from the typed Deployment: the container
/// tier through [`install_container_environment`], then the Host's own
/// selection of the local, namespace, and TrustedHost backends.
pub async fn with_session_environment_from_deployment(
    host: SharedHost,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    policy: &ContainerTierPolicy,
) -> Result<SharedHost, String> {
    install_container_environment(host, hand_factory.clone(), policy)
        .await?
        .with_session_environment_from_deployment(hand_factory)
        .await
}

/// [`with_session_environment_from_deployment`] with the installed namespace
/// Hand companion supplied by the caller (installed-binary qualification).
pub async fn with_session_environment_from_deployment_at(
    host: SharedHost,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    namespace_hand_path: std::path::PathBuf,
    policy: &ContainerTierPolicy,
) -> Result<SharedHost, String> {
    install_container_environment(host, hand_factory.clone(), policy)
        .await?
        .with_session_environment_from_deployment_at(hand_factory, namespace_hand_path)
        .await
}
