//! Composition of backend-erased, Session-owned container environments.

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerEnvironmentProvider;
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
use awaken_sandbox_container::ContainerProvider;

use crate::deployment_config::SandboxTier;
use crate::sandbox_source::LaunchSource;
#[cfg(any(feature = "container-docker", feature = "container-podman"))]
use crate::sandbox_source::spawn_container_reaper;
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
use crate::sandbox_source::{
    SANDBOX_CONFIG_HOME, acp_credential_mount, configured_container_egress_proxy, container_image,
    credential_projection, warm_pool_size,
};

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn wrap<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    provider: ContainerProvider<R>,
) -> Arc<dyn ContainerEnvironmentProvider> {
    let size = warm_pool_size();
    if size == 0 {
        Arc::new(provider)
    } else {
        Arc::new(awaken_sandbox_container::WarmContainerPool::new(
            Arc::new(provider),
            size,
        ))
    }
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn credential(
    source: &LaunchSource,
) -> Result<
    (
        Option<pc::MountRequirement>,
        Option<Arc<dyn pc::SecretBroker>>,
    ),
    String,
> {
    let projection = credential_projection(source)?;
    let mount = projection.as_ref().and_then(|(binding, _)| {
        source
            .cli()
            .and_then(|cli| acp_credential_mount(cli, binding, SANDBOX_CONFIG_HOME))
            .map(|(mount, _)| mount)
    });
    Ok((mount, projection.map(|(_, broker)| broker)))
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn finish<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    runtime: Arc<R>,
    image: Option<&str>,
    broker: Option<Arc<dyn pc::SecretBroker>>,
) -> Result<Arc<dyn ContainerEnvironmentProvider>, String> {
    let mut provider = ContainerProvider::new(runtime, container_image(image)?);
    if let Some(broker) = broker {
        provider = provider.with_secret_broker(broker);
    }
    if let Some(proxy) = configured_container_egress_proxy() {
        provider = provider.with_egress_proxy(proxy);
    }
    Ok(wrap(provider))
}

/// Build the one provider used by both Native tools and ACP attempts in a Session,
/// plus creation-time credential mounts that must exist before the environment starts.
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
pub(crate) async fn build(
    tier: SandboxTier,
    image: Option<&str>,
    source: &LaunchSource,
) -> Result<
    (
        Arc<dyn ContainerEnvironmentProvider>,
        Vec<pc::MountRequirement>,
    ),
    String,
> {
    let (credential, broker) = credential(source)?;
    let provider = match tier {
        #[cfg(feature = "container-docker")]
        SandboxTier::Docker => {
            let runtime = Arc::new(
                awaken_sandbox_container::docker::DockerRuntime::connect_local(8080)
                    .map_err(|error| format!("docker runtime: {error}"))?,
            );
            spawn_container_reaper(runtime.clone());
            finish(runtime, image, broker)?
        }
        #[cfg(feature = "container-podman")]
        SandboxTier::Podman => {
            let runtime = Arc::new(awaken_sandbox_container::podman::PodmanRuntime::new(8080));
            spawn_container_reaper(runtime.clone());
            finish(runtime, image, broker)?
        }
        #[cfg(feature = "container-k8s")]
        SandboxTier::K8s => {
            let namespace =
                std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
            // Exec-attached Session environments do not publish an ACP port. Keep the
            // constructor's legacy address inert until that adapter parameter is removed.
            let inert = "127.0.0.1:1".parse().expect("literal socket address");
            let runtime = Arc::new(
                awaken_sandbox_container::k8s::K8sRuntime::connect(namespace, inert)
                    .await
                    .map_err(|error| format!("k8s runtime: {error}"))?,
            );
            finish(runtime, image, broker)?
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
    Ok((provider, credential.into_iter().collect()))
}

#[cfg(not(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
)))]
pub(crate) async fn build(
    tier: SandboxTier,
    _image: Option<&str>,
    _source: &LaunchSource,
) -> Result<
    (
        Arc<dyn ContainerEnvironmentProvider>,
        Vec<pc::MountRequirement>,
    ),
    String,
> {
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
