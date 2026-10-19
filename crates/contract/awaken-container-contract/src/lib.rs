//! Container-tier vocabulary shared by container backends, the Gateway and the
//! Session owner: the allowlist capability wire, Kubernetes capacity and
//! continuation-volume policy, the live-input projection root, and the
//! Kubernetes Create broker wire.
//!
//! The provider's vocabulary -- realization intent, adoption, observation
//! purpose, effect ownership, the live [`awaken_provisioning_contract::SessionSandbox`]
//! and its participants -- is the provisioning contract's; this crate names it
//! from there. Provider-owned durable handle payloads stay opaque at this seam.

mod create;
mod host;
mod k8s_capacity;
mod policy;

pub use create::{K8sCreateRequest, K8sCreateResponse};
pub use host::{AllowlistCapability, normalize_allowlist_host};
pub use k8s_capacity::{
    DEFAULT_K8S_UNSCHEDULABLE_WAIT, K8sPodReadiness, K8sPodScheduledCondition, K8sPodWait,
    k8s_pod_age, k8s_pod_provisioning_deadline_ms, k8s_pod_unschedulable, k8s_pod_wait,
    kubernetes_refusal_is_capacity,
};
pub use policy::{
    K8sContinuationVolume, K8sImagePrewarm, LIVE_INPUTS_ROOT, REPOSITORY_GATEWAY_CA_PATH,
    live_input_relative_path,
};
