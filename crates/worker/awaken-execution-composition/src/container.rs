//! Selection of backend-erased, Session-owned container environments.

use std::sync::Arc;

use crate::ContainerEnvironmentComponents;
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
use awaken_provisioning_contract::{SandboxCapacity, SandboxProvider};
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
use awaken_sandbox_container::ContainerProvider;

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
use awaken_session_environment_contract::ContainerHandResidency;
use awaken_session_executor::SandboxTier;

/// Deployment-owned security coordinates of the container tier that the plain
/// deployment configuration cannot synthesize: a no-bypass allowlist proxy
/// (whose signing key is mounted only into the Worker and Gateway) and the
/// hosted Repository Gateway trust. Standalone configuration leaves these
/// absent; a downstream composition that owns them sets them here.
/// Consumer: [`build_container_environment_for_realization`].
#[derive(Clone, Default)]
pub struct ContainerTierPolicy {
    /// Capability-authenticated proxy used only when the container runtime has
    /// independently attested a no-bypass allowlist network boundary.
    pub allowlist_proxy: Option<awaken_sandbox_container::AllowlistProxy>,
    /// True only when the deployment installed the existing Worker MCP relay
    /// and its Gateway route; runtime policy and issuer evidence still gate
    /// the advertised provider capability.
    pub worker_mcp_relay_substitution: bool,
    /// Public CA and exact host alias for the hosted private Repository Gateway.
    /// This is transport trust, not a credential or Repository authority.
    #[cfg(feature = "k8s")]
    pub repository_gateway_trust: Option<awaken_sandbox_kubernetes::K8sRepositoryGatewayTrust>,
    /// One signed, independent Create executor selected by the Worker before
    /// provider construction and bound to its Registry identity before Ready.
    #[cfg(feature = "k8s")]
    pub k8s_create_transport: Option<Arc<dyn awaken_sandbox_kubernetes::K8sCreateTransport>>,
}

impl std::fmt::Debug for ContainerTierPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("ContainerTierPolicy");
        debug.field("allowlist_proxy", &self.allowlist_proxy.is_some());
        #[cfg(feature = "k8s")]
        debug.field(
            "repository_gateway_trust",
            &self.repository_gateway_trust.is_some(),
        );
        #[cfg(feature = "k8s")]
        debug.field("k8s_create_transport", &self.k8s_create_transport.is_some());
        debug.finish()
    }
}

/// The image a container tier requires, or a fail-closed error naming the config var.
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
fn container_image(image: Option<&str>) -> Result<String, String> {
    image
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "a container sandbox tier requires AWAKEN_CONTAINER_IMAGE".to_string())
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
struct BuiltContainerEnvironment {
    provider:
        Arc<dyn SandboxProvider<Environment = dyn awaken_provisioning_contract::SessionSandbox>>,
    capacity: Option<Arc<dyn SandboxCapacity>>,
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
const RESIDENT_HAND_PORT: u16 = 7777;

#[cfg(feature = "k8s")]
fn k8s_sandbox_control_forwarder(
    image: Option<&str>,
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_kubernetes::K8sSandboxControlForwarder, String> {
    awaken_sandbox_kubernetes::K8sSandboxControlForwarder::new(
        container_image(image)?,
        settings.container_hand_bin.clone(),
        awaken_sandbox_kubernetes::DEFAULT_REPOSITORY_GIT_CONTROL_PORT,
    )
    .map_err(|error| format!("Kubernetes Sandbox control forwarder: {error}"))
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
async fn wrap<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    provider: ContainerProvider<R>,
    leased: bool,
    settings: &awaken_session_executor::SandboxSettings,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<BuiltContainerEnvironment, String> {
    let size = settings.warm_pool_size;
    if size == 0 {
        return Ok(BuiltContainerEnvironment {
            provider: Arc::new(provider),
            capacity: None,
        });
    }
    #[cfg(not(unix))]
    {
        let _ = (leased, clock);
        Err("warm container capacity requires the Unix host lease protocol".into())
    }
    #[cfg(unix)]
    {
        let mut pool = awaken_sandbox_container::WarmContainerPool::with_limits(
            Arc::new(provider),
            size,
            settings.warm_pool_total_size,
            std::time::Duration::from_secs(settings.warm_pool_idle_ttl_secs),
        );
        // Warm capacity outlives this process only as far as its host lease
        // records reach: this reclaims what a dead incarnation left once its lease
        // expired, and keeps this pool's own records renewed. A runtime that names
        // no lease domain (Kubernetes) keeps process-local warm capacity.
        if leased {
            // The host lease records are judged on the process clock the
            // composition chose for the Host.
            let leases = awaken_sandbox_container::pool::WarmCapacityLeases::host_default(clock)
                .map_err(|error| format!("warm capacity leases: {error}"))?;
            pool = pool
                .with_capacity_leases(leases)
                .await
                .map_err(|error| format!("warm capacity leases: {error}"))?;
        }
        let pool = Arc::new(pool);
        Ok(BuiltContainerEnvironment {
            provider: pool.clone(),
            capacity: Some(pool),
        })
    }
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
async fn finish<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    runtime: Arc<R>,
    image: Option<&str>,
    settings: &awaken_session_executor::SandboxSettings,
    policy: &ContainerTierPolicy,
    package_provisioner: Option<Arc<dyn awaken_sandbox_container::PackageImageProvisioner>>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<BuiltContainerEnvironment, String> {
    let leased = runtime.capacity_lease_domain().is_some();
    let mut provider =
        ContainerProvider::new(runtime, container_image(image)?).with_clock(clock.clone());
    if settings.container_hand_residency == ContainerHandResidency::Resident {
        provider = provider.with_resident_hand(awaken_sandbox_container::ResidentHandConfig::new(
            settings.container_hand_bin.clone(),
            RESIDENT_HAND_PORT,
        )?);
    }
    if let Some(package_provisioner) = package_provisioner {
        provider = provider.with_package_provisioner(package_provisioner);
    }
    if let Some(url) = settings
        .container_forward_proxy
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        provider = provider.with_forward_proxy(awaken_sandbox_container::ForwardProxy {
            url: url.to_owned(),
        });
    }
    if let Some(proxy) = &policy.allowlist_proxy {
        provider = provider.with_allowlist_proxy(proxy.clone());
    }
    if policy.worker_mcp_relay_substitution {
        provider = provider.with_worker_mcp_relay_substitution();
    }
    // Every advertised capability is proven before the Worker derives its
    // manifest; an unproven one is withheld (ADR-0092 decision 5).
    provider
        .attest_startup()
        .await
        .map_err(|error| format!("container backend start-up evidence: {error}"))?;
    wrap(provider, leased, settings, clock).await
}

#[cfg(feature = "docker")]
fn configure_docker_runtime(
    mut runtime: awaken_sandbox_container::docker::DockerRuntime,
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_container::docker::DockerRuntime, String> {
    if let Some(registry) = &settings.package_image_registry {
        runtime = runtime.with_package_registry(registry);
    }
    if let Some(path) = &settings.package_registry_auth_file {
        runtime = runtime
            .with_package_registry_auth_file(path)
            .map_err(|error| format!("docker registry authentication: {error}"))?;
    }
    Ok(runtime
        .with_package_build_host_network(settings.package_build_host_network)
        .with_package_cache_ttl(std::time::Duration::from_secs(
            settings.package_local_cache_ttl_secs,
        )))
}

#[cfg(feature = "docker")]
fn docker_runtime(
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_container::docker::DockerRuntime, String> {
    configure_docker_runtime(
        awaken_sandbox_container::docker::DockerRuntime::connect_local(8080)
            .map_err(|error| format!("docker runtime: {error}"))?,
        settings,
    )
}

#[cfg(feature = "docker")]
fn docker_runtime_for_realization(
    realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_container::docker::DockerRuntime, String> {
    configure_docker_runtime(
        awaken_sandbox_container::docker::DockerRuntime::connect_local_for_realization(
            realization_namespace,
            8080,
        )
        .map_err(|error| format!("docker runtime: {error}"))?,
        settings,
    )
}

#[cfg(feature = "podman")]
fn configure_podman_runtime(
    mut runtime: awaken_sandbox_container::podman::PodmanRuntime,
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_container::podman::PodmanRuntime, String> {
    if let Some(registry) = &settings.package_image_registry {
        runtime = runtime.with_package_registry(registry);
    }
    if let Some(path) = &settings.package_registry_auth_file {
        runtime = runtime
            .with_package_registry_auth_file(path)
            .map_err(|error| format!("podman registry authentication: {error}"))?;
    }
    Ok(runtime
        .with_package_build_host_network(settings.package_build_host_network)
        .with_package_cache_ttl(std::time::Duration::from_secs(
            settings.package_local_cache_ttl_secs,
        )))
}

#[cfg(feature = "podman")]
fn podman_runtime(
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_container::podman::PodmanRuntime, String> {
    configure_podman_runtime(
        awaken_sandbox_container::podman::PodmanRuntime::with_bin(
            8080,
            settings.podman_bin.clone(),
        ),
        settings,
    )
}

#[cfg(feature = "podman")]
fn podman_runtime_for_realization(
    realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    settings: &awaken_session_executor::SandboxSettings,
) -> Result<awaken_sandbox_container::podman::PodmanRuntime, String> {
    configure_podman_runtime(
        awaken_sandbox_container::podman::PodmanRuntime::with_bin_for_realization(
            realization_namespace,
            8080,
            settings.podman_bin.clone(),
        ),
        settings,
    )
}

#[cfg(feature = "k8s")]
async fn k8s_package_provisioner(
    settings: &awaken_session_executor::SandboxSettings,
    create_transport: Option<Arc<dyn awaken_sandbox_kubernetes::K8sCreateTransport>>,
) -> Result<Option<Arc<dyn awaken_sandbox_container::PackageImageProvisioner>>, String> {
    let Some(builder) = settings.package_image_builder else {
        return Ok(None);
    };
    let registry = settings.package_image_registry.as_deref().ok_or_else(|| {
        "a Kubernetes package builder requires package_image_registry".to_string()
    })?;
    match builder {
        #[cfg(feature = "docker")]
        awaken_session_executor::PackageImageBuilder::Docker => {
            Ok(Some(Arc::new(docker_runtime(settings)?)))
        }
        #[cfg(not(feature = "docker"))]
        awaken_session_executor::PackageImageBuilder::Docker => {
            Err("package_image_builder=docker needs the `container-docker` feature".into())
        }
        #[cfg(feature = "podman")]
        awaken_session_executor::PackageImageBuilder::Podman => {
            Ok(Some(Arc::new(podman_runtime(settings)?)))
        }
        #[cfg(not(feature = "podman"))]
        awaken_session_executor::PackageImageBuilder::Podman => {
            Err("package_image_builder=podman needs the `container-podman` feature".into())
        }
        awaken_session_executor::PackageImageBuilder::Kubernetes => {
            let mut provisioner =
                awaken_sandbox_kubernetes::K8sPackageImageProvisioner::connect_with_create_transport(
                    settings.k8s_namespace.clone(),
                    registry,
                    settings.k8s_image_pull_secrets.clone(),
                    settings.package_registry_insecure,
                    create_transport,
                )
                .await
                .map_err(|error| format!("Kubernetes package builder: {error}"))?
                .with_buildkit_image(settings.k8s_buildkit_image.clone())
                .map_err(|error| format!("Kubernetes package builder: {error}"))?;
            if let Some(url) = settings
                .container_forward_proxy
                .as_deref()
                .filter(|url| !url.trim().is_empty())
            {
                provisioner = provisioner
                    .with_forward_proxy(awaken_sandbox_container::ForwardProxy {
                        url: url.to_owned(),
                    })
                    .map_err(|error| format!("Kubernetes package builder: {error}"))?;
            }
            Ok(Some(Arc::new(provisioner)))
        }
    }
}

/// Construct the raw package-image provisioner shared by Session realization
/// and Coordinator's one Environment build-job state machine. It deliberately
/// owns no journal, lease, or Environment-domain state.
pub async fn package_image_provisioner(
    deployment: &awaken_session_executor::DeploymentConfig,
) -> Result<Option<Arc<dyn awaken_sandbox_container::PackageImageProvisioner>>, String> {
    match deployment.sandbox_tier {
        #[cfg(feature = "docker")]
        SandboxTier::Docker => Ok(Some(Arc::new(docker_runtime(&deployment.sandbox)?))),
        #[cfg(feature = "podman")]
        SandboxTier::Podman => Ok(Some(Arc::new(podman_runtime(&deployment.sandbox)?))),
        #[cfg(feature = "k8s")]
        SandboxTier::K8s => k8s_package_provisioner(&deployment.sandbox, None).await,
        SandboxTier::Local | SandboxTier::Namespace => Ok(None),
        #[cfg(not(feature = "docker"))]
        SandboxTier::Docker => Ok(None),
        #[cfg(not(feature = "podman"))]
        SandboxTier::Podman => Ok(None),
        #[cfg(not(feature = "k8s"))]
        SandboxTier::K8s => Ok(None),
    }
}

/// Build the one provider used by both Native tools and ACP attempts in a Session,
/// plus creation-time credential mounts that must exist before the environment starts.
///
/// Canonical durable-Session builder. Every concrete runtime receives the same
/// installation-stable namespace; adapters may project it but must not derive a
/// process/lease identity independently.
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub async fn build_container_environment_for_realization(
    realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    tier: SandboxTier,
    image: Option<&str>,
    settings: &awaken_session_executor::SandboxSettings,
    policy: &ContainerTierPolicy,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<ContainerEnvironmentComponents, String> {
    if let Some(policy) = &settings.k8s_image_prewarm {
        policy.validate()?;
        if tier != SandboxTier::K8s {
            return Err("k8s_image_prewarm requires the Kubernetes backend".into());
        }
    }
    if settings.container_hand_residency == ContainerHandResidency::Resident
        && tier != SandboxTier::K8s
    {
        return Err("resident container Hand currently requires the Kubernetes Pod channel".into());
    }
    let built = match tier {
        #[cfg(feature = "docker")]
        SandboxTier::Docker => {
            let runtime = Arc::new(docker_runtime_for_realization(
                realization_namespace.clone(),
                settings,
            )?);
            finish(
                runtime.clone(),
                image,
                settings,
                policy,
                Some(runtime),
                clock,
            )
            .await?
        }
        #[cfg(feature = "podman")]
        SandboxTier::Podman => {
            let runtime = Arc::new(podman_runtime_for_realization(
                realization_namespace.clone(),
                settings,
            )?);
            finish(
                runtime.clone(),
                image,
                settings,
                policy,
                Some(runtime),
                clock,
            )
            .await?
        }
        #[cfg(feature = "k8s")]
        SandboxTier::K8s => {
            let namespace = settings.k8s_namespace.clone();
            // Exec-attached Session environments do not publish an ACP port. Keep the
            // constructor's legacy address inert until that adapter parameter is removed.
            let inert = "127.0.0.1:1".parse().expect("literal socket address");
            let mut runtime = awaken_sandbox_kubernetes::K8sRuntime::connect_for_realization_with_repository_gateway_trust(
                namespace,
                realization_namespace,
                inert,
                policy.repository_gateway_trust.clone(),
            )
            .await
            .map_err(|error| format!("k8s runtime: {error}"))?
            .with_image_pull_secrets(settings.k8s_image_pull_secrets.clone())
            .with_clock(clock.clone())
            .with_unschedulable_wait(std::time::Duration::from_secs(
                settings.k8s_unschedulable_wait_secs,
            ));
            if let Some(policy) = settings.k8s_image_prewarm.clone() {
                runtime = runtime
                    .with_image_prewarm(policy)
                    .map_err(|error| error.to_string())?;
            }
            runtime = runtime
                .with_sandbox_control_forwarder(k8s_sandbox_control_forwarder(image, settings)?);
            if let Some(continuation) = settings.k8s_continuation_volume.clone() {
                runtime = runtime.with_continuation_volume(continuation);
            }
            if settings.container_hand_residency == ContainerHandResidency::Resident {
                runtime = runtime.with_pod_channel_port(RESIDENT_HAND_PORT);
            }
            if let Some(transport) = &policy.k8s_create_transport {
                runtime = runtime
                    .with_create_transport(transport.clone())
                    .map_err(|error| format!("k8s Create transport: {error}"))?;
            }
            let runtime = Arc::new(runtime);
            let package_provisioner =
                k8s_package_provisioner(settings, policy.k8s_create_transport.clone()).await?;
            finish(runtime, image, settings, policy, package_provisioner, clock).await?
        }
        SandboxTier::Local | SandboxTier::Namespace => {
            return Err("local/namespace tiers do not use a container provider".into());
        }
        #[cfg(not(feature = "docker"))]
        SandboxTier::Docker => {
            return Err("AWAKEN_SANDBOX_TIER=docker needs the `container-docker` feature".into());
        }
        #[cfg(not(feature = "podman"))]
        SandboxTier::Podman => {
            return Err("AWAKEN_SANDBOX_TIER=podman needs the `container-podman` feature".into());
        }
        #[cfg(not(feature = "k8s"))]
        SandboxTier::K8s => {
            return Err("AWAKEN_SANDBOX_TIER=k8s needs the `container-k8s` feature".into());
        }
    };
    let cache_volume_initializer = matches!(tier, SandboxTier::K8s).then(|| {
        Arc::new(awaken_session_executor::SandboxCacheVolumeInitializer::new(
            built.provider.clone(),
        )) as Arc<dyn awaken_session_executor::CacheVolumeInitializer>
    });
    Ok(ContainerEnvironmentComponents {
        provider: built.provider,
        capacity: built.capacity,
        extra_mounts: Vec::new(),
        cache_volume_initializer,
    })
}

#[cfg(not(any(feature = "docker", feature = "podman", feature = "k8s")))]
pub async fn build_container_environment_for_realization(
    _realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    tier: SandboxTier,
    _image: Option<&str>,
    _settings: &awaken_session_executor::SandboxSettings,
    _policy: &ContainerTierPolicy,
    _clock: Arc<dyn awaken_time::WallClock>,
) -> Result<ContainerEnvironmentComponents, String> {
    Err(format!(
        "AWAKEN_SANDBOX_TIER={} needs its matching container feature",
        match tier {
            SandboxTier::Docker => "docker",
            SandboxTier::Podman => "podman",
            SandboxTier::K8s => "k8s",
            SandboxTier::Local => "local",
            SandboxTier::Namespace => "namespace",
        }
    ))
}

#[cfg(all(test, feature = "podman"))]
mod tests {
    use super::*;
    #[cfg(unix)]
    use awaken_provisioning_contract as pc;

    #[tokio::test]
    async fn resident_hand_rejects_a_runtime_without_the_private_pod_channel() {
        /*
         * Builder decision table: C1=resident/attached; C2=K8s/other tier.
         * R1 attached+any supported tier builds normally; R2 resident+K8s
         * reaches the Pod-channel builder; R3 resident+other rejects before
         * daemon access. FMECA: accepting loopback Resident on Docker/Podman
         * creates an unreachable Hand (S4/O3/D2); centralized builder rejection
         * prevents downstream compositions from bypassing CLI validation.
         */
        let settings = awaken_session_executor::SandboxSettings {
            container_hand_residency: ContainerHandResidency::Resident,
            ..Default::default()
        };
        let namespace =
            awaken_sandbox_container::ContainerRealizationNamespace::from_stable_parts([
                "resident-hand-rejection-test",
            ])
            .expect("static namespace parts are valid");
        let error = match build_container_environment_for_realization(
            namespace,
            SandboxTier::Podman,
            Some("sandbox:latest"),
            &settings,
            &ContainerTierPolicy::default(),
            awaken_time::system_clock(),
        )
        .await
        {
            Ok(_) => panic!("R3 must reject Resident outside Kubernetes"),
            Err(error) => error,
        };
        assert!(error.contains("requires the Kubernetes Pod channel"));
    }

    #[tokio::test]
    async fn provider_selection_supports_direct_and_warm_pool_shapes() {
        /* Capacity selection table: C1=size 0 or positive; C2=Unix lease
         * substrate or Windows host. E1 size 0 returns the direct provider;
         * E2 positive+Unix returns one warm-capacity owner; E3 positive+Windows
         * fails explicitly before allocating capacity. R1=zero=>E1;
         * R2=positive+Unix=>E2; R3=positive+Windows=>E3. */
        use awaken_sandbox_container::podman::PodmanRuntime;

        let settings = awaken_session_executor::SandboxSettings {
            warm_pool_size: 0,
            ..Default::default()
        };
        let direct = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let direct = wrap(direct, false, &settings, awaken_time::system_clock())
            .await
            .unwrap();
        assert!(direct.capacity.is_none());

        let settings = awaken_session_executor::SandboxSettings {
            warm_pool_size: 2,
            ..Default::default()
        };
        let pooled = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let pooled = wrap(pooled, false, &settings, awaken_time::system_clock()).await;
        #[cfg(unix)]
        assert!(pooled.unwrap().capacity.is_some(), "R2");
        #[cfg(not(unix))]
        assert!(
            pooled.is_err_and(|error| error.contains("requires the Unix host lease protocol")),
            "R3"
        );
    }

    #[tokio::test]
    async fn provider_selection_requires_an_image() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        let settings = awaken_session_executor::SandboxSettings::default();
        assert!(
            finish(
                Arc::new(PodmanRuntime::new(8080)),
                None,
                &settings,
                &ContainerTierPolicy::default(),
                None,
                awaken_time::system_clock()
            )
            .await
            .is_err()
        );
        assert!(
            finish(
                Arc::new(PodmanRuntime::new(8080)),
                Some("busybox"),
                &settings,
                &ContainerTierPolicy::default(),
                None,
                awaken_time::system_clock(),
            )
            .await
            .is_ok()
        );
    }

    /// L7 (warm-capacity lease table in `awaken_sandbox_container::pool::lease`):
    /// the production builder — not a test-only constructor — puts Podman warm
    /// capacity under host lease records, and capacity shutdown releases them.
    /// Real rootless Podman; self-skips when it is unavailable.
    #[cfg(unix)]
    #[tokio::test]
    async fn l7_the_production_builder_puts_warm_capacity_under_host_leases() {
        const IMAGE: &str = "docker.io/library/busybox:latest";
        let podman_runs = std::process::Command::new("podman")
            .args(["run", "--rm", "--network=none", IMAGE, "true"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !podman_runs {
            eprintln!("skipping: rootless Podman cannot start {IMAGE}");
            return;
        }
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let namespace =
            awaken_sandbox_container::ContainerRealizationNamespace::from_stable_parts([
                "l7-production-warm-lease",
                nonce.as_str(),
            ])
            .unwrap();
        let domain = format!("podman/{}", namespace.as_str());
        let settings = awaken_session_executor::SandboxSettings {
            warm_pool_size: 1,
            ..Default::default()
        };
        let components = build_container_environment_for_realization(
            namespace,
            SandboxTier::Podman,
            Some(IMAGE),
            &settings,
            &ContainerTierPolicy::default(),
            awaken_time::system_clock(),
        )
        .await
        .expect("production Podman composition");
        let capacity = components.capacity.expect("warm capacity is configured");
        let records = || {
            let root = awaken_sandbox_container::pool::WarmCapacityLeases::host_default(
                awaken_time::system_clock(),
            )
            .unwrap()
            .root()
            .to_path_buf();
            std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .flat_map(|dir| std::fs::read_dir(dir.path()).into_iter().flatten())
                .flatten()
                // Every record of this run's lease domain, lease or claim: a
                // shutdown that disposed its container must leave neither.
                .filter(|entry| {
                    std::fs::read_to_string(entry.path())
                        .is_ok_and(|record| record.contains(&domain))
                })
                .count()
        };
        // The one poolable row: mount-less and Ephemeral.
        let spec = pc::SandboxSpec {
            process_tree_supervision: false,
            scope: "l7-warm-lease".into(),
            isolation: pc::IsolationClass::Container,
            environment: None,
            command: vec!["sh".into(), "-c".into(), "sleep 3600".into()],
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
            control_services: Default::default(),
            lease_ttl_secs: None,
        };
        let warmed = capacity.prewarm_to(&spec, 1).await;
        let leased_while_warm = records();
        capacity.shutdown_capacity().await;
        assert_eq!(warmed.expect("L7 prewarm"), 1);
        assert_eq!(
            leased_while_warm, 1,
            "L7 the warm container has a host record"
        );
        assert_eq!(records(), 0, "L7 shutdown released it after disposal");
    }

    #[cfg(feature = "k8s")]
    #[test]
    fn kubernetes_control_forwarder_reuses_the_exact_sandbox_artifact() {
        /*
         * Production composition decision table:
         * C1=Kubernetes Session provider; C2=base image is digest-pinned;
         * C3=the configured companion path is absolute. E1=C1+C2+C3 installs
         * the RepositoryGitCredential control forwarder from the same artifact;
         * E2=C1+(!C2||!C3) rejects before Worker registration. This closes the
         * gap where protocol and fake-runtime tests passed while the production
         * Worker advertised no control service and every Repository Run stayed
         * Pending. Docker/Podman remain unchanged because they cannot publish
         * this Kubernetes sidecar transport.
         */
        let pinned = "registry.invalid/awaken-sandbox@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let settings = awaken_session_executor::SandboxSettings::default();
        assert!(
            k8s_sandbox_control_forwarder(Some(pinned), &settings).is_ok(),
            "E1 exact image and companion path"
        );
        let mutable = k8s_sandbox_control_forwarder(
            Some("registry.invalid/awaken-sandbox:latest"),
            &settings,
        )
        .expect_err("E2 mutable forwarder image must fail closed");
        assert!(
            mutable.contains("digest-pinned operator image"),
            "{mutable}"
        );

        let mut relative = settings;
        relative.container_hand_bin = "awaken-sandbox".into();
        let relative = k8s_sandbox_control_forwarder(Some(pinned), &relative)
            .expect_err("E2 relative executable must fail closed");
        assert!(relative.contains("absolute executable"), "{relative}");
    }
}

#[cfg(all(test, windows, feature = "docker"))]
mod windows_capacity_tests {
    use super::*;
    use awaken_sandbox_container::docker::DockerRuntime;

    #[tokio::test]
    async fn docker_capacity_is_direct_or_explicitly_unsupported() {
        /* Windows Docker capacity table: C1=size 0, C2=size positive;
         * E1 C1 returns a direct ContainerProvider without daemon I/O;
         * E2 C2 rejects before warm-capacity creation because the lease owner
         * requires Unix. R1=C1=>E1; R2=C2=>E2. */
        let runtime = Arc::new(DockerRuntime::connect_local(8080).unwrap());
        let direct = ContainerProvider::new(runtime.clone(), "sandbox:latest");
        let settings = awaken_session_executor::SandboxSettings::default();
        let built = wrap(direct, true, &settings, awaken_time::system_clock())
            .await
            .unwrap();
        assert!(built.capacity.is_none(), "R1");

        let pooled = ContainerProvider::new(runtime, "sandbox:latest");
        let settings = awaken_session_executor::SandboxSettings {
            warm_pool_size: 1,
            ..settings
        };
        let error = wrap(pooled, true, &settings, awaken_time::system_clock())
            .await
            .err()
            .expect("R2 unsupported warm capacity");
        assert!(
            error.contains("requires the Unix host lease protocol"),
            "R2"
        );
    }
}
