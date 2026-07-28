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
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
use crate::sandbox_source::container_image;
#[cfg(any(feature = "container-docker", feature = "container-podman"))]
use crate::sandbox_source::spawn_container_reaper;

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn wrap<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    provider: ContainerProvider<R>,
    size: usize,
) -> Arc<dyn ContainerEnvironmentProvider> {
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
fn finish<R: awaken_sandbox_container::ContainerRuntime + 'static>(
    runtime: Arc<R>,
    image: Option<&str>,
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<Arc<dyn ContainerEnvironmentProvider>, String> {
    let mut provider = ContainerProvider::new(runtime, container_image(image)?);
    if let Some(url) = settings
        .container_forward_proxy
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        provider = provider.with_forward_proxy(awaken_sandbox_container::ForwardProxy {
            url: url.to_owned(),
        });
    }
    Ok(wrap(provider, settings.warm_pool_size))
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
    settings: &crate::deployment_config::SandboxSettings,
) -> Result<
    (
        Arc<dyn ContainerEnvironmentProvider>,
        Vec<pc::MountRequirement>,
    ),
    String,
> {
    let provider = match tier {
        #[cfg(feature = "container-docker")]
        SandboxTier::Docker => {
            let runtime = Arc::new(
                awaken_sandbox_container::docker::DockerRuntime::connect_local(8080)
                    .map_err(|error| format!("docker runtime: {error}"))?,
            );
            spawn_container_reaper(runtime.clone(), settings);
            finish(runtime, image, settings)?
        }
        #[cfg(feature = "container-podman")]
        SandboxTier::Podman => {
            let runtime = Arc::new(awaken_sandbox_container::podman::PodmanRuntime::with_bin(
                8080,
                settings.podman_bin.clone(),
            ));
            spawn_container_reaper(runtime.clone(), settings);
            finish(runtime, image, settings)?
        }
        #[cfg(feature = "container-k8s")]
        SandboxTier::K8s => {
            let namespace = settings.k8s_namespace.clone();
            // Exec-attached Session environments do not publish an ACP port. Keep the
            // constructor's legacy address inert until that adapter parameter is removed.
            let inert = "127.0.0.1:1".parse().expect("literal socket address");
            let runtime = Arc::new(
                awaken_sandbox_container::k8s::K8sRuntime::connect(namespace, inert)
                    .await
                    .map_err(|error| format!("k8s runtime: {error}"))?,
            );
            finish(runtime, image, settings)?
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
    Ok((provider, Vec::new()))
}

#[cfg(not(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
)))]
pub(crate) async fn build(
    tier: SandboxTier,
    _image: Option<&str>,
    _settings: &crate::deployment_config::SandboxSettings,
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

#[cfg(all(test, feature = "container-podman"))]
mod tests {
    use super::*;

    #[test]
    fn provider_composition_selects_direct_and_warm_pool_shapes() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        let direct = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let _ = wrap(direct, 0);

        let pooled = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let _ = wrap(pooled, 2);
    }

    #[test]
    fn provider_composition_requires_an_image() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        let settings = crate::deployment_config::SandboxSettings::default();
        assert!(finish(Arc::new(PodmanRuntime::new(8080)), None, &settings).is_err());
        assert!(
            finish(
                Arc::new(PodmanRuntime::new(8080)),
                Some("busybox"),
                &settings
            )
            .is_ok()
        );
    }
}
