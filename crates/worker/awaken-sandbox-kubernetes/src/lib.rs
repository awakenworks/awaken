// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal entrypoint sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
//! Real Kubernetes container engine (ADR-0041 Slice 5) for the
//! `awaken-sandbox-container` provider, in its own crate so builds that do not
//! realize Kubernetes Sessions never compile the kube client.
//!
//! Implements [`ContainerRuntime`] over **kube** — the kube-apiserver via the SDK,
//! never `kubectl`. Faithful to awaken-next's `K3sHandWorker` + this crate's
//! [`K8sRuntime`]: a **Session-owned Pod** (PID 1 retains its namespaces while
//! attempts run through attached exec, `restartPolicy: Never`), **native GC** (an
//! `ownerReference` reaps orphans),
//! memory stores realized as authority-seeded **emptyDir** volumes, managed input Files
//! exposed through a **read-only shared volume + isolated projector sidecar**, the
//! untrusted agent
//! **hardened** (no SA token, dropped caps) + labeled for a NetworkPolicy, and the
//! stdio channel reached either by a direct **network dial** to the Service or, when
//! a rendezvous is set, by **reverse dial** (the egress-fenced Pod dials the host
//! out). Compile-verified here; running requires a cluster.
//! Shared Create capture vocabulary is read from its canonical contract:
//! ```
//! let profile = awaken_container_contract::CreateReplyProfile::FreshCreate;
//! assert_eq!(profile.header_value(), "bounded-create-v1");
//! ```
//! The old implementation path is absent, preventing a second export route:
//! ```compile_fail
//! let _: awaken_sandbox_kubernetes::CreateReplyProfile;
//! ```
#![warn(unreachable_pub)]

use awaken_container_contract::RuntimeError;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use async_trait::async_trait;
use awaken_provisioning_contract as pc;
#[cfg(test)]
use awaken_sandbox_control::SandboxControlServiceKind;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
#[cfg(test)]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Status};
use kube::Api;
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(test)]
use awaken_connection_plan::ConnectionPlan;
use awaken_provisioning_contract::SandboxEffectFence;
#[cfg(test)]
use awaken_provisioning_contract::{SandboxCreateAttempt, SandboxObservationPurpose};
#[cfg(test)]
use awaken_sandbox_container::engine::ExistingRealizationPhase;
use awaken_sandbox_container::engine::sandbox_scope_identity;
#[cfg(test)]
use awaken_sandbox_container::{ContainerObservationExpectation, ContainerRuntime};
use awaken_sandbox_container::{
    ContainerPlan, ContainerRealizationContext, ContainerRealizationIntent,
    ContainerRealizationNamespace,
};

pub(crate) mod api_defaults;
mod channel;
mod checkpoint;
mod client;
mod continuation;
mod continuation_observation;
mod create_ledger;
mod create_transport;
mod creation;
mod error;
mod existing_realization;
mod finalizer;
mod guarded_claim;
mod image_warmup;
mod lifecycle;
mod live_inputs;
mod memory;
mod names;
mod network_policy;
mod observation;
mod package_image;
/// Versioned, typed, read-only package-realization evidence shared by the sole
/// Kubernetes emitter and pinned composing consumers.
pub mod package_realization;
mod participants;
mod pod_evidence;
mod pod_projection;
mod pod_removal;
mod pod_security;
mod process;
mod process_termination;
mod projection;
mod projection_verification;
pub(crate) mod realization;
mod repository_gateway;
mod restore;
mod runtime_contract;
mod sandbox_control;
mod terminal_evidence;
#[cfg(test)]
use channel::accept_reverse_on;
use client::K8sClients;
pub use client::connect_control_client;
use continuation::{rebuild_claim_admission, rebuild_continuation_expectation};
use continuation_observation::{
    ContinuationObservationDisposition, ContinuationObservationEvidence,
    continuation_claim_observation_required, continuation_observation_disposition,
};
use error::api_not_found;
pub(crate) use error::{backend, get_optional};
use names::continuation_claim_for_pod;
pub(crate) use names::{k8s_runtime_id, pod_name};
use pod_evidence::{
    SANDBOX_ADOPTION_ANNOTATION, SANDBOX_ATTEMPT_ANNOTATION, SANDBOX_REALIZATION_ANNOTATION,
    observed_pod, observed_pod_incarnation, observed_pod_phase, pod_effect_fence,
    pod_owner_reference, projected_content_delete_preconditions, stamp_effect_fence,
    stamp_k8s_attempt, verify_projected_content,
};
#[cfg(test)]
use pod_evidence::{
    SANDBOX_EFFECT_ANNOTATION, SANDBOX_EFFECT_EPOCH_ANNOTATION, SANDBOX_EFFECT_EXPIRY_ANNOTATION,
    SANDBOX_EFFECT_OWNER_ANNOTATION, SANDBOX_EFFECT_RUNTIME_ANNOTATION, k8s_attempt_label,
};
#[cfg(test)]
use pod_projection::build_pod;
#[cfg(test)]
use pod_projection::{CONFIGMAP_KEY, build_configmap, build_credential_secret};
pub use pod_projection::{
    DEFAULT_REPOSITORY_GIT_CONTROL_PORT, K8sSandboxControlForwarder, pod_for_plan,
    pod_for_plan_with_control_forwarder,
};
use pod_projection::{build_pod_with_continuation, content_binds};
#[cfg(test)]
use pod_security::pod_resources;
use pod_security::{
    admit_network, egress_label, hardened_security_context, unenforceable_k8s_limit,
};
pub(crate) use pod_security::{has_forbidden_sandbox_namespace_shape, sandbox_network_labels};
#[cfg(test)]
use process::{K8sExecProcess, K8sExecState, k8s_exec_argv, k8s_live_file_result};
#[cfg(test)]
use process::{k8s_exit_status, signal_effect_is_complete};
pub use process_termination::{
    ObservedPodProcessExit, PodProcessBinding, PodProcessObserver, PodProcessStopper,
    PodProcessTermination, WORKER_PROCESS_EVIDENCE_FINALIZER,
};
use realization::{PodReadiness, create_or_verify, pod_readiness, stamp_pod_realization};
pub(crate) use realization::{create_or_verify_exact, stamp_realization};

pub use create_ledger::{K8sWorkerCreateLedger, WorkerCreateLedgerState};
pub use create_transport::K8sCreateTransport;
pub(crate) use create_transport::brokered_client;
pub use package_image::K8sPackageImageProvisioner;
pub use repository_gateway::K8sRepositoryGatewayTrust;

#[cfg(test)]
pub(crate) type PodSpecMutation = (&'static str, fn(&mut k8s_openapi::api::core::v1::PodSpec));

#[cfg(test)]
fn ephemeral_plan() -> ContainerPlan {
    ContainerPlan {
        image: "sandbox:1".into(),
        command: vec!["sleep".into(), "30".into()],
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: awaken_sandbox_container::NetworkMode::Open,
        egress_identity: Default::default(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        memory_mounts: Vec::new(),
        rootfs: awaken_sandbox_container::RootfsPlan::Image("sandbox:1".into()),
    }
}

static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A Kubernetes-backed [`ContainerRuntime`]. `agent_addr` is the Service endpoint the
/// runtime dials for the [`AgentChannel`]; `owner` (optional) is the GC owner.
pub struct K8sRuntime {
    clients: K8sClients,
    namespace: String,
    realization_namespace: ContainerRealizationNamespace,
    agent_addr: SocketAddr,
    owner: Option<OwnerReference>,
    /// Process-incarnation fence used by realization adoption.
    owner_id: String,
    /// When set, the host binds this address as a **reverse-dial rendezvous**: the
    /// Pod dials *out* to it (no inbound, no Service, fully egress-fenced) and the
    /// address is injected into the agent as `AWAKEN_ACP_RENDEZVOUS`. When `None`,
    /// the host direct-dials `agent_addr` (a published Service) instead.
    rendezvous: Option<SocketAddr>,
    image_pull_secrets: Vec<String>,
    repository_gateway_trust: Option<K8sRepositoryGatewayTrust>,
    /// Live apiserver evidence for the canonical sandbox NetworkPolicy graph.
    /// The same fact drives capability projection and restricted-Pod admission.
    network_policy_attestation: network_policy::Attestation,
    /// Private resident-process port reached only through the authenticated Pod
    /// port-forward subresource. `None` preserves the legacy channel topology.
    pod_channel_port: Option<u16>,
    sandbox_control_forwarder: Option<K8sSandboxControlForwarder>,
    continuation_volume: Option<awaken_container_contract::K8sContinuationVolume>,
    image_prewarm: Option<awaken_container_contract::K8sImagePrewarm>,
    create_transport: Option<Arc<dyn K8sCreateTransport>>,
    create_mode_frozen: AtomicBool,
    creation_attempts:
        Mutex<std::collections::HashMap<K8sCreationAttemptKey, Arc<K8sCreationAttemptGate>>>,
    /// The composing process's wall clock: Sandbox effect fences are judged on
    /// it. A runtime composed without one judges no fence (fail closed).
    clock: Option<Arc<dyn awaken_time::WallClock>>,
    /// Bound on waiting for the scheduler to place a Sandbox Pod (ADR-0092).
    unschedulable_wait: std::time::Duration,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct K8sCreationAttemptKey {
    scope: String,
    operation_id: String,
    owner: String,
    runtime_incarnation: String,
    epoch: u64,
}

impl K8sCreationAttemptKey {
    fn new(scope: &str, fence: &SandboxEffectFence) -> Self {
        Self {
            scope: scope.to_owned(),
            operation_id: fence.operation_id().to_owned(),
            owner: fence.owner().to_owned(),
            runtime_incarnation: fence.runtime_incarnation().to_owned(),
            epoch: fence.epoch(),
        }
    }
}

#[derive(Default)]
struct K8sCreationAttemptGate {
    barrier: Arc<tokio::sync::RwLock<()>>,
    closed: AtomicBool,
}

impl K8sRuntime {
    /// Connect via in-cluster ServiceAccount or the ambient kubeconfig.
    pub async fn connect(
        namespace: impl Into<String>,
        agent_addr: SocketAddr,
    ) -> Result<Self, RuntimeError> {
        let namespace = namespace.into();
        let realization_namespace = ContainerRealizationNamespace::from_stable_parts([
            "legacy-kubernetes-runtime",
            namespace.as_str(),
        ])
        .map_err(|error| backend(error.to_string()))?;
        Self::connect_for_realization(namespace, realization_namespace, agent_addr).await
    }

    pub async fn connect_for_realization(
        namespace: impl Into<String>,
        realization_namespace: ContainerRealizationNamespace,
        agent_addr: SocketAddr,
    ) -> Result<Self, RuntimeError> {
        Self::connect_for_realization_with_repository_gateway_trust(
            namespace,
            realization_namespace,
            agent_addr,
            None,
        )
        .await
    }

    /// Apply the exact private Git TLS egress contract before the first live
    /// NetworkPolicy attestation, so no weaker startup window is advertised.
    pub async fn connect_for_realization_with_repository_gateway_trust(
        namespace: impl Into<String>,
        realization_namespace: ContainerRealizationNamespace,
        agent_addr: SocketAddr,
        repository_gateway_trust: Option<K8sRepositoryGatewayTrust>,
    ) -> Result<Self, RuntimeError> {
        let clients = K8sClients::infer().await?;
        let namespace = namespace.into();
        let expected_repository_port = repository_gateway_trust.as_ref().map(|trust| trust.port);
        let runtime = Self {
            clients,
            namespace,
            realization_namespace,
            agent_addr,
            owner: None,
            owner_id: awaken_sandbox_container::engine::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            repository_gateway_trust,
            network_policy_attestation: network_policy::Attestation::new(expected_repository_port),
            pod_channel_port: None,
            sandbox_control_forwarder: None,
            continuation_volume: None,
            image_prewarm: None,
            create_transport: None,
            create_mode_frozen: AtomicBool::new(false),
            creation_attempts: Mutex::new(Default::default()),
            clock: None,
            unschedulable_wait: awaken_container_contract::DEFAULT_K8S_UNSCHEDULABLE_WAIT,
        };
        runtime
            .network_policy_attestation
            .refresh(&runtime.clients.control, &runtime.namespace)
            .await?;
        Ok(runtime)
    }

    /// Judge one Sandbox effect fence on the composed clock.
    pub(crate) fn validate_effect_fence(
        &self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), RuntimeError> {
        awaken_sandbox_container::engine::validate_runtime_effect_fence(
            effect_fence,
            self.clock.as_ref(),
        )
    }

    /// Set the GC owner (e.g. a Lease/ConfigMap) whose deletion reaps orphan Pods.
    #[must_use]
    pub fn with_owner(mut self, owner: OwnerReference) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Use a reverse-dial rendezvous at `addr` instead of direct-dialing the Service:
    /// the Pod dials out to it (egress-only, no inbound) and the host accepts.
    #[must_use]
    pub fn with_rendezvous(mut self, addr: SocketAddr) -> Self {
        self.rendezvous = Some(addr);
        self
    }

    /// Attach existing namespace-local imagePullSecrets to every Session Pod.
    /// Registry credentials remain Kubernetes-owned and are never injected into
    /// the Agent container.
    #[must_use]
    pub fn with_image_pull_secrets(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.image_pull_secrets = names
            .into_iter()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .collect();
        self
    }

    #[must_use]
    pub fn with_pod_channel_port(mut self, port: u16) -> Self {
        self.pod_channel_port = Some(port);
        self
    }

    #[must_use]
    pub fn with_sandbox_control_forwarder(mut self, forwarder: K8sSandboxControlForwarder) -> Self {
        self.sandbox_control_forwarder = Some(forwarder);
        self
    }

    /// Persist every canonical writable root on one PVC which is deliberately
    /// not owned by the Pod or Worker. Terminal-Pod rebuild reuses it; explicit
    /// Environment disposal removes it.
    #[must_use]
    pub fn with_continuation_volume(
        mut self,
        config: awaken_container_contract::K8sContinuationVolume,
    ) -> Self {
        self.continuation_volume = Some(config);
        self
    }

    /// Select the independent Create executor once for this runtime. Every
    /// `Api<K>::create` on the control client then crosses the same broker;
    /// reads and UID-fenced disposal remain on the existing Kubernetes client.
    pub fn with_create_transport(
        mut self,
        transport: Arc<dyn K8sCreateTransport>,
    ) -> Result<Self, RuntimeError> {
        if self.create_transport.is_some() || self.create_mode_frozen.load(Ordering::Acquire) {
            return Err(RuntimeError::Backend(
                "Kubernetes Create transport cannot replace an active admission mode".into(),
            ));
        }
        self.clients.control = brokered_client(self.clients.control.clone(), transport.clone());
        self.create_transport = Some(transport);
        Ok(self)
    }

    /// A runtime backed by a **lazy** client (no cluster dial), for unit-testing the
    /// builder + Pod-assembly paths; the live `create`/`wait`/… methods still need a
    /// real apiserver (exercised by the gated `k8s_it` integration test).
    #[cfg(test)]
    fn for_test(agent_addr: SocketAddr) -> Self {
        Self {
            clients: K8sClients::for_test(),
            namespace: "default".into(),
            realization_namespace: ContainerRealizationNamespace::from_stable_parts([
                "k8s-test-runtime",
            ])
            .expect("test namespace"),
            agent_addr,
            owner: None,
            owner_id: awaken_sandbox_container::engine::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            repository_gateway_trust: None,
            network_policy_attestation: network_policy::Attestation::new(None),
            pod_channel_port: None,
            sandbox_control_forwarder: None,
            continuation_volume: None,
            image_prewarm: None,
            create_transport: None,
            create_mode_frozen: AtomicBool::new(false),
            creation_attempts: Mutex::new(Default::default()),
            clock: Some(awaken_time::system_clock()),
            unschedulable_wait: awaken_container_contract::DEFAULT_K8S_UNSCHEDULABLE_WAIT,
        }
    }

    fn observed_creation_attempt(
        &self,
        scope: &str,
        fence: &SandboxEffectFence,
    ) -> Arc<K8sCreationAttemptGate> {
        self.creation_attempts
            .lock()
            .expect("Kubernetes creation attempt registry lock poisoned")
            .entry(K8sCreationAttemptKey::new(scope, fence))
            .or_default()
            .clone()
    }

    fn existing_creation_attempt(
        &self,
        scope: &str,
        fence: &SandboxEffectFence,
    ) -> Option<Arc<K8sCreationAttemptGate>> {
        self.creation_attempts
            .lock()
            .expect("Kubernetes creation attempt registry lock poisoned")
            .get(&K8sCreationAttemptKey::new(scope, fence))
            .cloned()
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
    }

    fn streaming_pods(&self) -> Api<Pod> {
        Api::namespaced(self.clients.streaming.clone(), &self.namespace)
    }

    fn configmaps(&self) -> Api<ConfigMap> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
    }

    fn secrets(&self) -> Api<Secret> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
    }

    fn persistent_volume_claims(&self) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
    }

    fn realization_runtime_id(&self, scope: &str) -> Result<String, RuntimeError> {
        k8s_runtime_id(&sandbox_scope_identity(
            self.realization_namespace.as_str(),
            scope,
        )?)
    }

    fn stamp_effect_evidence<K>(
        &self,
        object: &mut K,
        context: &ContainerRealizationContext<'_>,
        realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) where
        K: kube::Resource<DynamicType = ()>,
    {
        let annotations = object
            .meta_mut()
            .annotations
            .get_or_insert_with(Default::default);
        annotations.insert(
            SANDBOX_ADOPTION_ANNOTATION.to_owned(),
            context.adoption_fingerprint.to_string(),
        );
        annotations.insert(
            SANDBOX_REALIZATION_ANNOTATION.to_owned(),
            realization_fingerprint.to_string(),
        );
        annotations.insert(
            SANDBOX_ATTEMPT_ANNOTATION.to_owned(),
            context.attempt.as_str().to_owned(),
        );
        if let Some(effect_fence) = context.effect_fence {
            stamp_effect_fence(object, effect_fence);
        }
        stamp_k8s_attempt(object, context.attempt.as_str());
    }

    fn pod_for_effect(
        &self,
        id: &str,
        plan: &ContainerPlan,
        effect_fence: Option<&SandboxEffectFence>,
    ) -> Pod {
        let rendezvous = self.rendezvous.map(|a| a.to_string());
        let claim = continuation::claim_name(id, plan, self.continuation_volume.is_some());
        let mut pod = build_pod_with_continuation(
            id,
            plan,
            &self.owner,
            rendezvous.as_deref(),
            &self.image_pull_secrets,
            claim.as_deref(),
            effect_fence,
            self.sandbox_control_forwarder.as_ref(),
            self.repository_gateway_trust.as_ref(),
        );
        let labels = pod.metadata.labels.get_or_insert_with(Default::default);
        labels.insert(
            awaken_sandbox_container::engine::MANAGED_SANDBOX_LABEL.to_string(),
            "1".to_string(),
        );
        labels.insert(
            awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.to_string(),
            self.owner_id.clone(),
        );
        pod
    }
}

// Each argument is an independently governed immutable Pod input. Wrapping
// them would duplicate ContainerPlan or the runtime's existing ownership facts.

#[cfg(test)]
mod cleanup_tests;
/// Host side of the reverse-dial: bind the rendezvous and accept the Pod's outbound
/// connection, returning it as the agent channel. Extracted so it is testable with a
/// stand-in dialer (no cluster).
#[cfg(test)]
mod tests;
