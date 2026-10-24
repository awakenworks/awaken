//! Environment package-image builds.
//!
//! Each engine implements the package-image build port
//! (`awaken_container_contract::PackageImageProvisioner`); the Coordinator's
//! Environment image build stage and a Worker's Session realization both
//! build through it, and neither links a Sandbox adapter for it.
//!
//! The Kubernetes builder ([`K8sPackageImageProvisioner`], feature `k8s`)
//! runs rootless BuildKit Jobs that publish to a shared Registry and kubelet
//! image checks that prove availability. [`package_realization`] is the
//! versioned, read-only evidence binding a built image to its ConfigMap, Job,
//! Pod and the Sandbox Pod release that runs it, shared by that builder, the
//! Kubernetes Sandbox adapter and composing observers.
#![warn(unreachable_pub)]

#[cfg(feature = "k8s")]
mod kubernetes_build;
/// Versioned, typed, read-only package-realization evidence shared by the sole
/// Kubernetes emitter and pinned composing consumers.
#[cfg(feature = "k8s")]
pub mod package_realization;

#[cfg(feature = "k8s")]
pub use kubernetes_build::K8sPackageImageProvisioner;
