//! Selection of backend-erased, Session-owned container environments.

use std::sync::Arc;

use awaken_provisioning_contract as pc;
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
use awaken_sandbox_container::ContainerProvider;
use awaken_sandbox_container::{ContainerEnvironmentCapacity, ContainerEnvironmentProvider};

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
use crate::deployment_config::ContainerHandResidency;
use crate::deployment_config::SandboxTier;
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
use crate::sandbox_source::container_image;

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
struct BuiltContainerEnvironment {
    provider: Arc<dyn ContainerEnvironmentProvider>,
    capacity: Option<Arc<dyn ContainerEnvironmentCapacity>>,
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
const RESIDENT_HAND_PORT: u16 = 7777;

/// The one container-environment realization assembled from a deployment.
///
/// A downstream composition may decorate `provider` while retaining the exact
/// capacity and cache-volume owners created by this canonical builder. This
/// prevents product integrations from reproducing Kubernetes/Podman/Docker
/// construction merely to add an infrastructure concern such as checkpointing.
pub struct ContainerEnvironmentComponents {
    pub provider: Arc<dyn ContainerEnvironmentProvider>,
    pub capacity: Option<Arc<dyn ContainerEnvironmentCapacity>>,
    pub extra_mounts: Vec<pc::MountRequirement>,
    pub cache_volume_initializer: Option<Arc<dyn crate::CacheVolumeInitializer>>,
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn wrap<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    provider: ContainerProvider<R>,
    settings: &crate::deployment_config::SandboxSettings,
) -> BuiltContainerEnvironment {
    let size = settings.warm_pool_size;
    if size == 0 {
        BuiltContainerEnvironment {
            provider: Arc::new(provider),
            capacity: None,
        }
    } else {
        let pool = Arc::new(awaken_sandbox_container::WarmContainerPool::with_limits(
            Arc::new(provider),
            size,
            settings.warm_pool_total_size,
            std::time::Duration::from_secs(settings.warm_pool_idle_ttl_secs),
        ));
        BuiltContainerEnvironment {
            provider: pool.clone(),
            capacity: Some(pool),
        }
    }
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn finish<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    runtime: Arc<R>,
    image: Option<&str>,
    settings: &crate::deployment_config::SandboxSettings,
    package_provisioner: Option<Arc<dyn awaken_sandbox_container::PackageImageProvisioner>>,
) -> Result<BuiltContainerEnvironment, String> {
    let mut provider = ContainerProvider::new(runtime, container_image(image)?);
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
    if let Some(proxy) = &settings.container_allowlist_proxy {
        provider = provider.with_allowlist_proxy(proxy.clone());
    }
    Ok(wrap(provider, settings))
}

#[cfg(feature = "container-docker")]
fn configure_docker_runtime(
    mut runtime: awaken_sandbox_container::docker::DockerRuntime,
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<awaken_sandbox_container::docker::DockerRuntime, String> {
    if let Some(registry) = &settings.package_image_registry {
        runtime = runtime.with_package_registry(registry);
    }
    if let Some(path) = &settings.package_registry_auth_file {
        runtime = runtime
            .with_package_registry_auth_file(path)
            .map_err(|error| format!("docker registry authentication: {error}"))?;
    }
    Ok(
        runtime.with_package_cache_ttl(std::time::Duration::from_secs(
            settings.package_local_cache_ttl_secs,
        )),
    )
}

#[cfg(feature = "container-docker")]
fn docker_runtime(
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<awaken_sandbox_container::docker::DockerRuntime, String> {
    configure_docker_runtime(
        awaken_sandbox_container::docker::DockerRuntime::connect_local(8080)
            .map_err(|error| format!("docker runtime: {error}"))?,
        settings,
    )
}

#[cfg(feature = "container-docker")]
fn docker_runtime_for_realization(
    realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    settings: &crate::deployment_config::SandboxSettings,
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

#[cfg(feature = "container-podman")]
fn configure_podman_runtime(
    mut runtime: awaken_sandbox_container::podman::PodmanRuntime,
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<awaken_sandbox_container::podman::PodmanRuntime, String> {
    if let Some(registry) = &settings.package_image_registry {
        runtime = runtime.with_package_registry(registry);
    }
    if let Some(path) = &settings.package_registry_auth_file {
        runtime = runtime
            .with_package_registry_auth_file(path)
            .map_err(|error| format!("podman registry authentication: {error}"))?;
    }
    Ok(
        runtime.with_package_cache_ttl(std::time::Duration::from_secs(
            settings.package_local_cache_ttl_secs,
        )),
    )
}

#[cfg(feature = "container-podman")]
fn podman_runtime(
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<awaken_sandbox_container::podman::PodmanRuntime, String> {
    configure_podman_runtime(
        awaken_sandbox_container::podman::PodmanRuntime::with_bin(
            8080,
            settings.podman_bin.clone(),
        ),
        settings,
    )
}

#[cfg(feature = "container-podman")]
fn podman_runtime_for_realization(
    realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    settings: &crate::deployment_config::SandboxSettings,
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

#[cfg(feature = "container-k8s")]
async fn k8s_package_provisioner(
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<Option<Arc<dyn awaken_sandbox_container::PackageImageProvisioner>>, String> {
    let Some(builder) = settings.package_image_builder else {
        return Ok(None);
    };
    let registry = settings.package_image_registry.as_deref().ok_or_else(|| {
        "a Kubernetes package builder requires package_image_registry".to_string()
    })?;
    match builder {
        #[cfg(feature = "container-docker")]
        crate::PackageImageBuilder::Docker => Ok(Some(Arc::new(docker_runtime(settings)?))),
        #[cfg(not(feature = "container-docker"))]
        crate::PackageImageBuilder::Docker => {
            Err("package_image_builder=docker needs the `container-docker` feature".into())
        }
        #[cfg(feature = "container-podman")]
        crate::PackageImageBuilder::Podman => Ok(Some(Arc::new(podman_runtime(settings)?))),
        #[cfg(not(feature = "container-podman"))]
        crate::PackageImageBuilder::Podman => {
            Err("package_image_builder=podman needs the `container-podman` feature".into())
        }
        crate::PackageImageBuilder::Kubernetes => {
            let mut provisioner =
                awaken_sandbox_container::k8s::K8sPackageImageProvisioner::connect(
                    settings.k8s_namespace.clone(),
                    registry,
                    settings.k8s_image_pull_secrets.clone(),
                    settings.package_registry_insecure,
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
    deployment: &crate::DeploymentConfig,
) -> Result<Option<Arc<dyn awaken_sandbox_container::PackageImageProvisioner>>, String> {
    match deployment.sandbox_tier {
        #[cfg(feature = "container-docker")]
        SandboxTier::Docker => Ok(Some(Arc::new(docker_runtime(&deployment.sandbox)?))),
        #[cfg(feature = "container-podman")]
        SandboxTier::Podman => Ok(Some(Arc::new(podman_runtime(&deployment.sandbox)?))),
        #[cfg(feature = "container-k8s")]
        SandboxTier::K8s => k8s_package_provisioner(&deployment.sandbox).await,
        SandboxTier::Local | SandboxTier::Namespace => Ok(None),
        #[cfg(not(feature = "container-docker"))]
        SandboxTier::Docker => Ok(None),
        #[cfg(not(feature = "container-podman"))]
        SandboxTier::Podman => Ok(None),
        #[cfg(not(feature = "container-k8s"))]
        SandboxTier::K8s => Ok(None),
    }
}

/// Build the one provider used by both Native tools and ACP attempts in a Session,
/// plus creation-time credential mounts that must exist before the environment starts.
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
pub async fn build_container_environment(
    tier: SandboxTier,
    image: Option<&str>,
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<ContainerEnvironmentComponents, String> {
    let process_id = std::process::id().to_string();
    let legacy_namespace =
        awaken_sandbox_container::ContainerRealizationNamespace::from_stable_parts([
            "legacy-container-composition",
            process_id.as_str(),
        ])
        .map_err(|error| error.to_string())?;
    build_container_environment_for_realization(legacy_namespace, tier, image, settings).await
}

/// Canonical durable-Session builder. Every concrete runtime receives the same
/// installation-stable namespace; adapters may project it but must not derive a
/// process/lease identity independently.
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
pub async fn build_container_environment_for_realization(
    realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    tier: SandboxTier,
    image: Option<&str>,
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<ContainerEnvironmentComponents, String> {
    if settings.container_hand_residency == ContainerHandResidency::Resident
        && tier != SandboxTier::K8s
    {
        return Err("resident container Hand currently requires the Kubernetes Pod channel".into());
    }
    let built = match tier {
        #[cfg(feature = "container-docker")]
        SandboxTier::Docker => {
            let runtime = Arc::new(docker_runtime_for_realization(
                realization_namespace.clone(),
                settings,
            )?);
            finish(runtime.clone(), image, settings, Some(runtime))?
        }
        #[cfg(feature = "container-podman")]
        SandboxTier::Podman => {
            let runtime = Arc::new(podman_runtime_for_realization(
                realization_namespace.clone(),
                settings,
            )?);
            finish(runtime.clone(), image, settings, Some(runtime))?
        }
        #[cfg(feature = "container-k8s")]
        SandboxTier::K8s => {
            let namespace = settings.k8s_namespace.clone();
            // Exec-attached Session environments do not publish an ACP port. Keep the
            // constructor's legacy address inert until that adapter parameter is removed.
            let inert = "127.0.0.1:1".parse().expect("literal socket address");
            let mut runtime = awaken_sandbox_container::k8s::K8sRuntime::connect_for_realization(
                namespace,
                realization_namespace,
                inert,
            )
            .await
            .map_err(|error| format!("k8s runtime: {error}"))?
            .with_image_pull_secrets(settings.k8s_image_pull_secrets.clone());
            if let Some(continuation) = settings.k8s_continuation_volume.clone() {
                runtime = runtime.with_continuation_volume(continuation);
            }
            if settings.container_hand_residency == ContainerHandResidency::Resident {
                runtime = runtime.with_pod_channel_port(RESIDENT_HAND_PORT);
            }
            let runtime = Arc::new(runtime);
            let package_provisioner = k8s_package_provisioner(settings).await?;
            finish(runtime, image, settings, package_provisioner)?
        }
        SandboxTier::Local | SandboxTier::Namespace => {
            return Err("local/namespace tiers do not use a container provider".into());
        }
        #[cfg(not(feature = "container-docker"))]
        SandboxTier::Docker => {
            return Err("AWAKEN_SANDBOX_TIER=docker needs the `container-docker` feature".into());
        }
        #[cfg(not(feature = "container-podman"))]
        SandboxTier::Podman => {
            return Err("AWAKEN_SANDBOX_TIER=podman needs the `container-podman` feature".into());
        }
        #[cfg(not(feature = "container-k8s"))]
        SandboxTier::K8s => {
            return Err("AWAKEN_SANDBOX_TIER=k8s needs the `container-k8s` feature".into());
        }
    };
    let cache_volume_initializer = matches!(tier, SandboxTier::K8s).then(|| {
        Arc::new(crate::cache_volume::SandboxCacheVolumeInitializer::new(
            built.provider.clone(),
        )) as Arc<dyn crate::CacheVolumeInitializer>
    });
    Ok(ContainerEnvironmentComponents {
        provider: built.provider,
        capacity: built.capacity,
        extra_mounts: Vec::new(),
        cache_volume_initializer,
    })
}

#[cfg(not(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
)))]
pub async fn build_container_environment(
    tier: SandboxTier,
    _image: Option<&str>,
    _settings: &crate::deployment_config::SandboxSettings,
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

#[cfg(not(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
)))]
pub async fn build_container_environment_for_realization(
    _realization_namespace: awaken_sandbox_container::ContainerRealizationNamespace,
    tier: SandboxTier,
    image: Option<&str>,
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<ContainerEnvironmentComponents, String> {
    build_container_environment(tier, image, settings).await
}

#[cfg(all(test, feature = "container-podman"))]
mod tests {
    use super::*;

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
        let settings = crate::deployment_config::SandboxSettings {
            container_hand_residency: ContainerHandResidency::Resident,
            ..Default::default()
        };
        let error = match build_container_environment(
            SandboxTier::Podman,
            Some("sandbox:latest"),
            &settings,
        )
        .await
        {
            Ok(_) => panic!("R3 must reject Resident outside Kubernetes"),
            Err(error) => error,
        };
        assert!(error.contains("requires the Kubernetes Pod channel"));
    }

    #[test]
    fn provider_selection_supports_direct_and_warm_pool_shapes() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        let settings = crate::deployment_config::SandboxSettings {
            warm_pool_size: 0,
            ..Default::default()
        };
        let direct = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let _ = wrap(direct, &settings);

        let settings = crate::deployment_config::SandboxSettings {
            warm_pool_size: 2,
            ..Default::default()
        };
        let pooled = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let _ = wrap(pooled, &settings);
    }

    #[test]
    fn provider_selection_requires_an_image() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        let settings = crate::deployment_config::SandboxSettings::default();
        assert!(finish(Arc::new(PodmanRuntime::new(8080)), None, &settings, None).is_err());
        assert!(
            finish(
                Arc::new(PodmanRuntime::new(8080)),
                Some("busybox"),
                &settings,
                None
            )
            .is_ok()
        );
    }
}
