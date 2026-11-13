//! The package-image builders constructed from a deployment's
//! [`PackageImageBuildSettings`]. The Coordinator's Environment image build
//! stage and a Worker's Session realization both construct their builder
//! here, so the two cannot configure the same deployment's builder
//! differently; which engine builds is the deployment's choice
//! (`SandboxSettings::package_image_builder_for`), made by each root.

use awaken_container_contract::PackageImageBuildSettings;

/// The Docker builder, constructed without probing the engine.
#[cfg(feature = "docker")]
pub fn docker_package_image_provisioner(
    build: &PackageImageBuildSettings,
) -> Result<crate::DockerPackageImageProvisioner, String> {
    let mut builder = crate::DockerPackageImageProvisioner::connect_local()
        .map_err(|error| format!("docker package builder: {error}"))?;
    if let Some(registry) = &build.registry {
        builder = builder.with_package_registry(registry);
    }
    if let Some(path) = &build.registry_auth_file {
        builder = builder
            .with_package_registry_auth_file(path)
            .map_err(|error| format!("docker registry authentication: {error}"))?;
    }
    Ok(builder
        .with_package_build_host_network(build.build_host_network)
        .with_package_cache_ttl(build.local_cache_ttl))
}

/// The rootless Podman builder, constructed without probing the engine.
#[cfg(feature = "podman")]
pub fn podman_package_image_provisioner(
    build: &PackageImageBuildSettings,
) -> Result<crate::PodmanPackageImageProvisioner, String> {
    let mut builder = crate::PodmanPackageImageProvisioner::with_bin(build.podman_bin.clone());
    if let Some(registry) = &build.registry {
        builder = builder.with_package_registry(registry);
    }
    if let Some(path) = &build.registry_auth_file {
        builder = builder
            .with_package_registry_auth_file(path)
            .map_err(|error| format!("podman registry authentication: {error}"))?;
    }
    Ok(builder
        .with_package_build_host_network(build.build_host_network)
        .with_package_cache_ttl(build.local_cache_ttl))
}

/// The Kubernetes Kaniko builder; it requires the registry its Pods pull
/// from. A Worker creates builder Pods through its brokered `create_transport`;
/// the Coordinator creates them directly.
#[cfg(feature = "k8s")]
pub async fn kubernetes_package_image_provisioner(
    build: &PackageImageBuildSettings,
    create_transport: Option<
        std::sync::Arc<dyn awaken_worker_fleet_kubernetes::K8sCreateTransport>,
    >,
) -> Result<crate::K8sPackageImageProvisioner, String> {
    let registry = build.registry.as_deref().ok_or_else(|| {
        "a Kubernetes package builder requires package_image_registry".to_string()
    })?;
    let mut provisioner = crate::K8sPackageImageProvisioner::connect_with_create_transport(
        build.cluster_namespace.clone(),
        registry,
        build.image_pull_secrets.clone(),
        build.registry_insecure,
        create_transport,
    )
    .await
    .map_err(|error| format!("Kubernetes package builder: {error}"))?
    .with_kaniko_image(build.kaniko_image.clone())
    .map_err(|error| format!("Kubernetes package builder: {error}"))?;
    if let Some(secret) = &build.push_secret {
        provisioner = provisioner
            .with_registry_push_secret(secret.clone())
            .map_err(|error| format!("Kubernetes package builder: {error}"))?;
    }
    if let Some(proxy) = build.forward_proxy.clone() {
        provisioner = provisioner
            .with_forward_proxy(proxy)
            .map_err(|error| format!("Kubernetes package builder: {error}"))?;
    }
    Ok(provisioner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(root: &std::path::Path) -> PackageImageBuildSettings {
        PackageImageBuildSettings {
            podman_bin: root.join("missing-podman").to_string_lossy().into_owned(),
            registry: None,
            registry_auth_file: None,
            registry_insecure: false,
            build_host_network: false,
            local_cache_ttl: std::time::Duration::from_secs(60),
            cluster_namespace: "awaken".into(),
            image_pull_secrets: Vec::new(),
            kaniko_image: awaken_container_contract::DEFAULT_K8S_KANIKO_IMAGE.into(),
            push_secret: None,
            forward_proxy: None,
        }
    }

    #[cfg(feature = "podman")]
    #[test]
    fn podman_builder_validates_package_write_credentials() {
        // RC1 a configured registry credential file that is missing refuses
        // the builder with its exact configuration error before any engine
        // probe; RC2 without one the builder constructs unprobed. No
        // credential bytes or daemon are supplied.
        let temporary = tempfile::tempdir().unwrap();
        let mut build = build(temporary.path());
        podman_package_image_provisioner(&build).expect("RC2 constructs without a probe");
        build.registry = Some("registry.invalid/packages".into());
        build.registry_auth_file = Some(temporary.path().join("missing-registry-auth"));
        let error = podman_package_image_provisioner(&build)
            .err()
            .expect("RC1 the builder validates configured write credentials");
        assert!(error.contains("registry authentication"), "{error}");
    }

    #[cfg(feature = "k8s")]
    #[tokio::test]
    async fn a_kubernetes_builder_requires_the_registry_its_pods_pull_from() {
        // K1 refuses before any API connection when no registry is named.
        let temporary = tempfile::tempdir().unwrap();
        let error = kubernetes_package_image_provisioner(&build(temporary.path()), None)
            .await
            .err()
            .expect("K1 no registry, no Kubernetes builder");
        assert!(error.contains("requires package_image_registry"), "{error}");
    }
}
