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
type ContainerCredential = (
    Option<pc::MountRequirement>,
    Option<Arc<dyn pc::SecretBroker>>,
);

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
fn credential(source: &LaunchSource) -> Result<ContainerCredential, String> {
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
    Ok(wrap(provider, warm_pool_size()))
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

#[cfg(all(test, feature = "container-podman"))]
mod tests {
    use super::*;
    use awaken_run_executor_acp::{LaunchResolver, OpenError, ResolvedModel};
    use awaken_runtime_contract::activation::RunActivation;

    struct Broker;

    struct Resolver;

    impl LaunchResolver for Resolver {
        fn model(&self, _activation: &RunActivation) -> Result<ResolvedModel, OpenError> {
            Ok(ResolvedModel {
                base_url: String::new(),
                model: String::new(),
                api_key: String::new(),
            })
        }
    }

    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvRestore {
        fn set(values: &[(&'static str, Option<&str>)]) -> Self {
            let previous = values
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect();
            for (key, value) in values {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self(previous)
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl pc::SecretBroker for Broker {
        async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Ok(Vec::new())
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    #[test]
    fn provider_composition_selects_direct_and_warm_pool_shapes() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        let direct = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let _ = wrap(direct, 0);

        let pooled = ContainerProvider::new(Arc::new(PodmanRuntime::new(8080)), "busybox");
        let _ = wrap(pooled, 2);
    }

    #[test]
    fn provider_composition_installs_a_secret_broker_and_requires_an_image() {
        use awaken_sandbox_container::podman::PodmanRuntime;

        assert!(
            finish(
                Arc::new(PodmanRuntime::new(8080)),
                None,
                Some(Arc::new(Broker)),
            )
            .is_err()
        );
        assert!(
            finish(
                Arc::new(PodmanRuntime::new(8080)),
                Some("busybox"),
                Some(Arc::new(Broker)),
            )
            .is_ok()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn projected_credentials_and_podman_build_are_wired_once() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let credential_file = temp.path().join("auth.json");
        std::fs::write(&credential_file, b"synthetic-credential").unwrap();
        std::fs::set_permissions(&credential_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let credential_path = credential_file.to_string_lossy().into_owned();
        let _env = EnvRestore::set(&[
            (
                crate::acp_provision::ACP_CREDENTIAL_FILE_ENV,
                Some(&credential_path),
            ),
            (
                "AWAKEN_CONTAINER_EGRESS_PROXY",
                Some("http://127.0.0.1:7777"),
            ),
            ("AWAKEN_SANDBOX_WARM_POOL", Some("0")),
            ("AWAKEN_SANDBOX_REAP", Some("0")),
        ]);
        let source = LaunchSource::Projected {
            cli: Box::new(*awaken_run_executor_acp::acp_cli("claude").unwrap()),
            resolver: Arc::new(Resolver),
        };

        let (mount, broker) = credential(&source).unwrap();
        let mount = mount.expect("native credential mount");
        assert_eq!(mount.mount_path, "/acp-config/.credentials.json");
        assert!(broker.is_some());
        assert!(
            finish(
                Arc::new(awaken_sandbox_container::podman::PodmanRuntime::new(8080)),
                Some("busybox"),
                broker,
            )
            .is_ok()
        );

        assert!(
            build(SandboxTier::Local, Some("busybox"), &source)
                .await
                .is_err()
        );
        assert!(
            build(SandboxTier::Podman, Some("busybox"), &source)
                .await
                .is_ok()
        );
    }
}
