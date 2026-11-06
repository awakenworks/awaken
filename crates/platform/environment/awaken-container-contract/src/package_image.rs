//! The package-image build port between the Environment image builders and
//! their consumers.
//!
//! A container Sandbox provider realizes a Session's package requirements
//! through it, and the Coordinator's Environment image build stage builds and
//! publishes the same images through it. The engines that implement it own no
//! Session, job or lease state.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::RuntimeError;

/// Independent package-image build/publish port.
#[async_trait]
pub trait PackageImageProvisioner: Send + Sync {
    /// Whether the builder answers now; a builder that does not is not
    /// advertised as package provisioning (ADR-0092).
    async fn probe_builder(&self) -> Result<(), RuntimeError>;

    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        Ok(reference.to_owned())
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError>;

    async fn package_image_available(
        &self,
        _base_image: &str,
        _packages: &awaken_environment_contract::PackageRequirements,
        _network: &pc::NetworkPolicy,
        _image: &str,
    ) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}

/// How a deployment's package image builder builds and publishes: the one
/// value the Coordinator's Environment image build stage and a Worker's
/// Session realization both construct their builder from, so the two cannot
/// build the same deployment's images against different registries or
/// policies. Which engine builds is the deployment's choice, not this value's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageImageBuildSettings {
    /// The deployment-selected Podman executable.
    pub podman_bin: String,
    /// The shared OCI registry built images are published to, if any. A
    /// cluster build requires it: its Pods pull from it.
    pub registry: Option<String>,
    /// The Docker/containers authentication file for that registry.
    pub registry_auth_file: Option<std::path::PathBuf>,
    /// Whether a cluster build reaches the registry over plain HTTP.
    pub registry_insecure: bool,
    /// Whether a local engine build may use the host network; the
    /// Environment's network policy still bounds it.
    pub build_host_network: bool,
    /// Age after which unused Awaken-derived images leave the local engine cache.
    pub local_cache_ttl: std::time::Duration,
    /// The cluster namespace builder Jobs run in.
    pub cluster_namespace: String,
    /// Image pull secrets of the builder Pods.
    pub image_pull_secrets: Vec<String>,
    /// The rootless BuildKit image of the builder Pods.
    pub buildkit_image: String,
    /// Connectivity proxy for the builder's unrestricted traffic.
    pub forward_proxy: Option<crate::ForwardProxy>,
}
