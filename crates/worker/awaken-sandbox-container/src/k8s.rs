//! Real Kubernetes backend (ADR-0041 Slice 5, `k8s` feature).
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
//! out) — all via [`crate::net`]. Compile-verified here; running requires a cluster.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, SplitChannel};
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
#[cfg(test)]
use kube::Client;
use kube::api::{AttachParams, DeleteParams, ListParams, PostParams};
use kube::{Api, Resource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::net::TcpAgentTransport;
use crate::runtime::{
    ExistingRealization, ExistingRealizationDecision, ExistingRealizationPhase,
    ExistingRealizationRecovery, PhysicalIncarnation, RebuildContinuityEvidence,
    existing_realization_decision, legacy_unfenced_fingerprint, sandbox_observation,
};
use crate::{
    BindPlan, ContainerCreateAttempt, ContainerEffectFence, ContainerObservationExpectation,
    ContainerPlan, ContainerRealizationContext, ContainerRealizationIntent,
    ContainerRealizationNamespace, ContainerRuntime, ContainerState, RuntimeAgentProcess,
    RuntimeError, SandboxControlBindingRequest, container_effect_fence_from_values,
    sandbox_scope_identity,
};
mod channel;
mod client;
mod continuation;
mod creation;
mod error;
mod lifecycle;
mod live_inputs;
mod memory;
mod names;
mod network_policy;
mod pod_projection;
mod pod_security;
mod process;
mod realization;
mod restore;
mod runtime_contract;
mod sandbox_control;
#[cfg(test)]
use channel::accept_reverse_on;
use client::K8sClients;
pub(crate) use client::install_rustls_crypto_provider;
use continuation::{rebuild_claim_admission, rebuild_continuation_expectation};
use error::api_not_found;
pub(crate) use error::backend;
use names::{configmap_name, continuation_claim_for_pod, credential_secret_name};
pub(crate) use names::{k8s_runtime_id, pod_name};
#[cfg(test)]
use pod_projection::build_pod;
pub use pod_projection::pod_for_plan;
use pod_projection::{
    CONFIGMAP_KEY, append_writable_and_cache_volumes, build_configmap, build_credential_secret,
    content_binds, credential_binds, credential_key,
};
use pod_security::{
    admit_network, egress_label, hardened_security_context, pod_resources, unenforceable_k8s_limit,
};
pub(crate) use pod_security::{has_forbidden_sandbox_namespace_shape, sandbox_network_labels};
use process::{K8sExecProcess, K8sExecState, k8s_exec_argv, k8s_live_file_result};
#[cfg(test)]
use process::{k8s_exit_status, signal_effect_is_complete};
use realization::{PodReadiness, create_or_verify, pod_readiness, stamp_pod_realization};
pub(crate) use realization::{create_or_verify_exact, stamp_realization};
pub use sandbox_control::{
    DEFAULT_REPOSITORY_GIT_CONTROL_PORT, K8sSandboxControlForwarder,
    pod_for_plan_with_control_forwarder,
};

pub use crate::k8s_package_image::K8sPackageImageProvisioner;

#[cfg(test)]
pub(crate) type PodSpecMutation = (&'static str, fn(&mut k8s_openapi::api::core::v1::PodSpec));

static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const SANDBOX_REALIZATION_ANNOTATION: &str = "awaken.dev/sandbox-realization";
const SANDBOX_ADOPTION_ANNOTATION: &str = "awaken.dev/sandbox-adoption";
const SANDBOX_EFFECT_ANNOTATION: &str = "awaken.dev/sandbox-effect";
const SANDBOX_EFFECT_OWNER_ANNOTATION: &str = "awaken.dev/sandbox-effect-owner";
const SANDBOX_EFFECT_RUNTIME_ANNOTATION: &str = "awaken.dev/sandbox-effect-runtime";
const SANDBOX_EFFECT_EPOCH_ANNOTATION: &str = "awaken.dev/sandbox-effect-epoch";
const SANDBOX_EFFECT_EXPIRY_ANNOTATION: &str = "awaken.dev/sandbox-effect-expiry";
const SANDBOX_ATTEMPT_ANNOTATION: &str = "awaken.dev/sandbox-attempt";

fn k8s_attempt_label(attempt_id: &str) -> String {
    let digest = blake3::hash(attempt_id.as_bytes()).to_hex();
    format!("a-{}", &digest.as_str()[..32])
}

fn stamp_k8s_attempt<K>(object: &mut K, attempt_id: &str)
where
    K: kube::Resource<DynamicType = ()>,
{
    object
        .meta_mut()
        .labels
        .get_or_insert_with(Default::default)
        .insert(
            crate::SANDBOX_ATTEMPT_LABEL.to_owned(),
            k8s_attempt_label(attempt_id),
        );
}

fn observed_pod_phase(pod: &Pod) -> ExistingRealizationPhase {
    if pod.metadata.deletion_timestamp.is_some() {
        return ExistingRealizationPhase::Indeterminate;
    }
    match pod_readiness(pod) {
        PodReadiness::Ready => ExistingRealizationPhase::Ready,
        PodReadiness::Waiting(_) => ExistingRealizationPhase::Creating,
        PodReadiness::Failed(_) => ExistingRealizationPhase::Terminal,
    }
}

fn observed_pod_incarnation(pod: &Pod) -> Result<PhysicalIncarnation, RuntimeError> {
    Ok(PhysicalIncarnation {
        identity: pod
            .metadata
            .uid
            .clone()
            .ok_or_else(|| backend("Kubernetes Sandbox Pod has no UID"))?,
        version: Some(
            pod.metadata
                .resource_version
                .clone()
                .ok_or_else(|| backend("Kubernetes Sandbox Pod has no resourceVersion"))?,
        ),
    })
}

fn pod_effect_fence(
    annotations: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<Option<ContainerEffectFence>, RuntimeError> {
    container_effect_fence_from_values(
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_OWNER_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_RUNTIME_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_EPOCH_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_EXPIRY_ANNOTATION))
            .map(String::as_str),
    )
}

fn stamp_effect_fence<K>(object: &mut K, effect_fence: &ContainerEffectFence)
where
    K: kube::Resource<DynamicType = ()>,
{
    let annotations = object
        .meta_mut()
        .annotations
        .get_or_insert_with(Default::default);
    annotations.insert(
        SANDBOX_EFFECT_ANNOTATION.to_owned(),
        effect_fence.operation_id.clone(),
    );
    annotations.insert(
        SANDBOX_EFFECT_OWNER_ANNOTATION.to_owned(),
        effect_fence.owner.clone(),
    );
    annotations.insert(
        SANDBOX_EFFECT_RUNTIME_ANNOTATION.to_owned(),
        effect_fence.runtime_incarnation.clone(),
    );
    annotations.insert(
        SANDBOX_EFFECT_EPOCH_ANNOTATION.to_owned(),
        effect_fence.epoch.to_string(),
    );
    annotations.insert(
        SANDBOX_EFFECT_EXPIRY_ANNOTATION.to_owned(),
        effect_fence.expires_at_unix_ms.to_string(),
    );
}

fn observed_pod(pod: &Pod) -> Result<ExistingRealization, RuntimeError> {
    let locator = pod
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let annotations = pod.metadata.annotations.as_ref();
    Ok(ExistingRealization {
        locator,
        incarnation: observed_pod_incarnation(pod)?,
        adoption_fingerprint: annotations
            .and_then(|annotations| annotations.get(SANDBOX_ADOPTION_ANNOTATION))
            .cloned(),
        fingerprint: annotations
            .and_then(|annotations| annotations.get(SANDBOX_REALIZATION_ANNOTATION))
            .cloned(),
        fence: pod_effect_fence(annotations)?,
        attempt_id: annotations
            .and_then(|annotations| annotations.get(SANDBOX_ATTEMPT_ANNOTATION))
            .cloned(),
        recovery: ExistingRealizationRecovery::Reconstructible,
        phase: observed_pod_phase(pod),
    })
}

fn pod_owner_reference(pod: &Pod, expected_uid: &str) -> Result<OwnerReference, RuntimeError> {
    let name = pod
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let uid = pod
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no UID"))?;
    if uid != expected_uid {
        return Err(backend(
            "Kubernetes Sandbox Pod incarnation changed before projection",
        ));
    }
    if pod.metadata.deletion_timestamp.is_some() {
        return Err(backend(
            "Kubernetes Sandbox Pod is terminating before projection",
        ));
    }
    Ok(OwnerReference {
        api_version: "v1".into(),
        kind: "Pod".into(),
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: Some(false),
    })
}

fn has_exact_pod_owner(metadata: &ObjectMeta, pod_name: &str, pod_uid: &str) -> bool {
    metadata.owner_references.as_deref().is_some_and(|owners| {
        let [owner] = owners else {
            return false;
        };
        owner.api_version == "v1"
            && owner.kind == "Pod"
            && owner.name == pod_name
            && owner.uid == pod_uid
            && owner.controller == Some(true)
    })
}

fn has_exact_attempt(metadata: &ObjectMeta, attempt_id: &str) -> bool {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(SANDBOX_ATTEMPT_ANNOTATION))
        .is_some_and(|attempt| attempt == attempt_id)
        && metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(crate::SANDBOX_ATTEMPT_LABEL))
            .is_some_and(|attempt| attempt == &k8s_attempt_label(attempt_id))
}

fn verify_projected_content(
    metadata: &ObjectMeta,
    pod_name: &str,
    pod_uid: &str,
    attempt_id: &str,
) -> Result<(), RuntimeError> {
    if has_exact_attempt(metadata, attempt_id) && has_exact_pod_owner(metadata, pod_name, pod_uid) {
        Ok(())
    } else {
        Err(backend(
            "Kubernetes projected content differs from its exact Pod owner or attempt",
        ))
    }
}

fn projected_content_delete_preconditions(
    metadata: &ObjectMeta,
    pod_name: &str,
    pod_uid: &str,
    attempt_id: &str,
) -> Result<Option<kube::api::Preconditions>, RuntimeError> {
    if !has_exact_attempt(metadata, attempt_id) {
        return Ok(None);
    }
    let exact_owner = has_exact_pod_owner(metadata, pod_name, pod_uid);
    if !exact_owner {
        return Ok(None);
    }
    let uid = metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes projected content has no UID"))?;
    let resource_version = metadata
        .resource_version
        .clone()
        .ok_or_else(|| backend("Kubernetes projected content has no resourceVersion"))?;
    Ok(Some(kube::api::Preconditions {
        uid: Some(uid),
        resource_version: Some(resource_version),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContinuationObservationDisposition {
    Live,
    Disposing,
    Incompatible,
}

#[derive(Clone, Copy)]
struct ContinuationObservationEvidence<'a> {
    pod_present: bool,
    pod_deleting: bool,
    cleanup_gate_held: bool,
    expected_uid: Option<&'a str>,
    observed_uid: Option<&'a str>,
    observed_terminating: bool,
    effect_fenced: bool,
    effect_fenced_total_absence: bool,
}

/// Classify the one combined Pod/PVC continuation fact before either the
/// observer or cleanup path interprets it. The cleanup finalizer plus its
/// validated provenance annotation is the provider's durable physical
/// serialization gate; only the aggregate's typed authorization is evidence that
/// source-durability participants completed. A terminating exact claim is the
/// response-loss evidence after that gate was released. Neither a stable name
/// nor matching claim content is authority on its own.
fn continuation_observation_disposition(
    evidence: ContinuationObservationEvidence<'_>,
) -> Result<ContinuationObservationDisposition, RuntimeError> {
    let ContinuationObservationEvidence {
        pod_present,
        pod_deleting,
        cleanup_gate_held,
        expected_uid,
        observed_uid,
        observed_terminating,
        effect_fenced,
        effect_fenced_total_absence,
    } = evidence;
    if expected_uid != observed_uid {
        if !pod_present
            && expected_uid.is_some()
            && observed_uid.is_none()
            && effect_fenced_total_absence
        {
            return Ok(ContinuationObservationDisposition::Live);
        }
        return Ok(ContinuationObservationDisposition::Incompatible);
    }

    if !pod_present && expected_uid.is_some() {
        return if observed_terminating && effect_fenced {
            Ok(ContinuationObservationDisposition::Disposing)
        } else {
            Ok(ContinuationObservationDisposition::Incompatible)
        };
    }

    if pod_present && cleanup_gate_held {
        return if effect_fenced && expected_uid.is_some() {
            Ok(ContinuationObservationDisposition::Disposing)
        } else {
            Err(backend(
                "Kubernetes continuation cleanup gate has no exact fenced claim authority",
            ))
        };
    }

    if pod_present && pod_deleting {
        return if observed_terminating && effect_fenced && expected_uid.is_some() {
            Ok(ContinuationObservationDisposition::Disposing)
        } else {
            Err(backend(
                "Kubernetes Sandbox Pod deletion is indeterminate before continuation cleanup",
            ))
        };
    }

    if observed_terminating {
        return Err(backend(
            "Kubernetes continuation PVC is terminating without exact cleanup evidence",
        ));
    }

    Ok(ContinuationObservationDisposition::Live)
}

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
    /// Live apiserver evidence for the canonical sandbox NetworkPolicy graph.
    /// The same fact drives capability projection and restricted-Pod admission.
    network_policy_attestation: network_policy::Attestation,
    /// Private resident-process port reached only through the authenticated Pod
    /// port-forward subresource. `None` preserves the legacy channel topology.
    pod_channel_port: Option<u16>,
    sandbox_control_forwarder: Option<K8sSandboxControlForwarder>,
    continuation_volume: Option<crate::K8sContinuationVolume>,
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
        // kube's rustls client needs a process-level CryptoProvider; install ring
        // once (idempotent — a prior install by the host is fine).
        install_rustls_crypto_provider();
        let clients = K8sClients::infer().await?;
        let namespace = namespace.into();
        let runtime = Self {
            clients,
            namespace,
            realization_namespace,
            agent_addr,
            owner: None,
            owner_id: crate::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            network_policy_attestation: network_policy::Attestation::new(),
            pod_channel_port: None,
            sandbox_control_forwarder: None,
            continuation_volume: None,
        };
        runtime
            .network_policy_attestation
            .refresh(&runtime.clients.control, &runtime.namespace)
            .await?;
        Ok(runtime)
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
    pub fn with_continuation_volume(mut self, config: crate::K8sContinuationVolume) -> Self {
        self.continuation_volume = Some(config);
        self
    }

    /// A runtime backed by a **lazy** client (no cluster dial), for unit-testing the
    /// builder + Pod-assembly paths; the live `create`/`wait`/… methods still need a
    /// real apiserver (exercised by the gated `k8s_it` integration test).
    #[cfg(test)]
    fn for_test(agent_addr: SocketAddr) -> Self {
        install_rustls_crypto_provider();
        Self {
            clients: K8sClients::for_test(),
            namespace: "default".into(),
            realization_namespace: ContainerRealizationNamespace::from_stable_parts([
                "k8s-test-runtime",
            ])
            .expect("test namespace"),
            agent_addr,
            owner: None,
            owner_id: crate::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            network_policy_attestation: network_policy::Attestation::new(),
            pod_channel_port: None,
            sandbox_control_forwarder: None,
            continuation_volume: None,
        }
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

    fn desired_create_claim(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<Option<PersistentVolumeClaim>, RuntimeError> {
        if !matches!(context.intent, ContainerRealizationIntent::Create)
            || !continuation::claim_required(plan, self.continuation_volume.is_some())
        {
            return Ok(None);
        }
        let effect_fence = context.effect_fence.ok_or_else(|| {
            backend("Kubernetes retained continuation creation requires a durable effect fence")
        })?;
        let runtime_id = self.realization_runtime_id(context.scope)?;
        let config = self
            .continuation_volume
            .as_ref()
            .expect("claim selector requires configured continuation storage");
        let mut claim = continuation::build_claim(&runtime_id, config)?;
        // The ordinary digest owns the immutable PVC shape. The aggregate fence
        // is stamped afterwards so a lease-expiry renewal does not create a
        // second filesystem identity; admission below still compares the exact
        // stable effect identity and monotonic expiry.
        stamp_realization(&mut claim)?;
        stamp_effect_fence(&mut claim, effect_fence);
        Ok(Some(claim))
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
        continuation::create_claim_admission(
            expected_fence,
            Some(continuation::CreateClaimObservation {
                effect_fence: observed_fence.as_ref(),
                uid_present: observed.metadata.uid.is_some(),
                terminating: observed.metadata.deletion_timestamp.is_some(),
                realization_matches: realization::realization_digest_matches(desired, observed)?,
            }),
        )
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
        let observed = self
            .persistent_volume_claims()
            .get_opt(name)
            .await
            .map_err(backend)?;
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
        let Some(pod) = self.pods().get_opt(&name).await.map_err(backend)? else {
            return Ok(Vec::new());
        };
        if pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(crate::MANAGED_SANDBOX_LABEL))
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
        if matches!(plan.network, crate::NetworkMode::None) {
            self.probe_ready().await?;
        }
        admit_network(&plan.network, self.network_policy_attestation.current())
    }

    async fn observed_continuation_disposition(
        &self,
        pod_name: &str,
        pod: Option<&Pod>,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
        effect_fenced: bool,
        effect_fenced_total_absence: bool,
    ) -> Result<ContinuationObservationDisposition, RuntimeError> {
        let expected_claim_uid = match runtime_handle {
            Some(pc::ContainerContinuationHandle::KubernetesContinuation { claim_uid }) => {
                Some(claim_uid.as_str())
            }
            Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
                claim_uid, ..
            }) => claim_uid.as_deref(),
            Some(pc::ContainerContinuationHandle::HostBindRestoration(_)) => {
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
            pod_present: pod.is_some(),
            pod_deleting: pod.is_some_and(|pod| pod.metadata.deletion_timestamp.is_some()),
            cleanup_gate_held,
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

    async fn cleanup_projected_kind<K>(
        &self,
        api: &Api<K>,
        pod_name: &str,
        pod_uid: &str,
        attempt_id: &str,
    ) -> Result<(), RuntimeError>
    where
        K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + Resource<DynamicType = ()>,
    {
        let selector = format!(
            "awaken-cfg-owner={pod_name},{}={}",
            crate::SANDBOX_ATTEMPT_LABEL,
            k8s_attempt_label(attempt_id),
        );
        let params = ListParams::default().labels(&selector);
        for object in api.list(&params).await.map_err(backend)? {
            let Some(preconditions) = projected_content_delete_preconditions(
                object.meta(),
                pod_name,
                pod_uid,
                attempt_id,
            )?
            else {
                continue;
            };
            let name = object
                .meta()
                .name
                .as_deref()
                .ok_or_else(|| backend("Kubernetes projected content has no name"))?;
            match api
                .delete(name, &DeleteParams::default().preconditions(preconditions))
                .await
            {
                Ok(_) => {}
                Err(error) if api_not_found(&error) => {}
                Err(error) => return Err(backend(error)),
            }
        }
        Ok(())
    }

    async fn cleanup_projected_content(
        &self,
        pod_name: &str,
        pod_uid: &str,
        attempt_id: &str,
    ) -> Result<(), RuntimeError> {
        let configmaps = self.configmaps();
        self.cleanup_projected_kind(&configmaps, pod_name, pod_uid, attempt_id)
            .await?;
        let secrets = self.secrets();
        self.cleanup_projected_kind(&secrets, pod_name, pod_uid, attempt_id)
            .await
    }

    async fn await_exact_pod_deleted(
        &self,
        pod_name: &str,
        pod_uid: &str,
    ) -> Result<(), RuntimeError> {
        const DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        const DELETE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
        let deadline = tokio::time::Instant::now() + DELETE_TIMEOUT;
        loop {
            match self.pods().get(pod_name).await {
                Err(error) if api_not_found(&error) => return Ok(()),
                Err(error) => return Err(backend(error)),
                Ok(pod) if pod.metadata.uid.as_deref() != Some(pod_uid) => return Ok(()),
                Ok(_) if tokio::time::Instant::now() >= deadline => {
                    return Err(backend(format!(
                        "Kubernetes Sandbox Pod `{pod_name}` incarnation `{pod_uid}` was not deleted within {}s",
                        DELETE_TIMEOUT.as_secs()
                    )));
                }
                Ok(_) => tokio::time::sleep(DELETE_POLL).await,
            }
        }
    }

    async fn begin_exact_pod_delete(
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
        match self.pods().delete(pod_name, &params).await {
            Ok(_) => {}
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        }

        // Explicit cleanup is authorized only while the exact owner Pod still
        // exists and only for participants stamped by the same attempt. Once
        // that Pod is absent (or its stable name points at a new UID), native
        // ownerReference GC is the sole deletion authority.
        match self.pods().get(pod_name).await {
            Ok(observed) if observed.metadata.uid.as_deref() == Some(expected_uid) => {
                if let Some(attempt_id) = attempt_id {
                    self.cleanup_projected_content(pod_name, expected_uid, attempt_id)
                        .await?;
                }
            }
            Ok(_) => {}
            Err(error) if api_not_found(&error) => {}
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
        self.begin_exact_pod_delete(pod, expected_uid, expected_resource_version, attempt_id)
            .await?;
        self.await_exact_pod_deleted(pod_name, expected_uid).await
    }

    async fn remove_bound(
        &self,
        container_id: &str,
        persisted_pod_uid: Option<&str>,
        persisted_claim_uid: Option<&str>,
        authorization: Option<&pc::SandboxDisposalAuthorization>,
    ) -> Result<(), RuntimeError> {
        let effect_fenced = authorization.is_some();
        let pods = self.pods();
        let observed_pod = match pods.get(container_id).await {
            Ok(pod) => Some(pod),
            Err(error) if api_not_found(&error) => None,
            Err(error) => return Err(backend(error)),
        };
        let pod_claim_uid = observed_pod
            .as_ref()
            .and_then(continuation::bound_claim_uid)
            .map(str::to_owned);
        match (&observed_pod, persisted_pod_uid) {
            (None, _) => {}
            (Some(_), None) => {
                return Err(backend(
                    "Kubernetes Sandbox deletion requires a persisted Pod UID",
                ));
            }
            (Some(pod), Some(expected)) if pod.metadata.uid.as_deref() != Some(expected) => {
                return Err(backend(
                    "Kubernetes Sandbox Pod incarnation changed before deletion",
                ));
            }
            (Some(_), Some(_)) => {}
        }
        if persisted_pod_uid.is_some()
            && observed_pod.is_some()
            && persisted_claim_uid != pod_claim_uid.as_deref()
        {
            return Err(backend(
                "Sandbox Pod continuation PVC differs from its persisted incarnation evidence",
            ));
        }
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
                effect_fenced,
                effect_fenced && persisted_claim_uid.is_some(),
            )
            .await?;
        if continuation == ContinuationObservationDisposition::Incompatible {
            return Err(backend(
                "Kubernetes Sandbox continuation state differs from its durable handle",
            ));
        }

        let claims = self.persistent_volume_claims();
        let Some(observed_pod) = observed_pod else {
            if let Some(uid) = expected_claim_uid {
                let pending =
                    continuation::absent_pod_claim_deletion(&claims, container_id, uid).await?;
                if let Some(pending) = pending.as_ref() {
                    continuation::await_claim_deletion(&claims, pending).await?;
                }
            }
            return Ok(());
        };

        if continuation == ContinuationObservationDisposition::Disposing
            && observed_pod.metadata.deletion_timestamp.is_some()
            && !continuation::pod_cleanup_gate_held(&observed_pod)
        {
            // Cause/effect row KCO1/K3: PVC termination plus exact deleting Pod
            // without our finalizer can only be reached after this cleanup
            // owner released the gate (the replace response may have been
            // lost). No mutation remains authorized: await exact A, then
            // re-read and await exact P. A foreign replacement UID ends only
            // the A wait and still cannot redirect the claim fence.
            let pod_uid = persisted_pod_uid.expect("disposing Pod has persisted UID");
            self.await_exact_pod_deleted(container_id, pod_uid).await?;
            if let Some(uid) = expected_claim_uid {
                let pending =
                    continuation::absent_pod_claim_deletion(&claims, container_id, uid).await?;
                if let Some(pending) = pending.as_ref() {
                    continuation::await_claim_deletion(&claims, pending).await?;
                }
            }
            return Ok(());
        }

        // Cause/effect order for a retained source is fixed: exact Pod A first
        // acquires the durable cleanup finalizer by UID/resourceVersion CAS;
        // only that winner may establish P's deletion intent. Rebuild DELETE
        // winning the CAS leaves P untouched. Once P is terminating, begin A's
        // exact deletion, remove only our finalizer, then await both objects.
        let mut pending_pod_cleanup = if expected_claim_uid.is_some() {
            let authorization = authorization.ok_or_else(|| {
                backend(
                    "Kubernetes retained continuation disposal requires aggregate authorization authority",
                )
            })?;
            Some(
                continuation::acquire_pod_cleanup_gate(
                    &pods,
                    &observed_pod,
                    persisted_pod_uid.expect("retained bound Pod has persisted UID"),
                    authorization,
                )
                .await?,
            )
        } else {
            None
        };
        let pending_claim_deletion = if let Some(uid) = expected_claim_uid {
            // Any API error may be response loss after PVC DELETE acceptance.
            // Retain the Pod finalizer and fail so a root retry can classify P's
            // deletionTimestamp; releasing the gate here would reopen the
            // stable name while claim disposal may already be irreversible.
            continuation::initiate_claim_deletion(&claims, container_id, uid).await?
        } else {
            None
        };

        if let Some(pending) = pending_pod_cleanup.as_mut() {
            let current = pods.get(container_id).await.map_err(backend)?;
            *pending = continuation::acquire_pod_cleanup_gate(
                &pods,
                &current,
                persisted_pod_uid.expect("retained bound Pod has persisted UID"),
                authorization.expect("retained cleanup has authorization authority"),
            )
            .await?;
        }
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
        self.begin_exact_pod_delete(pod, &incarnation.identity, resource_version, attempt_id)
            .await?;
        if let Some(pending) = pending_pod_cleanup.as_ref() {
            continuation::release_pod_cleanup_gate(&pods, pending).await?;
        }
        self.await_exact_pod_deleted(container_id, &incarnation.identity)
            .await?;
        if let Some(pending) = pending_claim_deletion.as_ref() {
            continuation::await_claim_deletion(&claims, pending).await?;
        }
        Ok(())
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
        );
        let labels = pod.metadata.labels.get_or_insert_with(Default::default);
        labels.insert(crate::MANAGED_SANDBOX_LABEL.to_string(), "1".to_string());
        labels.insert(
            crate::RUNTIME_OWNER_LABEL.to_string(),
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
            volumes.push(Volume {
                name: memory::PROJECTION_VOLUME.into(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: memory::PROJECTION_VOLUME.into(),
                mount_path: memory::PROJECTION_ROOT.into(),
                read_only: Some(true),
                ..Default::default()
            });
            memory_projector_mounts.push(VolumeMount {
                name: memory::PROJECTION_VOLUME.into(),
                mount_path: memory::PROJECTION_ROOT.into(),
                read_only: Some(false),
                ..Default::default()
            });
            sidecars.push(Container {
                name: memory::PROJECTOR.into(),
                image: Some(plan.image.clone()),
                command: Some(crate::environment_keepalive_command()),
                env: Some(vec![EnvVar {
                    name: memory::PROJECTION_FENCE_ENV.into(),
                    value: Some(memory::projection_fence_value(effect_fence)),
                    value_from: None,
                }]),
                volume_mounts: Some(memory_projector_mounts),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
        }

        let continuation_subpaths = append_writable_and_cache_volumes(
            plan,
            continuation_claim,
            &mut volumes,
            &mut agent_mounts,
        );
        continuation::append_init_container(plan, &continuation_subpaths, &mut init_containers);
        live_inputs::append_projection(plan, &mut volumes, &mut agent_mounts, &mut sidecars);
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
            if crate::live_inputs::manages(bind) {
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

        let agent_command = effect_fence
            .filter(|_| !plan.memory_mounts.is_empty())
            .map_or_else(
                || plan.command.clone(),
                |effect_fence| memory::gated_agent_command(&plan.command, effect_fence),
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
                    fs_group: Some(10001),
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
                image_pull_secrets: (!image_pull_secrets.is_empty()).then(|| {
                    image_pull_secrets
                        .iter()
                        .map(|name| LocalObjectReference { name: name.clone() })
                        .collect()
                }),
                ..Default::default()
            }),
            status: None,
        }
    }
}

/// Host side of the reverse-dial: bind the rendezvous and accept the Pod's outbound
/// connection, returning it as the agent channel. Extracted so it is testable with a
/// stand-in dialer (no cluster).
#[cfg(test)]
mod tests;
