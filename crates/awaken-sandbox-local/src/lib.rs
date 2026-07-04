//! Local and namespace sandbox providers backed by [`awaken_file_store::FileStore`].
//!
//! Both providers are async throughout; no blocking I/O occurs on the async executor.

mod broker;
mod env;
mod error;
mod mount;
mod output;
mod provider;
mod sandbox;
mod source;
#[cfg(test)]
mod tests;

pub mod docker;
pub mod k8s;

pub use broker::SecretBroker;
pub use docker::{DockerBind, DockerMountMaterializer};
pub use env::{
    EgressReplacer, EgressReplacerBuilder, EnvValue, EnvVar, EnvVisibility, egress_placeholder,
};
pub use error::SandboxError;
pub use k8s::{
    K8sConfigMap, K8sInitContainer, K8sMount, K8sMountMaterializer, K8sMountSource, K8sMountSpec,
    K8sVolume, K8sVolumeMount,
};
pub use mount::{Mount, MountAccess, MountLifetime, MountSource};
pub use output::{Artifact, OutputCollector};
pub use provider::{LocalSandboxProvider, NamespaceSandboxProvider, SandboxProvider};
pub use sandbox::Sandbox;
pub use source::Source;
