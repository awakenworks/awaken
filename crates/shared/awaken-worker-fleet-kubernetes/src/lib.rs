// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Kubernetes control of the Worker fleet.
//!
//! The Coordinator controls its Kubernetes Worker fleet through this crate:
//! the provider-owned Create request ledger frozen with each Worker incarnation
//! ([`K8sWorkerCreateLedger`]) and its isolation guard, and the exact
//! observation and stop of a Pod's process domain ([`PodProcessObserver`],
//! [`PodProcessStopper`]). A Worker retains its own process evidence through
//! the same observer. Neither needs the Sandbox adapter: the Coordinator
//! controls the fleet, not Sandboxes.
//!
//! The Kubernetes Sandbox adapter (`awaken-sandbox-kubernetes`) creates
//! Sandbox Pods through the same ledger and reuses the same Kubernetes API
//! boundary instead of keeping a second copy: status classification
//! ([`backend`], [`get_optional`]), the exact Pod metadata CAS loop
//! ([`pod_metadata`]), API-default normalization ([`api_defaults`]) and the
//! managed Pod/claim names ([`managed_names`]).
#![warn(unreachable_pub)]

mod api_error;
mod client;
mod create_ledger;
mod create_transport;
mod network_labels;
mod process_termination;

pub mod api_defaults;
pub mod managed_names;
pub mod object_realization;
pub mod pod_metadata;

pub use api_error::{KubernetesFailure, api_conflict, api_not_found, backend, get_optional};
pub use client::connect_control_client;
pub use create_ledger::{
    K8sWorkerCreateLedger, WorkerCreateLedgerState, original_guard_release_required,
    release_original_guard, verify_pod_isolation,
};
pub use create_transport::{K8sCreateTransport, brokered_client};
pub use network_labels::sandbox_network_labels;
pub use process_termination::{
    CONTINUATION_CLEANUP_FINALIZER, ObservedPodProcessExit, PodProcessBinding, PodProcessObserver,
    PodProcessStopper, PodProcessTermination, WORKER_PROCESS_EVIDENCE_FINALIZER,
    has_forbidden_sandbox_namespace_shape,
};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
