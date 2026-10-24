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
