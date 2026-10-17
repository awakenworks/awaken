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
#![warn(unreachable_pub)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServiceKind;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar,
    LocalObjectReference, PersistentVolumeClaim, Pod, PodSecurityContext, PodSpec, Secret,
    SecretVolumeSource, Volume, VolumeMount,
};
#[cfg(test)]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Api;
#[cfg(test)]
use kube::Client;
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(test)]
use awaken_connection_plan::ConnectionPlan;
use awaken_sandbox_container::engine::{
    ExistingRealization, ExistingRealizationDecision, ExistingRealizationPhase,
    ExistingRealizationRecovery, RebuildContinuityEvidence, existing_realization_decision,
    sandbox_scope_identity,
};
#[cfg(test)]
use awaken_sandbox_container::{ContainerCreateAttempt, ContainerObservationExpectation};
use awaken_sandbox_container::{
    ContainerEffectFence, ContainerObservationPurpose, ContainerPlan, ContainerRealizationContext,
    ContainerRealizationIntent, ContainerRealizationNamespace, ContainerRuntime, RuntimeError,
};
use awaken_sandbox_container::{CreateTicket, TicketResult, TicketStatus};
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
mod finalizer;
mod image_warmup;
mod lifecycle;
mod live_inputs;
mod memory;
mod names;
mod network_policy;
mod package_image;
/// Versioned, typed, read-only package-realization evidence shared by the sole
/// Kubernetes emitter and pinned composing consumers.
pub mod package_realization;
mod participants;
mod pod_evidence;
mod pod_projection;
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
pub(crate) use awaken_outbound_http::install_rustls_crypto_provider;
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
use names::{configmap_name, continuation_claim_for_pod, credential_secret_name};
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
pub use pod_projection::pod_for_plan;
use pod_projection::{
    CONFIGMAP_KEY, append_writable_and_cache_volumes, content_binds, credential_binds,
    credential_key,
};
#[cfg(test)]
use pod_projection::{build_configmap, build_credential_secret};
use pod_security::{
    admit_network, egress_label, hardened_security_context, pod_resources, unenforceable_k8s_limit,
};
pub(crate) use pod_security::{has_forbidden_sandbox_namespace_shape, sandbox_network_labels};
use process::{K8sExecProcess, K8sExecState, k8s_exec_argv, k8s_live_file_result};
#[cfg(test)]
use process::{k8s_exit_status, signal_effect_is_complete};
pub use process_termination::{
    ObservedPodProcessExit, PodProcessBinding, PodProcessObserver, PodProcessStopper,
    PodProcessTermination, WORKER_PROCESS_EVIDENCE_FINALIZER,
};
use realization::{PodReadiness, create_or_verify, pod_readiness, stamp_pod_realization};
pub(crate) use realization::{create_or_verify_exact, stamp_realization};
pub use sandbox_control::{
    DEFAULT_REPOSITORY_GIT_CONTROL_PORT, K8sSandboxControlForwarder,
    pod_for_plan_with_control_forwarder,
};

pub use create_ledger::{
    CreateReplyProfile, ISOLATION_ID_ANNOTATION, ISOLATION_LABEL, K8S_CREATE_CAPTURE_HEADER,
    K8S_CREATE_WIRE_VERSION, K8sLedgerBinding, K8sWorkerCreateLedger, ORIGINAL_GUARD_FINALIZER,
    REQUEST_DIGEST_ANNOTATION, RequestJoin, TicketAdmission, WorkerCreateLedgerState,
    collect_create_body, original_request_metadata_matches,
};
pub(crate) use create_transport::brokered_client;
pub use create_transport::{K8sCreateRequest, K8sCreateResponse, K8sCreateTransport};
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
    continuation_volume: Option<awaken_sandbox_container::K8sContinuationVolume>,
    image_prewarm: Option<awaken_sandbox_container::K8sImagePrewarm>,
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
    fn new(scope: &str, fence: &ContainerEffectFence) -> Self {
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
        config: awaken_sandbox_container::K8sContinuationVolume,
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
        fence: &ContainerEffectFence,
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
        fence: &ContainerEffectFence,
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

    fn create_claim_admission(
        &self,
        desired: &PersistentVolumeClaim,
        observed: Option<&PersistentVolumeClaim>,
        expected_fence: &ContainerEffectFence,
    ) -> Result<continuation::CreateClaimAdmission, RuntimeError> {
        let Some(observed) = observed else {
            return continuation::create_claim_admission(expected_fence, None);
        };
        let observed_fence = pod_effect_fence(observed.metadata.annotations.as_ref())?;
        let admission = continuation::create_claim_admission(
            expected_fence,
            Some(continuation::CreateClaimObservation {
                effect_fence: observed_fence.as_ref(),
                uid_present: observed.metadata.uid.is_some(),
                terminating: observed.metadata.deletion_timestamp.is_some(),
            }),
        )?;
        // First Create, 409 replay and handle-free terminal reconstruction
        // share one complete immutable PVC projection verifier.
        projection_verification::verify_claim_projection(desired, observed, None)?;
        Ok(admission)
    }

    async fn preflight_create_claim(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<(), RuntimeError> {
        let Some(desired) = self.desired_create_claim(context, plan)? else {
            return Ok(());
        };
        let expected_fence = context
            .effect_fence
            .expect("desired retained claim requires an effect fence");
        let name = desired
            .metadata
            .name
            .as_deref()
            .expect("canonical continuation claim has a name");
        let observed = crate::get_optional(&self.persistent_volume_claims(), name).await?;
        self.create_claim_admission(&desired, observed.as_ref(), expected_fence)
            .map(drop)
    }

    async fn create_or_recover_claim(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<Option<PersistentVolumeClaim>, RuntimeError> {
        let Some(desired) = self.desired_create_claim(context, plan)? else {
            return Ok(None);
        };
        let expected_fence = context
            .effect_fence
            .expect("desired retained claim requires an effect fence");
        let claims = self.persistent_volume_claims();
        let observed = create_or_verify(&claims, &desired).await?;
        match self.create_claim_admission(&desired, Some(&observed), expected_fence)? {
            continuation::CreateClaimAdmission::Reuse => Ok(Some(observed)),
            continuation::CreateClaimAdmission::Create => {
                unreachable!("an API-created or conflict-observed continuation claim is present")
            }
        }
    }

    async fn existing_realizations(
        &self,
        scope: &str,
        plan: &ContainerPlan,
    ) -> Result<Vec<ExistingRealization>, RuntimeError> {
        let runtime_id = self.realization_runtime_id(scope)?;
        let name = pod_name(&runtime_id);
        let Some(pod) = crate::get_optional(&self.pods(), &name).await? else {
            return Ok(Vec::new());
        };
        if pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(awaken_sandbox_container::engine::MANAGED_SANDBOX_LABEL))
            .map(String::as_str)
            != Some("1")
        {
            return Err(backend(
                "Kubernetes stable Sandbox name is occupied by a non-Awaken Pod",
            ));
        }
        let annotations = pod.metadata.annotations.as_ref();
        let fingerprint = annotations
            .and_then(|values| values.get(SANDBOX_REALIZATION_ANNOTATION))
            .cloned();
        let fence = pod_effect_fence(annotations)?;
        let attempt_id = annotations
            .and_then(|values| values.get(SANDBOX_ATTEMPT_ANNOTATION))
            .cloned();
        let mut phase = observed_pod_phase(&pod);

        let expected_claim =
            continuation::claim_name(&runtime_id, plan, self.continuation_volume.is_some());
        let pod_claim = continuation::bound_claim_name(&pod)?;
        match (expected_claim.as_deref(), pod_claim) {
            (None, None) => {}
            (Some(expected), Some(actual)) if expected == actual => {
                let live_claim_uid = match self.persistent_volume_claims().get(expected).await {
                    Ok(claim) => Some(continuation::claim_uid(&claim)?),
                    Err(error) if api_not_found(&error) => None,
                    Err(error) => return Err(backend(error)),
                };
                if live_claim_uid.as_deref() != continuation::bound_claim_uid(&pod) {
                    return Err(backend(
                        "Kubernetes continuation PVC differs from the Pod's immutable binding evidence",
                    ));
                }
            }
            _ => phase = ExistingRealizationPhase::Indeterminate,
        }

        let incarnation = observed_pod_incarnation(&pod)?;
        let recovery = if plan.egress_identity.ephemeral_capability || attempt_id.is_none() {
            ExistingRealizationRecovery::CurrentAttemptOnly
        } else {
            ExistingRealizationRecovery::Reconstructible
        };
        Ok(vec![ExistingRealization {
            locator: name,
            incarnation,
            adoption_fingerprint: annotations
                .and_then(|values| values.get(SANDBOX_ADOPTION_ANNOTATION))
                .cloned(),
            fingerprint,
            fence,
            attempt_id,
            recovery,
            phase,
        }])
    }

    async fn create_decision(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<ExistingRealizationDecision, RuntimeError> {
        self.preflight_create_claim(context, plan).await?;
        let rebuild_continuity = self.rebuild_continuity_evidence(context, plan).await?;
        existing_realization_decision(
            context,
            realization_fingerprint,
            rebuild_continuity,
            &self.existing_realizations(context.scope, plan).await?,
        )
    }

    /// Revalidate the aggregate-authorized source continuation immediately
    /// before the shared create/replace decision. A stable PVC name is not
    /// continuity evidence: only the exact persisted UID may preserve a
    /// retained workspace after the source Pod is absent or terminal.
    async fn rebuild_continuity_evidence(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<RebuildContinuityEvidence, RuntimeError> {
        let runtime_id = self.realization_runtime_id(context.scope)?;
        let expected = rebuild_continuation_expectation(
            context.intent,
            continuation::claim_name(&runtime_id, plan, self.continuation_volume.is_some()),
        )?;
        let Some(expected) = expected else {
            return Ok(RebuildContinuityEvidence::Unavailable);
        };
        let claim = match self
            .persistent_volume_claims()
            .get(&expected.claim_name)
            .await
        {
            Ok(claim) => claim,
            Err(error) if api_not_found(&error) => {
                return Err(backend(
                    "Kubernetes rebuild source continuation PVC is absent",
                ));
            }
            Err(error) => return Err(backend(error)),
        };
        rebuild_claim_admission(&claim, &expected.claim_uid)?;
        Ok(RebuildContinuityEvidence::ExactContinuation)
    }

    async fn admit_plan(&self, plan: &ContainerPlan) -> Result<(), RuntimeError> {
        if let Some(limit) = unenforceable_k8s_limit(&plan.limits) {
            return Err(RuntimeError::Backend(format!(
                "k8s cannot enforce a per-Pod `{limit}` limit (it is a node/kubelet \
                 setting, not a Pod-spec field); refusing to place a `{limit}`-limited \
                 spec on the k8s tier rather than silently dropping the cap"
            )));
        }
        if !matches!(plan.network, awaken_sandbox_container::NetworkMode::Open) {
            self.probe_ready().await?;
        }
        admit_network(
            &plan.network,
            self.network_policy_attestation.current(),
            self.network_policy_attestation.allowlist_current(),
        )
    }

    async fn observed_continuation_disposition(
        &self,
        pod_name: &str,
        pod: Option<&Pod>,
        runtime_handle: Option<&awaken_sandbox_container::ContainerContinuationHandle>,
        purpose: ContainerObservationPurpose,
        effect_fenced: bool,
        effect_fenced_total_absence: bool,
    ) -> Result<ContinuationObservationDisposition, RuntimeError> {
        let expected_claim_uid = match runtime_handle {
            Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuation {
                    claim_uid,
                },
            ) => Some(claim_uid.as_str()),
            Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
                    claim_uid,
                    ..
                },
            )
            | Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    claim_uid,
                    ..
                },
            ) => claim_uid.as_deref(),
            Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(_),
            ) => {
                return Err(backend(
                    "guarded Kubernetes claim has no ordinary Pod continuation",
                ));
            }
            Some(awaken_sandbox_container::ContainerContinuationHandle::HostBindRestoration(_)) => {
                return Err(backend(
                    "Kubernetes observation received a host-bind continuation handle",
                ));
            }
            None => None,
        };
        self.observed_continuation_disposition_for_claim(
            pod_name,
            pod,
            expected_claim_uid,
            purpose,
            effect_fenced,
            effect_fenced_total_absence,
        )
        .await
    }

    async fn observed_continuation_disposition_for_claim(
        &self,
        pod_name: &str,
        pod: Option<&Pod>,
        expected_claim_uid: Option<&str>,
        purpose: ContainerObservationPurpose,
        effect_fenced: bool,
        effect_fenced_total_absence: bool,
    ) -> Result<ContinuationObservationDisposition, RuntimeError> {
        let claim_name = continuation_claim_for_pod(pod_name)?;
        if let Some(pod) = pod {
            let bound_claim = continuation::bound_claim_name(pod)?;
            let bound_claim_uid = continuation::bound_claim_uid(pod);
            let binding_matches = match expected_claim_uid {
                None => bound_claim.is_none() && bound_claim_uid.is_none(),
                Some(expected_uid) => {
                    bound_claim == Some(claim_name.as_str())
                        && bound_claim_uid == Some(expected_uid)
                }
            };
            if !binding_matches {
                return Ok(ContinuationObservationDisposition::Incompatible);
            }
        }
        if !continuation_claim_observation_required(
            self.continuation_volume.is_some(),
            expected_claim_uid.is_some(),
        ) {
            return Ok(ContinuationObservationDisposition::Live);
        }
        let observed_claim = match self.persistent_volume_claims().get(&claim_name).await {
            Ok(claim) => Some(claim),
            Err(error) if api_not_found(&error) => None,
            Err(error) => return Err(backend(error)),
        };
        let cleanup_authorization = pod
            .map(continuation::observed_pod_cleanup_authorization)
            .transpose()?
            .flatten();
        let cleanup_gate_held = cleanup_authorization.is_some();
        continuation_observation_disposition(ContinuationObservationEvidence {
            purpose,
            pod_present: pod.is_some(),
            pod_deleting: pod.is_some_and(|pod| pod.metadata.deletion_timestamp.is_some()),
            cleanup_gate_held,
            original_terminal_guard: pod
                .is_some_and(continuation::terminal_original_guard_retained),
            expected_uid: expected_claim_uid,
            observed_uid: observed_claim
                .as_ref()
                .map(continuation::claim_uid)
                .transpose()?
                .as_deref(),
            observed_terminating: observed_claim
                .as_ref()
                .is_some_and(|claim| claim.metadata.deletion_timestamp.is_some()),
            effect_fenced,
            effect_fenced_total_absence,
        })
    }

    async fn replace_exact_realization(
        &self,
        observed: &ExistingRealization,
    ) -> Result<(), RuntimeError> {
        let pod = match self.pods().get(&observed.locator).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        if pod.metadata.finalizers.as_ref().is_some_and(|values| {
            values
                .iter()
                .any(|value| value == create_ledger::ORIGINAL_GUARD_FINALIZER)
        }) {
            return Err(backend(
                "brokered Pod replacement requires the original terminal cleanup owner",
            ));
        }
        let resource_version = observed
            .incarnation
            .version
            .as_deref()
            .ok_or_else(|| backend("Kubernetes Sandbox observation has no resourceVersion"))?;
        self.delete_exact_pod(
            &pod,
            &observed.incarnation.identity,
            resource_version,
            observed.attempt_id.as_deref(),
        )
        .await
    }

    async fn await_exact_pod_deleted(
        &self,
        pod_name: &str,
        pod_uid: &str,
        admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
    ) -> Result<(), RuntimeError> {
        const DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        lifecycle::await_exact_deletion(
            &self.pods(),
            pod_name,
            pod_uid,
            DELETE_TIMEOUT,
            "Kubernetes Sandbox Pod",
            admit,
        )
        .await
    }

    async fn begin_exact_pod_delete(
        &self,
        pod: &Pod,
        expected_uid: &str,
        expected_resource_version: &str,
        _attempt_id: Option<&str>,
        admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
    ) -> Result<(), RuntimeError> {
        let pod_name = pod
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
        if pod.metadata.uid.as_deref() != Some(expected_uid) {
            return Err(backend(
                "Kubernetes Sandbox Pod incarnation changed before deletion",
            ));
        }
        if pod.metadata.resource_version.as_deref() != Some(expected_resource_version) {
            return Err(backend(
                "Kubernetes Sandbox Pod resourceVersion changed before deletion",
            ));
        }
        let params = lifecycle::pod_delete_params(
            pod,
            kube::api::Preconditions {
                uid: Some(expected_uid.to_owned()),
                resource_version: Some(expected_resource_version.to_owned()),
            },
        );
        admit()?;
        let deleted = self.pods().delete(pod_name, &params).await;
        admit()?;
        match deleted {
            Ok(_) => {}
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        }

        Ok(())
    }

    async fn delete_exact_pod(
        &self,
        pod: &Pod,
        expected_uid: &str,
        expected_resource_version: &str,
        attempt_id: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let pod_name = pod
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
        self.begin_exact_pod_delete(
            pod,
            expected_uid,
            expected_resource_version,
            attempt_id,
            &|| Ok(()),
        )
        .await?;
        self.await_exact_pod_deleted(pod_name, expected_uid, &|| Ok(()))
            .await
    }

    async fn remove_bound(
        &self,
        container_id: &str,
        persisted_pod_uid: Option<&str>,
        persisted_claim_uid: Option<&str>,
        projected: Option<&awaken_sandbox_container::KubernetesProjectedParticipants>,
        release_original_guard: bool,
        authorization: Option<(
            &pc::SandboxDisposalAuthorization,
            &dyn pc::CurrentExecutionAuthority,
        )>,
    ) -> Result<(), RuntimeError> {
        // One borrowed admission callback threads the existing operation-bound
        // authority through all cleanup writes. It owns no lease or retry state.
        // Unfenced legacy removal retains its existing UID/continuation gates;
        // aggregate cleanup cannot supply provenance without current authority.
        let admit = || {
            if let Some((authorization, authority)) = authorization {
                pc::with_current_sandbox_effect(
                    authorization.effect_fence(),
                    authority,
                    |_| Ok(()),
                )
                .map_err(backend)?;
            }
            Ok(())
        };
        admit()?;
        participants::validate_bound(
            self,
            projected,
            persisted_pod_uid,
            None,
            authorization.is_some(),
        )?;
        let cleanup_authorization = authorization.map(|(authorization, _)| {
            continuation::PodCleanupAuthorization::Aggregate(authorization.clone())
        });
        let authorization = cleanup_authorization.as_ref();
        let effect_fenced = authorization.is_some();
        let pods = self.pods();
        let observed_pod = match pods.get(container_id).await {
            Ok(pod) => Some(pod),
            Err(error) if api_not_found(&error) => None,
            Err(error) => return Err(backend(error)),
        };
        admit()?;
        participants::validate_bound(
            self,
            projected,
            persisted_pod_uid,
            observed_pod.as_ref(),
            effect_fenced,
        )?;
        let guarded_original = create_ledger::original_guard_release_required(
            observed_pod.as_ref(),
            release_original_guard,
        )?;
        let pod_claim_uid = continuation::validated_bound_pod_claim_uid(
            observed_pod.as_ref(),
            persisted_pod_uid,
            persisted_claim_uid,
        )?;
        let expected_claim_uid = persisted_claim_uid.or(pod_claim_uid.as_deref());
        if self.continuation_volume.is_some() && expected_claim_uid.is_none() {
            let claim_name = continuation_claim_for_pod(container_id)?;
            match self.persistent_volume_claims().get(&claim_name).await {
                Err(error) if api_not_found(&error) => {}
                Err(error) => return Err(backend(error)),
                Ok(_) => {
                    return Err(backend(
                        "continuation PVC exists without persisted incarnation evidence",
                    ));
                }
            }
        }

        let continuation = self
            .observed_continuation_disposition_for_claim(
                container_id,
                observed_pod.as_ref(),
                expected_claim_uid,
                ContainerObservationPurpose::TerminalCleanup,
                effect_fenced,
                effect_fenced && persisted_claim_uid.is_some(),
            )
            .await?;
        admit()?;
        if continuation == ContinuationObservationDisposition::Incompatible {
            return Err(backend(
                "Kubernetes Sandbox continuation state differs from its durable handle",
            ));
        }

        let Some(observed_pod) = observed_pod else {
            return continuation::finish_after_absent_pod(
                self,
                container_id,
                expected_claim_uid,
                projected,
                &admit,
            )
            .await;
        };

        if continuation == ContinuationObservationDisposition::Disposing
            && observed_pod.metadata.deletion_timestamp.is_some()
            && continuation::observed_pod_cleanup_authorization(&observed_pod)?.is_none()
        {
            // Cause/effect row KCO1/K3: PVC termination plus exact deleting Pod
            // without our finalizer can only be reached after this cleanup
            // owner released the gate (the replace response may have been
            // lost). No mutation remains authorized: await exact A, then
            // re-read and await exact P. A foreign replacement UID ends only
            // the A wait and still cannot redirect the claim fence.
            let pod_uid = persisted_pod_uid.expect("disposing Pod has persisted UID");
            return continuation::finish_released_bound_disposal(
                self,
                container_id,
                pod_uid,
                expected_claim_uid,
                projected,
                guarded_original,
                &admit,
            )
            .await;
        }

        // Cause/effect order for a retained source is fixed: exact Pod A first
        // acquires the durable cleanup finalizer by UID/resourceVersion CAS;
        // only that winner may establish P's deletion intent. Rebuild DELETE
        // winning the CAS leaves P untouched. Once P is terminating, begin A's
        // exact deletion, remove only our finalizer, then await both objects.
        let (pending_pod_cleanup, pending_claim_deletion) = continuation::begin_bound_disposal(
            self,
            container_id,
            &observed_pod,
            persisted_pod_uid,
            expected_claim_uid,
            authorization,
            &admit,
        )
        .await?;
        let pod = pending_pod_cleanup
            .as_ref()
            .map_or(&observed_pod, |pending| &pending.pod);
        let incarnation = observed_pod_incarnation(pod)?;
        let resource_version = incarnation
            .version
            .as_deref()
            .expect("Kubernetes Pod observations always include resourceVersion");
        let attempt_id = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(SANDBOX_ATTEMPT_ANNOTATION))
            .map(String::as_str);
        if let Some(projected) = projected {
            participants::finish_projected(self, projected, container_id, true, &admit).await?;
        }
        self.begin_exact_pod_delete(
            pod,
            &incarnation.identity,
            resource_version,
            if projected.is_some() {
                None
            } else {
                attempt_id
            },
            &admit,
        )
        .await?;
        continuation::release_bound_guards(
            &pods,
            container_id,
            &incarnation.identity,
            pending_pod_cleanup.as_ref(),
            guarded_original,
            &admit,
        )
        .await?;
        self.await_exact_pod_deleted(container_id, &incarnation.identity, &admit)
            .await?;
        if let Some(pending) = pending_claim_deletion.as_ref() {
            admit()?;
            continuation::await_claim_deletion(&self.persistent_volume_claims(), pending, &admit)
                .await?;
        }
        participants::finish_cleanup(self, projected, container_id, &admit).await
    }

    fn pod_for_effect(
        &self,
        id: &str,
        plan: &ContainerPlan,
        effect_fence: Option<&ContainerEffectFence>,
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
#[allow(clippy::too_many_arguments)]
fn build_pod_with_continuation(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    rendezvous: Option<&str>,
    image_pull_secrets: &[String],
    continuation_claim: Option<&str>,
    effect_fence: Option<&ContainerEffectFence>,
    sandbox_control_forwarder: Option<&K8sSandboxControlForwarder>,
    repository_gateway_trust: Option<&K8sRepositoryGatewayTrust>,
) -> Pod {
    assert!(
        plan.control_services.is_empty() || sandbox_control_forwarder.is_some(),
        "a demanded Sandbox control service requires an installed trusted forwarder"
    );
    {
        let mut agent_env: Vec<EnvVar> = plan
            .env
            .iter()
            .map(|(k, v)| EnvVar {
                name: k.clone(),
                value: Some(v.clone()),
                value_from: None,
            })
            .collect();
        if let Some(trust) = repository_gateway_trust
            && plan.env.iter().any(|(key, _)| key == "HTTPS_PROXY")
        {
            agent_env.retain(|variable| variable.name != "NO_PROXY");
            agent_env.push(EnvVar {
                name: "NO_PROXY".into(),
                value: Some(format!("localhost,127.0.0.1,{}", trust.host)),
                value_from: None,
            });
        }
        if let Some(addr) = rendezvous {
            agent_env.push(EnvVar {
                name: "AWAKEN_ACP_RENDEZVOUS".into(),
                value: Some(addr.to_string()),
                value_from: None,
            });
        }

        // Each Memory store uses one authoritative host-side mounter. Its bounded
        // snapshot is streamed by a runtime-only projector into a pod-scoped
        // emptyDir before create returns; disposal reads that exact tree back and
        // lets the retained mounter perform the canonical CAS harvest. The Pod
        // receives neither a second Memory database nor Resource-authority credentials.
        let mut volumes: Vec<Volume> = Vec::new();
        let mut agent_mounts: Vec<VolumeMount> = Vec::new();
        if let Some(trust) = repository_gateway_trust {
            volumes.push(Volume {
                name: "repository-gateway-trust".into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(trust.ca_secret_name.clone()),
                    items: Some(vec![k8s_openapi::api::core::v1::KeyToPath {
                        key: trust.ca_secret_key.clone(),
                        path: "ca.crt".into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: "repository-gateway-trust".into(),
                mount_path: awaken_sandbox_container::REPOSITORY_GATEWAY_CA_PATH
                    .strip_suffix("/ca.crt")
                    .expect("Repository CA path has one file component")
                    .into(),
                read_only: Some(true),
                ..Default::default()
            });
        }
        let mut sidecars: Vec<Container> = Vec::new();
        let mut init_containers: Vec<Container> = Vec::new();
        let mut memory_projector_mounts = Vec::new();
        for (i, mm) in plan.memory_mounts.iter().enumerate() {
            let vol = format!("mem-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol.clone(),
                mount_path: mm.mount_path.clone(),
                read_only: Some(mm.access == pc::MountAccess::ReadOnly),
                ..Default::default()
            });
            memory_projector_mounts.push(VolumeMount {
                name: vol,
                mount_path: format!("/memory/{i}"),
                ..Default::default()
            });
        }
        if !memory_projector_mounts.is_empty() {
            let effect_fence = effect_fence.expect(
                "canonical Kubernetes Memory projection requires an Environment effect fence",
            );
            sidecars.push(Container {
                name: memory::PROJECTOR.into(),
                image: Some(plan.image.clone()),
                command: Some(awaken_sandbox_container::engine::environment_keepalive_command()),
                env: Some(vec![EnvVar {
                    name: projection::PROJECTION_FENCE_ENV.into(),
                    value: Some(projection::projection_fence_value(effect_fence)),
                    value_from: None,
                }]),
                volume_mounts: Some(memory_projector_mounts),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
        }

        append_writable_and_cache_volumes(
            plan,
            continuation_claim,
            &mut volumes,
            &mut agent_mounts,
        );
        live_inputs::append_projection(
            plan,
            effect_fence,
            &mut volumes,
            &mut agent_mounts,
            &mut sidecars,
        );
        if plan
            .control_services
            .contains(&SandboxControlServiceKind::RepositoryGitCredential)
            && let Some(forwarder) = sandbox_control_forwarder
        {
            let (volume, agent_mount, forwarder_container) = sandbox_control::projection(forwarder);
            volumes.push(volume);
            agent_mounts.push(agent_mount);
            sidecars.push(forwarder_container);
        }

        // Inline content has no host path a Pod can bind, so every item is backed by
        // a ConfigMap created alongside the Pod, except Managed Files whose initial
        // and later generations share the runtime-owned live projector. Other paths
        // keep the exact read-only subPath projection used by bwrap parity.
        for (i, bind) in content_binds(plan).iter().enumerate() {
            if awaken_sandbox_container::engine::manages_live_input(bind) {
                continue;
            }
            let vol = format!("cfg-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                config_map: Some(ConfigMapVolumeSource {
                    name: configmap_name(id, i),
                    ..Default::default()
                }),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol,
                mount_path: bind.mount_path.clone(),
                sub_path: Some(CONFIGMAP_KEY.to_string()),
                read_only: Some(bind.read_only),
                ..Default::default()
            });
        }

        // A Kubernetes Secret volume is immutable/read-only. Seed each native OAuth
        // credential into a pod-scoped writable emptyDir in an init container, then
        // mount that config directory at the CLI's native home. The worker harvests it via the
        // exec subresource before terminating the agent and writes it back to the broker.
        for (i, bind) in credential_binds(plan).iter().enumerate() {
            let seed_vol = format!("credential-seed-{i}");
            if bind.credential_file_path.is_none() {
                volumes.push(Volume {
                    name: seed_vol.clone(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(credential_secret_name(id, i)),
                        default_mode: Some(0o440),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                agent_mounts.push(VolumeMount {
                    name: seed_vol,
                    mount_path: bind.mount_path.clone(),
                    sub_path: Some(credential_key(bind).to_string()),
                    read_only: Some(true),
                    ..Default::default()
                });
                continue;
            }
            let writable_vol = format!("credential-rw-{i}");
            volumes.push(Volume {
                name: seed_vol.clone(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(credential_secret_name(id, i)),
                    default_mode: Some(0o400),
                    ..Default::default()
                }),
                ..Default::default()
            });
            volumes.push(Volume {
                name: writable_vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            init_containers.push(Container {
                name: format!("credential-init-{i}"),
                image: Some(plan.image.clone()),
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "cp -a /seed/. /writable/ && chmod 600 /writable/*".into(),
                ]),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: seed_vol,
                        mount_path: "/seed".into(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: writable_vol.clone(),
                        mount_path: "/writable".into(),
                        ..Default::default()
                    },
                ]),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: writable_vol,
                mount_path: bind.mount_path.clone(),
                read_only: Some(false),
                ..Default::default()
            });
        }

        let agent_command = effect_fence.map_or_else(
            || plan.command.clone(),
            |effect_fence| projection::gated_agent_command(&plan.command, effect_fence),
        );
        let mut containers = vec![Container {
            name: "agent".into(),
            image: Some(plan.image.clone()),
            // Session environment: PID 1 keeps the namespaces alive; attempt agents
            // are attached exec processes created by `spawn_agent`.
            command: Some(agent_command),
            env: Some(agent_env),
            // Enforce the advertised resource caps as the container's limits.
            resources: pod_resources(&plan.requests, &plan.limits),
            volume_mounts: (!agent_mounts.is_empty()).then_some(agent_mounts),
            // Harden the untrusted agent: no privilege escalation, all caps dropped.
            security_context: Some(hardened_security_context()),
            ..Default::default()
        }];
        containers.extend(sidecars);

        // Label the Pod with its egress posture so a platform-managed NetworkPolicy
        // selects and enforces it (deny-all-except-proxy/DNS for `restricted`). The
        // per-cluster NetworkPolicy holds the real proxy/DNS addresses; the adapter
        // only declares the posture — it does not invent IPs it doesn't have.
        let labels = sandbox_network_labels(Some(egress_label(&plan.network)));

        Pod {
            metadata: ObjectMeta {
                name: Some(pod_name(id)),
                labels: Some(labels),
                // native GC: the platform reaps this Pod when the owner is deleted.
                owner_references: owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                init_containers: (!init_containers.is_empty()).then_some(init_containers),
                containers,
                volumes: (!volumes.is_empty()).then_some(volumes),
                security_context: Some(PodSecurityContext {
                    fs_group: Some(awaken_sandbox_container::engine::SANDBOX_EXECUTION_ID),
                    ..Default::default()
                }),
                // a finished agent Pod is reaped, not looped.
                restart_policy: Some("Never".into()),
                // Disposable child/probe environments have no continuity to flush.
                // Make their canonical Pod deletion complete inside the bounded
                // provider-disposal contract; retained Sessions keep Kubernetes'
                // normal graceful-termination default.
                termination_grace_period_seconds: lifecycle::termination_grace_period(
                    plan.filesystem_continuity,
                ),
                // The untrusted agent must NOT reach the kube API (no SA token): its
                // only control-plane channel is the ACP data channel, nothing else.
                automount_service_account_token: Some(false),
                enable_service_links: Some(false),
                image_pull_secrets: (!image_pull_secrets.is_empty()).then(|| {
                    image_pull_secrets
                        .iter()
                        .map(|name| LocalObjectReference { name: name.clone() })
                        .collect()
                }),
                host_aliases: repository_gateway_trust.map(|trust| {
                    vec![k8s_openapi::api::core::v1::HostAlias {
                        hostnames: Some(vec![trust.host.clone()]),
                        ip: trust.address.to_string(),
                    }]
                }),
                ..Default::default()
            }),
            status: None,
        }
    }
}

#[cfg(test)]
mod cleanup_tests;
/// Host side of the reverse-dial: bind the rendezvous and accept the Pod's outbound
/// connection, returning it as the agent channel. Extracted so it is testable with a
/// stand-in dialer (no cluster).
#[cfg(test)]
mod tests;
