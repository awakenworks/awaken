//! Worker-plane composition of Session execution environments.
//!
//! The Runtime Host holds only the Session environment ports
//! (`awaken-session-environment-contract`) and never chooses a sandbox
//! implementation itself. This crate selects and builds the concrete tier a
//! deployment names — the container tiers (Docker, Podman, Kubernetes), the
//! local Workdir and Namespace tiers, and the operator-enabled TrustedHost
//! backend — and installs them into the Host. Process roots — `awaken-worker`,
//! `awaken-cli`'s all-in-one path, and the scenario host — call
//! [`with_session_environment_from_deployment`].

use std::sync::Arc;

use awaken_session_executor::{SandboxEnvironmentProvider, SharedHost};
use awaken_session_environment_contract::HandExecutorFactory;

mod container;
mod local;

pub use container::{
    ContainerTierPolicy, build_container_environment_for_realization, package_image_provisioner,
};
pub use local::{
    resolve_sandbox_tier, select_local_session_environment, with_backend_owned_environment,
    with_workdir_fallback,
};

/// The one container-environment realization assembled from a deployment by
/// [`build_container_environment_for_realization`].
///
/// A downstream composition may decorate `provider` while retaining the exact
/// capacity and cache-volume owners created by the canonical builder. This
/// prevents product integrations from reproducing Kubernetes/Podman/Docker
/// construction merely to add an infrastructure concern such as checkpointing.
/// Consumer: [`install_container_environment_components`].
pub struct ContainerEnvironmentComponents {
    pub provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
    pub capacity: Option<Arc<dyn awaken_container_contract::ContainerEnvironmentCapacity>>,
    pub extra_mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub cache_volume_initializer: Option<Arc<dyn awaken_session_executor::CacheVolumeInitializer>>,
}

/// The auxiliary-run sandboxes every process root installs on its Host
/// (`SharedHost::with_auxiliary_sandboxes`): fresh Workdir roots under the
/// process temp directory, one per auxiliary purpose.
#[must_use]
pub fn local_auxiliary_sandboxes()
-> Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory> {
    Arc::new(awaken_sandbox_local::LocalAuxiliarySandboxes::in_process_temp_dir())
}

/// Install the complete result of the canonical container builder, after a
/// downstream composition may have decorated its provider, as the Host's
/// authoritative Session provider: the container environment over it with the
/// Deployment's Hand binary, idle policy and residency. Capacity, creation
/// mounts, and cache initialization stay paired with that same provider.
#[must_use]
pub fn install_container_environment_components(
    host: SharedHost,
    components: ContainerEnvironmentComponents,
    hand_factory: Arc<dyn HandExecutorFactory>,
) -> SharedHost {
    let settings = &host.deployment_config().sandbox;
    let provider = SandboxEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
        components.provider,
        components.capacity,
        components.extra_mounts,
        hand_factory,
        settings.container_hand_bin.clone(),
        std::time::Duration::from_secs(settings.container_hand_idle_secs),
        settings.container_hand_residency,
        host.clock(),
    )
    .into_session_provider();
    let host = match components.cache_volume_initializer {
        Some(initializer) => host.with_cache_volume_initializer(initializer),
        None => host,
    };
    host.with_session_environment_provider(provider)
}

/// Install one already-configured container provider and its optional
/// never-used-capacity owner as the Host's Session provider. Keeping both
/// handles from the same startup prevents provider erasure from orphaning
/// startup warmup and shutdown drain. A caller that also installs a secret
/// broker does so afterwards.
#[must_use]
pub fn install_container_provider(
    host: SharedHost,
    provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
    capacity: Option<Arc<dyn awaken_container_contract::ContainerEnvironmentCapacity>>,
    hand_factory: Arc<dyn HandExecutorFactory>,
) -> SharedHost {
    install_container_environment_components(
        host,
        ContainerEnvironmentComponents {
            provider,
            capacity,
            extra_mounts: Vec::new(),
            cache_volume_initializer: None,
        },
        hand_factory,
    )
}

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
    Ok(install_container_environment_components(
        host,
        components,
        hand_factory,
    ))
}

/// Select the Session environment once from the typed Deployment, regardless
/// of whether the Host serves Native, provider, or ACP model execution: the
/// container tier through [`install_container_environment`], then the local
/// tiers and the TrustedHost backend, validating the installed Hand companion.
/// A Deployment that serves ACP also receives the TrustedHost Workdir backend
/// its BackendOwned candidates realize in.
pub async fn with_session_environment_from_deployment(
    host: SharedHost,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    policy: &ContainerTierPolicy,
) -> Result<SharedHost, String> {
    with_session_environment_from_deployment_at(
        host,
        hand_factory,
        local::installed_namespace_hand_path(),
        policy,
    )
    .await
}

/// [`with_session_environment_from_deployment`] with the installed namespace
/// Hand companion supplied by the caller (installed-binary qualification):
/// Cargo's exact artifact rather than the release installation directory.
/// This input never bypasses capability validation or changes selection.
pub async fn with_session_environment_from_deployment_at(
    host: SharedHost,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    namespace_hand_path: std::path::PathBuf,
    policy: &ContainerTierPolicy,
) -> Result<SharedHost, String> {
    let host = install_container_environment(host, hand_factory.clone(), policy).await?;
    let host = select_local_session_environment(host, hand_factory, namespace_hand_path).await?;
    Ok(if host.deployment_config().acp.is_some() {
        with_backend_owned_environment(host)
    } else {
        host
    })
}

/// Select the Deployment's Session environment, then serve `acp:*` sessions
/// from an explicitly supplied launch source in it. Product code supplies a
/// projected source backed by published access; deterministic dev fixtures
/// may supply a fixed one.
pub async fn with_acp_launch_source(
    host: SharedHost,
    hand_factory: Arc<dyn HandExecutorFactory>,
    source: awaken_session_executor::LaunchSource,
) -> Result<SharedHost, String> {
    let host = with_session_environment_from_deployment(
        host,
        Some(hand_factory),
        &ContainerTierPolicy::default(),
    )
    .await?;
    Ok(with_backend_owned_environment(host).with_acp_launch_source(source))
}
