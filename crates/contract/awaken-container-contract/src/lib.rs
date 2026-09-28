//! Backend-erased ports for Session-owned container environments.
//!
//! This is the container projection of the
//! [`awaken_provisioning_contract::SandboxProvider`] family:
//! the same ownership split (an unattached environment versus one owned by an
//! aggregate effect), the same observation basis, and the same terminal
//! obligation. It differs only where containers genuinely differ: environments
//! are shared `Arc<dyn ContainerEnvironment>` values that carry create-attempt
//! and checkpoint-upload participants, and adoption carries retained attempt
//! evidence.
//!
//! The ports moved here from `awaken-sandbox-container` so that a Session owner
//! can hold a container provider without linking any container backend. They
//! are their own contract crate, split from `awaken-provisioning-contract`
//! whose vocabulary they speak (ADR-0081 split-facade policy). The provider's
//! durable handle payload stays opaque here: [`ContainerEnvironmentProvider`]
//! decodes its own handles, so no backend payload vocabulary crosses this seam.

mod checkpoint;
mod environment;
mod host;
mod policy;
mod provider;

pub use checkpoint::{ContainerCheckpointAccess, ContainerCheckpointMutation};
pub use environment::{
    ContainerCreateAttempt, ContainerEnvironment, EnvironmentFile, RuntimeAgentProcess,
};
pub use host::normalize_allowlist_host;
pub use policy::{
    K8sContinuationVolume, K8sImagePrewarm, LIVE_INPUTS_ROOT, REPOSITORY_GATEWAY_CA_PATH,
    live_input_relative_path,
};
pub use provider::{
    ContainerAdoptionLayout, ContainerEnvironmentAdoption, ContainerEnvironmentCapacity,
    ContainerEnvironmentProvider, ContainerObservationPurpose, EnvironmentRealizationIntent,
    RestorableContainerEnvironmentProvider, RestoreTargetContainerEnvironmentProvider,
    restore_ladder_is_consistent,
};
