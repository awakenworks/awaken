// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: One bootstrap installs the exact claimed ACP materializer as both launch
// @formal reason: resolver and Session secret broker; this link is not a formal-proof claim.
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

use awaken_session_executor::{DeploymentConfig, HostDependencies};
use awaken_session_environment::SandboxEnvironmentProvider;
use awaken_session_environment_contract::HandExecutorFactory;

mod acp_backend;
mod acp_policy;
mod acp_provision;
mod container;
mod local;
mod sandbox_source;

pub use acp_policy::acp_adapter_policy;
pub use acp_provision::PublishedAcpLaunchResolver;
pub use sandbox_source::{AcpLaunchRegistry, LaunchSource};

pub use container::{
    ContainerTierPolicy, build_container_environment_for_realization, package_image_provisioner,
};
pub use local::{
    local_tier_readiness, resolve_sandbox_tier, select_local_session_environment,
    with_backend_owned_environment, with_workdir_fallback,
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
/// (`HostDependencies::with_auxiliary_sandboxes`): fresh Workdir roots under the
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
    host: HostDependencies,
    deployment: &DeploymentConfig,
    components: ContainerEnvironmentComponents,
    hand_factory: Arc<dyn HandExecutorFactory>,
) -> HostDependencies {
    let settings = &deployment.sandbox;
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
    host: HostDependencies,
    deployment: &DeploymentConfig,
    provider: Arc<dyn awaken_container_contract::ContainerEnvironmentProvider>,
    capacity: Option<Arc<dyn awaken_container_contract::ContainerEnvironmentCapacity>>,
    hand_factory: Arc<dyn HandExecutorFactory>,
) -> HostDependencies {
    install_container_environment_components(
        host,
        deployment,
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
    host: HostDependencies,
    deployment: &DeploymentConfig,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    policy: &ContainerTierPolicy,
) -> Result<HostDependencies, String> {
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
        deployment,
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
    host: HostDependencies,
    deployment: &DeploymentConfig,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    policy: &ContainerTierPolicy,
) -> Result<HostDependencies, String> {
    with_session_environment_from_deployment_at(
        host,
        deployment,
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
    host: HostDependencies,
    deployment: &DeploymentConfig,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    namespace_hand_path: std::path::PathBuf,
    policy: &ContainerTierPolicy,
) -> Result<HostDependencies, String> {
    // One process owner for both warm and rebuilt Session MCP projections.
    // The Supervisor remains the only TERM/KILL/wait algorithm.
    let host = host.with_mcp_process_reaper(Arc::new(awaken_run_executor_acp::Supervisor));
    let host =
        install_container_environment(host, deployment, hand_factory.clone(), policy).await?;
    let host =
        select_local_session_environment(host, deployment, hand_factory, namespace_hand_path)
            .await?;
    Ok(if deployment.acp.is_some() {
        with_backend_owned_environment(host, deployment)
    } else {
        host
    })
}

/// Select the Deployment's Session environment, then serve `acp:*` sessions
/// from an explicitly supplied launch source in it. Product code supplies a
/// projected source backed by published access; deterministic dev fixtures
/// may supply a fixed one.
pub async fn with_acp_launch_source(
    host: HostDependencies,
    deployment: &DeploymentConfig,
    hand_factory: Arc<dyn HandExecutorFactory>,
    source: LaunchSource,
) -> Result<HostDependencies, String> {
    let host = with_session_environment_from_deployment(
        host,
        deployment,
        Some(hand_factory),
        &ContainerTierPolicy::default(),
    )
    .await?;
    Ok(install_acp_source(
        with_backend_owned_environment(host, deployment),
        deployment,
        source,
        None,
    ))
}

fn install_acp_source(
    host: HostDependencies,
    deployment: &DeploymentConfig,
    source: LaunchSource,
    store_dir: Option<std::path::PathBuf>,
) -> HostDependencies {
    let portable_root = deployment.acp_session_blob_root.clone();
    let observer = host.acp_launch_observer();
    host.with_acp(Arc::new(acp_backend::BoundAcpSessionFactory::new(
        source,
        Some(observer),
        portable_root,
        store_dir,
    )))
}

/// Deterministic test/dev startup on the same bound Session process factory.
/// Its resolver remains the only source of model and credential requirements.
#[must_use]
pub fn with_projected_acp(
    host: HostDependencies,
    deployment: &DeploymentConfig,
    cli: awaken_run_executor_acp::AcpCli,
    resolver: Arc<dyn awaken_run_executor_acp::LaunchResolver>,
    store_dir: Option<std::path::PathBuf>,
) -> HostDependencies {
    with_projected_acp_argv(host, deployment, cli, resolver, store_dir, None)
}

#[must_use]
pub fn with_projected_acp_argv(
    host: HostDependencies,
    deployment: &DeploymentConfig,
    cli: awaken_run_executor_acp::AcpCli,
    resolver: Arc<dyn awaken_run_executor_acp::LaunchResolver>,
    store_dir: Option<std::path::PathBuf>,
    launch_argv: Option<Vec<String>>,
) -> HostDependencies {
    let secret_broker = resolver.secret_broker();
    let source = LaunchSource::Projected(
        AcpLaunchRegistry::single_with_resolved_argv(cli, resolver, launch_argv)
            .unwrap_or_else(|error| panic!("configure projected ACP launch route: {error}")),
    );
    let host = install_acp_source(host, deployment, source, store_dir);
    match secret_broker {
        Some(broker) => host.with_session_secret_broker(broker),
        None => host,
    }
}

/// Install the exact ACP routes advertised by this Host's typed deployment.
///
/// This is the sole production route builder for Worker and CLI process roots.
/// It reads the same immutable `DeploymentConfig` the Host serves; the frozen
/// Run binding still selects its CLI and candidate at attempt time. A missing
/// profile installs no ACP executor, while an exact local materializer remains
/// the same secret broker for the resolver and the Session environment.
#[must_use]
pub fn with_acp_from_deployment(
    host: HostDependencies,
    deployment: &DeploymentConfig,
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    brokered: Option<Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>>,
) -> HostDependencies {
    let Some(profile) = deployment.acp.as_ref() else {
        return match credentials {
            Some(credentials) => host.with_session_secret_broker(Arc::new(credentials)),
            None => host,
        };
    };
    let base = awaken_session_executor::session_sandbox_base(deployment);
    let routes = profile
        .cli_ids()
        .map(|id| {
            let cli = *awaken_run_executor_acp::acp_cli(id)
                .expect("AcpWorkerProfile validates every CLI");
            let resolver = Arc::new(match (&credentials, &brokered) {
                (Some(_), Some(_)) => {
                    panic!("ACP credential materializers are mutually exclusive")
                }
                (Some(credentials), None) => {
                    PublishedAcpLaunchResolver::new(cli, Some(base.clone()), credentials.clone())
                }
                (None, Some(materializer)) => PublishedAcpLaunchResolver::brokered(
                    cli,
                    Some(base.clone()),
                    materializer.clone(),
                ),
                (None, None) => PublishedAcpLaunchResolver::backend_owned(cli, Some(base.clone())),
            });
            (
                cli,
                resolver as Arc<dyn awaken_run_executor_acp::LaunchResolver>,
                profile.launch_argv(id).map(<[String]>::to_vec),
            )
        })
        .collect();
    let source = LaunchSource::Projected(
        AcpLaunchRegistry::with_resolved_argv(routes)
            .unwrap_or_else(|error| panic!("configure ACP launch routes: {error}")),
    );
    let host = install_acp_source(host, deployment, source, None);
    match credentials {
        Some(credentials) => host.with_session_secret_broker(Arc::new(credentials)),
        None => host,
    }
}
