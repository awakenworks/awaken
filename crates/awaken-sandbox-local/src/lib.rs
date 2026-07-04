//! Local and namespace sandbox providers backed by [`awaken_file_store::FileStore`].
//!
//! Both providers are async throughout; no blocking I/O occurs on the async executor.

mod error;
mod mount;
mod provider;
mod sandbox;
mod source;
#[cfg(test)]
mod tests;

pub mod docker;
pub mod k8s;

pub use docker::{DockerBind, DockerMountMaterializer};
pub use error::SandboxError;
pub use k8s::{
    K8sConfigMap, K8sInitContainer, K8sMount, K8sMountMaterializer, K8sMountSource, K8sMountSpec,
    K8sVolume, K8sVolumeMount,
};
pub use mount::{Mount, MountAccess};
pub use provider::{LocalSandboxProvider, NamespaceSandboxProvider, SandboxProvider};
pub use sandbox::Sandbox;
pub use source::Source;
