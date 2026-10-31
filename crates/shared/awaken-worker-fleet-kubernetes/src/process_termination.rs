// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Independent observation of one frozen, private Pod process domain.
//!
//! A Worker process incarnation is one container instance: the bound running
//! container's runtime ID and start time inside the same Pod UID. A
//! `restartPolicy: Always` Pod may restart that container in place; the new
//! container is a new incarnation of the same Pod-scoped Worker ID, and the old
//! one is settled from the exact terminated state that names its container ID
//! (`state` or `lastState`). When neither names it but the same Pod UID already
//! started a newer instance of the same container name, the old instance is
//! Superseded: kubelet runs one instance per container name, so it stopped,
//! with no exit code. Otherwise its evidence is lost and observation stays
//! Unknown; no exit code is ever inferred.
//!
//! This evidence covers local processes only. It does not discharge a Dispatch
//! attempt, prove external requests stopped, or grant replay authority. In
//! particular, Pod absence, deletion and phase Failed are not stop receipts.

use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde::{Deserialize, Serialize};

use crate::{api_not_found, backend};
use awaken_container_contract::RuntimeError;
use k8s_openapi::api::core::v1::PodSpec;

mod stop;
pub use stop::{EndedRetainedWorkerPod, PodProcessStopper, ProcessEvidence};

/// Installed by the trusted Worker deployment before execution admission, and
/// retained until the exact attempt evidence has been durably consumed. This is
/// not the Session continuation-cleanup finalizer and must not release its PVCs.
pub const WORKER_PROCESS_EVIDENCE_FINALIZER: &str = "awaken.dev/worker-process-evidence";

/// The Sandbox continuation-cleanup finalizer. It retains a Session Pod's
/// process evidence as well; this observer never releases it.
pub const CONTINUATION_CLEANUP_FINALIZER: &str = "awaken.dev/continuation-cleanup";

/// One shared predicate for Pod namespace shapes that invalidate a private
/// Sandbox boundary. `None`, explicit `false`, and an explicit empty ephemeral
/// container list are equivalent Kubernetes API-default representations.
pub fn has_forbidden_sandbox_namespace_shape(spec: &PodSpec) -> bool {
    spec.host_network == Some(true)
        || spec.host_pid == Some(true)
        || spec.host_ipc == Some(true)
        || spec.share_process_namespace == Some(true)
        || spec
            .ephemeral_containers
            .as_ref()
            .is_some_and(|containers| !containers.is_empty())
}

/// Freeze this identity before admitting effects. Namespace/name are lookup
/// coordinates only; UID, node, container ID and start time identify execution.
/// Deserializing a binding never establishes termination or execution authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodProcessBinding {
    namespace: String,
    name: String,
    uid: String,
    node: String,
    containers: Vec<ContainerProcessBinding>,
    /// Legacy/unretained coordinates cannot acquire stop-command capability by
    /// adding a finalizer after the original admission.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    retained: bool,
    #[serde(
        default = "worker_evidence_finalizer",
        skip_serializing_if = "is_worker_evidence_finalizer"
    )]
    retention_finalizer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    init_containers: Vec<ContainerProcessBinding>,
    /// The Pod is `restartPolicy: Always`: its bound containers may be replaced
    /// in place, so each is observed by exact container ID in `state` or
    /// `lastState`. Absence preserves the original Never-policy contract.
    /// Bindings persisted before container-instance identity carry the same
    /// fact and settle by the same container-ID rule (their IDs were exact).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    restartable: bool,
}

fn worker_evidence_finalizer() -> String {
    WORKER_PROCESS_EVIDENCE_FINALIZER.into()
}
fn is_worker_evidence_finalizer(value: &str) -> bool {
    value == WORKER_PROCESS_EVIDENCE_FINALIZER
}

impl PodProcessBinding {
    /// Match the provider coordinate to the authenticated Worker, not a name
    /// alone. The full node/container binding is still checked by the observer.
    pub fn matches_pod(&self, namespace: &str, name: &str, uid: &str) -> bool {
        self.namespace == namespace && self.name == name && self.uid == uid
    }

    pub fn evidence_was_retained(&self) -> bool {
        self.retained
    }
    pub fn worker_evidence_was_retained(&self) -> bool {
        self.retained && self.retention_finalizer == WORKER_PROCESS_EVIDENCE_FINALIZER
    }

    /// The Worker incarnation this binding identifies: its container instance.
    /// See [`container_incarnation`].
    pub fn incarnation(&self) -> String {
        container_incarnation(
            self.containers
                .iter()
                .map(|container| (container.name.as_str(), container.container_id.as_str())),
        )
    }

    /// Whether this binding is the process of `incarnation` in Pod `pod_uid`.
    /// A historical incarnation equal to the Pod UID predates container
    /// identity and names whichever single process the Pod was bound to.
    pub fn belongs_to_incarnation(&self, pod_uid: &str, incarnation: &str) -> bool {
        self.uid == pod_uid && (incarnation == pod_uid || self.incarnation() == incarnation)
    }
}

/// The incarnation token for one container instance set: the lowercase hex
/// runtime ID of a single container (`containerd://<hex>` -> `<hex>`), else a
/// 32-hex digest over every `(name, container ID)` pair in name order. The
/// token is lowercase hex, so it can never equal a Pod UID (which has dashes).
pub(crate) fn container_incarnation<'a>(
    containers: impl Iterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut containers: Vec<_> = containers.collect();
    containers.sort();
    if let [(_, id)] = containers.as_slice() {
        let hex = id.rsplit("://").next().unwrap_or_default();
        if !hex.is_empty()
            && hex.len() <= 128
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return hex.to_owned();
        }
    }
    let encoded = serde_json::to_vec(&containers).expect("container coordinates serialize");
    blake3::hash(&encoded).to_hex().as_str()[..32].to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerProcessBinding {
    name: String,
    container_id: String,
    started_at: Time,
}

/// An observation obtained from this adapter's authenticated Kubernetes client.
/// Deliberately not deserializable: caller-supplied JSON is not provider evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PodProcessTermination {
    Unknown,
    /// Exact non-restarting container terminated. This variant is an observation,
    /// not a whole-attempt acknowledgement or proof about remote participants.
    Observed(Box<ObservedPodProcessExit>),
    /// A restartable binding's processes stopped without an exact exit for
    /// every one: the same Pod UID started a newer instance of each bound
    /// container name whose exit was lost. Proves the bound processes are not
    /// running; carries no exit code and nothing about external effects.
    Superseded(Box<SupersededPodProcess>),
}

/// Subject-bound evidence minted only by authenticated observation. Private
/// fields and no Deserialize implementation prevent arbitrary receipt input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedPodProcessExit {
    binding: PodProcessBinding,
    resource_version: String,
    exits: Vec<ObservedContainerExit>,
}

/// The exact runtime exit of one bound container, as kubelet reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObservedContainerExit {
    name: String,
    container_id: String,
    exit_code: i32,
    reason: String,
    finished_at: Time,
}

impl ObservedContainerExit {
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl ObservedPodProcessExit {
    pub fn binding(&self) -> &PodProcessBinding {
        &self.binding
    }

    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    pub fn exits(&self) -> &[ObservedContainerExit] {
        &self.exits
    }

    /// Durable evidence for the Registry exit receipt: the exact container
    /// exits (ID, code, reason, finish time). It depends only on those facts,
    /// so a repeated observation of the same exit yields the same reference.
    pub fn evidence_ref(&self) -> String {
        format!(
            "k8s-container-exit:v1:{}",
            serde_json::to_string(&self.exits).expect("container exits serialize")
        )
    }
}

/// One bound container instance a newer instance of the same name replaced in
/// the same Pod UID. Only the bound coordinates are evidence: the successor may
/// restart again, and the receipt must stay the same across observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SupersededContainer {
    name: String,
    container_id: String,
    started_at: Time,
}

/// Subject-bound end of a restartable binding minted only by authenticated
/// observation: every bound container either exited exactly or was superseded,
/// and at least one was superseded. No Deserialize implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupersededPodProcess {
    binding: PodProcessBinding,
    resource_version: String,
    exits: Vec<ObservedContainerExit>,
    superseded: Vec<SupersededContainer>,
}

impl SupersededPodProcess {
    pub fn binding(&self) -> &PodProcessBinding {
        &self.binding
    }

    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    /// Durable evidence for the Registry receipt. It names only the bound
    /// instances (exact exits and superseded coordinates), so a repeated
    /// observation yields the same reference even after further restarts.
    pub fn evidence_ref(&self) -> String {
        format!(
            "k8s-container-superseded:v1:{}",
            serde_json::to_string(&(&self.exits, &self.superseded))
                .expect("container ends serialize")
        )
    }
}

/// Read-only process observer. Unlike Sandbox runtime startup, construction does
/// not create NetworkPolicies or other execution resources. The Coordinator may
/// share this exact authenticated client with its Pod TokenReview adapter.
pub struct PodProcessObserver {
    pods: kube::Api<Pod>,
    namespace: String,
}

impl PodProcessObserver {
    pub fn new(client: kube::Client, namespace: &str) -> Result<Self, RuntimeError> {
        if namespace.trim().is_empty() || namespace.trim() != namespace {
            return Err(backend("Pod process observer requires an exact namespace"));
        }
        Ok(Self {
            pods: kube::Api::namespaced(client, namespace),
            namespace: namespace.into(),
        })
    }

    /// Retain this registered UID before Ready; never adopt a deleting or
    /// replacement Pod. This creates metadata only, not a process binding.
    pub async fn retain_pod_process_evidence(
        &self,
        name: &str,
        uid: &str,
    ) -> Result<(), RuntimeError> {
        use crate::pod_metadata::{ExactFinalizerAdmission, update_exact_pod_finalizer};
        update_exact_pod_finalizer(
            &self.pods,
            name,
            WORKER_PROCESS_EVIDENCE_FINALIZER,
            true,
            &|| Ok(()),
            &|pod| {
                if uid.is_empty()
                    || pod.metadata.uid.as_deref() != Some(uid)
                    || pod.metadata.deletion_timestamp.is_some()
                {
                    return Err(backend("Worker retention requires the original live Pod"));
                }
                Ok(if evidence_is_retained(pod) {
                    ExactFinalizerAdmission::Complete
                } else {
                    ExactFinalizerAdmission::Retain
                })
            },
            &|_| {},
        )
        .await
    }
    /// Obtain the exact running process coordinate through the existing control
    /// client. The caller must persist it before allowing any external effects.
    /// Every ordinary container is included. Only a `restartPolicy: Never` Pod
    /// is admitted: an in-place restart would put a second process under the
    /// same UID, Registry incarnation and Create ledger. Init, ephemeral and
    /// independently restartable containers remain outside this proof domain.
    pub async fn bind_pod_process(
        &self,
        name: &str,
        expected_uid: &str,
    ) -> Result<PodProcessBinding, RuntimeError> {
        self.observed_pod_binding(name, expected_uid)
            .await
            .map(|(_, binding)| binding)
    }

    /// The incarnation token of the Pod's running container instance, if the
    /// Pod could be bound once its evidence is retained (same checks as
    /// [`Self::bind_pod_process`] except retention). Registration uses it to
    /// name the incarnation and to refuse an unbindable Pod before retaining.
    pub async fn admissible_incarnation(
        &self,
        name: &str,
        expected_uid: &str,
    ) -> Result<String, RuntimeError> {
        if expected_uid.is_empty() {
            return Err(backend("Pod process binding requires an authenticated UID"));
        }
        let pod = self.pods.get(name).await.map_err(backend)?;
        bind_checked(&self.namespace, name, expected_uid, &pod, false)
            .map(|binding| binding.incarnation())
    }

    async fn observed_pod_binding(
        &self,
        name: &str,
        expected_uid: &str,
    ) -> Result<(Pod, PodProcessBinding), RuntimeError> {
        if expected_uid.is_empty() {
            return Err(backend("Pod process binding requires an authenticated UID"));
        }
        let pod = self.pods.get(name).await.map_err(backend)?;
        let binding = bind(&self.namespace, name, expected_uid, &pod)?;
        Ok((pod, binding))
    }

    /// Observe a relay destination from the same Pod and exact running-process
    /// proof as ordinary Worker binding. No second Pod lookup may pair a UID
    /// from one observation with an address from another.
    pub async fn relay_ip_for(
        &self,
        name: &str,
        expected_uid: &str,
    ) -> Result<std::net::IpAddr, RuntimeError> {
        let (pod, binding) = self.observed_pod_binding(name, expected_uid).await?;
        if !binding.worker_evidence_was_retained() {
            return Err(backend("Worker relay Pod evidence is not retained"));
        }
        pod.status
            .as_ref()
            .and_then(|status| status.pod_ip.as_deref())
            .ok_or_else(|| backend("Worker relay Pod address is missing"))?
            .parse()
            .map_err(|_| backend("Worker relay Pod address is invalid"))
    }

    /// Can be called by a different live owner after the original process died.
    /// A 404 retains uncertainty; lost status cannot be reconstructed from a Pod
    /// name, a timeout, or the replacement Pod's status. Transport/RBAC failures
    /// remain errors so the existing caller can retry observation, not execution.
    pub async fn observe_pod_process_termination(
        &self,
        binding: &PodProcessBinding,
    ) -> Result<PodProcessTermination, RuntimeError> {
        if binding.namespace != self.namespace {
            return Err(backend("Pod process binding belongs to another namespace"));
        }
        match self.pods.get(&binding.name).await {
            Ok(pod) => Ok(observe(binding, &pod)),
            Err(error) if api_not_found(&error) => Ok(PodProcessTermination::Unknown),
            Err(error) => Err(backend(error)),
        }
    }
}

fn process_statuses(pod: &Pod) -> Option<Vec<&ContainerStatus>> {
    let spec = pod.spec.as_ref()?;
    // Under Always a container may restart in place: its identity is then its
    // container ID, not a zero restart count. The frozen policy is compared
    // by the observation that consumes these statuses.
    let restartable = spec.restart_policy.as_deref() == Some("Always");
    if !matches!(spec.restart_policy.as_deref(), Some("Never" | "Always"))
        || has_forbidden_sandbox_namespace_shape(spec)
        || spec.containers.is_empty()
        || spec
            .volumes
            .as_ref()
            .is_some_and(|v| v.iter().any(|v| v.host_path.is_some()))
    {
        return None;
    }
    let status = pod.status.as_ref()?;
    completed_init_bindings(pod)?;
    if status
        .ephemeral_container_statuses
        .as_ref()
        .is_some_and(|v| !v.is_empty())
    {
        return None;
    }
    let statuses = status.container_statuses.as_ref()?;
    if statuses.len() != spec.containers.len() {
        return None;
    }
    let mut result = Vec::with_capacity(statuses.len());
    for container in &spec.containers {
        if !private_container(container)
            || result
                .iter()
                .any(|prior: &&ContainerStatus| prior.name == container.name)
        {
            return None;
        }
        let mut matching = statuses
            .iter()
            .filter(|status| status.name == container.name);
        let status = matching.next()?;
        if matching.next().is_some() || (!restartable && status.restart_count != 0) {
            return None;
        }
        result.push(status);
    }
    result.sort_by(|left, right| left.name.cmp(&right.name));
    Some(result)
}

fn coordinate(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

// One retention catalog shared by pre-entry binding and later stop admission.
// The Create guard already retains the original Sandbox until its Resource
// owner joins terminal disposal; reusing it grants no Worker-release authority.
const PROCESS_RETENTION_FINALIZERS: [&str; 3] = [
    WORKER_PROCESS_EVIDENCE_FINALIZER,
    CONTINUATION_CLEANUP_FINALIZER,
    awaken_container_contract::ORIGINAL_GUARD_FINALIZER,
];

fn retention_finalizer(pod: &Pod) -> Option<&str> {
    let finalizers = pod.metadata.finalizers.as_ref()?;
    PROCESS_RETENTION_FINALIZERS
        .into_iter()
        .find(|candidate| finalizers.iter().any(|value| value == candidate))
}
fn evidence_is_retained(pod: &Pod) -> bool {
    retention_finalizer(pod).is_some()
}
fn binding_retention_is_present(binding: &PodProcessBinding, pod: &Pod) -> bool {
    PROCESS_RETENTION_FINALIZERS.contains(&binding.retention_finalizer.as_str())
        && pod
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|values| values.contains(&binding.retention_finalizer))
}

fn private_container(container: &k8s_openapi::api::core::v1::Container) -> bool {
    let Some(security) = &container.security_context else {
        return false;
    };
    let Some(capabilities) = &security.capabilities else {
        return false;
    };
    !container.name.is_empty()
        && container.restart_policy.is_none()
        && security.privileged != Some(true)
        && security.allow_privilege_escalation == Some(false)
        && capabilities
            .drop
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "ALL"))
        && capabilities
            .add
            .as_ref()
            .is_none_or(|values| values.is_empty())
}

fn completed_init_bindings(pod: &Pod) -> Option<Vec<ContainerProcessBinding>> {
    let spec = pod.spec.as_ref()?;
    let containers = spec.init_containers.as_deref().unwrap_or_default();
    let statuses = pod
        .status
        .as_ref()?
        .init_container_statuses
        .as_deref()
        .unwrap_or_default();
    if containers.len() != statuses.len() {
        return None;
    }
    let mut bindings = Vec::new();
    for container in containers {
        if !private_container(container)
            || bindings
                .iter()
                .any(|prior: &ContainerProcessBinding| prior.name == container.name)
        {
            return None;
        }
        let mut matches = statuses
            .iter()
            .filter(|status| status.name == container.name);
        let status = matches.next()?;
        if matches.next().is_some() || status.restart_count != 0 {
            return None;
        }
        let state = status.state.as_ref()?;
        if state.running.is_some() || state.waiting.is_some() {
            return None;
        }
        let exit = state.terminated.as_ref()?;
        if exit.reason.as_deref() != Some("Completed")
            || exit.exit_code != 0
            || exit.finished_at.as_ref()? < exit.started_at.as_ref()?
        {
            return None;
        }
        let id = coordinate(status.container_id.as_deref())?;
        if exit.container_id.as_deref() != Some(&id) {
            return None;
        }
        bindings.push(ContainerProcessBinding {
            name: status.name.clone(),
            container_id: id,
            started_at: exit.started_at.clone()?,
        });
    }
    bindings.sort_by(|left, right| left.name.cmp(&right.name));
    Some(bindings)
}

fn bind(
    namespace: &str,
    name: &str,
    expected_uid: &str,
    pod: &Pod,
) -> Result<PodProcessBinding, RuntimeError> {
    bind_checked(namespace, name, expected_uid, pod, true)
}

fn bind_checked(
    namespace: &str,
    name: &str,
    expected_uid: &str,
    pod: &Pod,
    require_restartable_retention: bool,
) -> Result<PodProcessBinding, RuntimeError> {
    let binding = (|| {
        if pod.metadata.deletion_timestamp.is_some()
            || pod.metadata.namespace.as_deref() != Some(namespace)
            || pod.metadata.name.as_deref() != Some(name)
            || expected_uid.is_empty()
            || pod.metadata.uid.as_deref() != Some(expected_uid)
            || pod.status.as_ref()?.phase.as_deref() != Some("Running")
        {
            return None;
        }
        let containers = process_statuses(pod)?
            .into_iter()
            .map(|status| {
                let state = status.state.as_ref()?;
                if state.terminated.is_some() || state.waiting.is_some() {
                    return None;
                }
                Some(ContainerProcessBinding {
                    name: status.name.clone(),
                    container_id: coordinate(status.container_id.as_deref())?,
                    started_at: state.running.as_ref()?.started_at.clone()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        // The bound running containers are this incarnation. Under Always a
        // later container is another incarnation whose predecessor's exit
        // survives only in `lastState`, so its evidence must already be retained.
        let restartable = pod.spec.as_ref()?.restart_policy.as_deref() == Some("Always");
        if restartable && require_restartable_retention && !evidence_is_retained(pod) {
            return None;
        }
        Some(PodProcessBinding {
            namespace: coordinate(Some(namespace))?,
            name: coordinate(Some(name))?,
            uid: coordinate(pod.metadata.uid.as_deref())?,
            node: coordinate(pod.spec.as_ref()?.node_name.as_deref())?,
            containers,
            retained: evidence_is_retained(pod),
            retention_finalizer: retention_finalizer(pod)
                .unwrap_or(WORKER_PROCESS_EVIDENCE_FINALIZER)
                .into(),
            init_containers: completed_init_bindings(pod)?,
            restartable,
        })
    })();
    binding.ok_or_else(|| backend("Pod has no admissible private process binding"))
}

/// One exact process-set comparison for observation and stop admission. Neither
/// a copied Pod name nor replacement container statuses can redirect a command.
fn matching_statuses<'a>(
    binding: &PodProcessBinding,
    pod: &'a Pod,
) -> Option<Vec<&'a ContainerStatus>> {
    if completed_init_bindings(pod)? != binding.init_containers {
        return None;
    }
    let statuses = process_statuses(pod)?;
    let expected_policy = if binding.restartable {
        "Always"
    } else {
        "Never"
    };
    if pod.spec.as_ref()?.restart_policy.as_deref() != Some(expected_policy)
        || pod.metadata.namespace.as_deref() != Some(binding.namespace.as_str())
        || pod.metadata.name.as_deref() != Some(binding.name.as_str())
        || pod.metadata.uid.as_deref() != Some(binding.uid.as_str())
        || pod.spec.as_ref()?.node_name.as_deref() != Some(binding.node.as_str())
        || statuses.len() != binding.containers.len()
    {
        return None;
    }
    for (status, expected) in statuses.iter().zip(&binding.containers) {
        let state = status.state.as_ref()?;
        let started = match (&state.running, &state.terminated, &state.waiting) {
            (Some(running), None, None) => running.started_at.as_ref()?,
            (None, Some(terminated), None) => terminated.started_at.as_ref()?,
            _ => return None,
        };
        if status.name != expected.name
            || status.container_id.as_deref() != Some(expected.container_id.as_str())
            || started != &expected.started_at
        {
            return None;
        }
    }
    Some(statuses)
}

/// One bound container's fate in a restartable Pod, by exact container ID,
/// within the same Pod UID and container name (the Pod fence is checked first):
///
/// | Observation of the bound instance                        | Fate         |
/// |----------------------------------------------------------|--------------|
/// | `state` or `lastState` names it with an exact exit       | `Exited`     |
/// | it is the running instance                               | `Running`    |
/// | a newer instance of the name (other ID, later startedAt) | `Superseded` |
/// | otherwise (synthetic status only, no later start)        | `Lost`       |
///
/// The Pod being gone or carrying another UID is not a fate: nothing about the
/// bound instance can be observed and the whole binding stays Unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ContainerFate {
    /// The bound container instance is still the running one.
    Running,
    /// `state` or `lastState` names the bound container with an exact exit.
    Exited(ObservedContainerExit),
    /// Its exit is no longer reported, but kubelet already started a newer
    /// instance of the same name in the same Pod UID. Kubelet runs one
    /// instance per name, so the bound one stopped; no exit code is known.
    Superseded(SupersededContainer),
    /// Neither: the exit evidence is lost and must never be inferred.
    Lost,
}

/// The container instances a status reports for one name: the current one
/// (running or terminated) and the last terminated one, each with its start.
fn reported_instances(status: &ContainerStatus) -> impl Iterator<Item = (&str, &Time)> {
    let state = status.state.as_ref();
    let current_start = state.and_then(|state| {
        state
            .running
            .as_ref()
            .and_then(|running| running.started_at.as_ref())
            .or_else(|| {
                state
                    .terminated
                    .as_ref()
                    .and_then(|terminated| terminated.started_at.as_ref())
            })
    });
    let current = status.container_id.as_deref().zip(current_start);
    let terminated = [
        state.and_then(|state| state.terminated.as_ref()),
        status
            .last_state
            .as_ref()
            .and_then(|state| state.terminated.as_ref()),
    ]
    .into_iter()
    .flatten()
    .filter_map(|terminated| {
        terminated
            .container_id
            .as_deref()
            .zip(terminated.started_at.as_ref())
    });
    current.into_iter().chain(terminated)
}

/// A newer instance of the bound container's name has started: another
/// container ID whose start is strictly later than the bound instance's.
fn newer_instance_started(expected: &ContainerProcessBinding, status: &ContainerStatus) -> bool {
    reported_instances(status)
        .any(|(id, started)| id != expected.container_id && started > &expected.started_at)
}

/// Node-loss controllers may synthesize Failed/ContainerStatusUnknown. Require
/// an exact runtime exit of exactly the bound container, not a phase or code.
fn exact_exit(
    expected: &ContainerProcessBinding,
    terminated: &k8s_openapi::api::core::v1::ContainerStateTerminated,
) -> Option<ObservedContainerExit> {
    let reason = terminated.reason.as_deref()?;
    if !matches!(reason, "Completed" | "Error" | "OOMKilled")
        || terminated.container_id.as_deref() != Some(expected.container_id.as_str())
        || terminated.started_at.as_ref()? != &expected.started_at
        || terminated.finished_at.as_ref()? < terminated.started_at.as_ref()?
    {
        return None;
    }
    Some(ObservedContainerExit {
        name: expected.name.clone(),
        container_id: expected.container_id.clone(),
        exit_code: terminated.exit_code,
        reason: reason.to_owned(),
        finished_at: terminated.finished_at.clone()?,
    })
}

/// The Pod-UID fence plus the per-container fate of a restartable binding.
/// `None` means the Pod is not this binding's Pod (UID, node, policy, init
/// set, private shape or container names differ): nothing can be observed.
fn restartable_fates(binding: &PodProcessBinding, pod: &Pod) -> Option<Vec<ContainerFate>> {
    if !binding.restartable || completed_init_bindings(pod)? != binding.init_containers {
        return None;
    }
    let statuses = process_statuses(pod)?;
    if pod.spec.as_ref()?.restart_policy.as_deref() != Some("Always")
        || pod.metadata.namespace.as_deref() != Some(binding.namespace.as_str())
        || pod.metadata.name.as_deref() != Some(binding.name.as_str())
        || pod.metadata.uid.as_deref() != Some(binding.uid.as_str())
        || pod.spec.as_ref()?.node_name.as_deref() != Some(binding.node.as_str())
        || statuses.len() != binding.containers.len()
    {
        return None;
    }
    let mut fates = Vec::with_capacity(statuses.len());
    for (status, expected) in statuses.into_iter().zip(&binding.containers) {
        if status.name != expected.name {
            return None;
        }
        let exited = [
            status
                .state
                .as_ref()
                .and_then(|state| state.terminated.as_ref()),
            status
                .last_state
                .as_ref()
                .and_then(|state| state.terminated.as_ref()),
        ]
        .into_iter()
        .flatten()
        .filter(|terminated| {
            terminated.container_id.as_deref() == Some(expected.container_id.as_str())
        })
        .find_map(|terminated| exact_exit(expected, terminated));
        let running = status.container_id.as_deref() == Some(expected.container_id.as_str())
            && status
                .state
                .as_ref()
                .and_then(|state| state.running.as_ref())
                .and_then(|running| running.started_at.as_ref())
                == Some(&expected.started_at);
        fates.push(match (exited, running) {
            (Some(exit), _) => ContainerFate::Exited(exit),
            (None, true) => ContainerFate::Running,
            (None, false) if newer_instance_started(expected, status) => {
                ContainerFate::Superseded(SupersededContainer {
                    name: expected.name.clone(),
                    container_id: expected.container_id.clone(),
                    started_at: expected.started_at.clone(),
                })
            }
            (None, false) => ContainerFate::Lost,
        });
    }
    Some(fates)
}

/// The Pod still runs a container instance other than this restartable
/// binding's: a newer incarnation owns the Pod and its retained evidence.
fn runs_newer_container(binding: &PodProcessBinding, pod: &Pod) -> bool {
    binding.restartable
        && pod.metadata.uid.as_deref() == Some(binding.uid.as_str())
        && pod
            .status
            .as_ref()
            .and_then(|status| status.container_statuses.as_ref())
            .is_some_and(|statuses| {
                statuses.iter().any(|status| {
                    binding.containers.iter().any(|bound| {
                        bound.name == status.name
                            && status
                                .container_id
                                .as_deref()
                                .is_some_and(|id| id != bound.container_id)
                    })
                })
            })
}

/// The end of a restartable binding from its per-container fates:
///
/// | Fates of the bound containers                       | Observation  |
/// |-----------------------------------------------------|--------------|
/// | every one `Exited`                                  | `Observed`   |
/// | no `Running` or `Lost`, at least one `Superseded`   | `Superseded` |
/// | any `Running` or `Lost`                             | `Unknown`    |
///
/// The Pod may live on with a newer incarnation, so neither row requires
/// deletion intent or a terminal phase.
fn observe_restartable(binding: &PodProcessBinding, pod: &Pod) -> PodProcessTermination {
    let ended = (|| {
        if !binding.retained || !binding_retention_is_present(binding, pod) {
            return None;
        }
        let fates = restartable_fates(binding, pod)?;
        let resource_version = coordinate(pod.metadata.resource_version.as_deref())?;
        let mut exits = Vec::with_capacity(fates.len());
        let mut superseded = Vec::new();
        for fate in fates {
            match fate {
                ContainerFate::Exited(exit) => exits.push(exit),
                ContainerFate::Superseded(container) => superseded.push(container),
                ContainerFate::Running | ContainerFate::Lost => return None,
            }
        }
        Some(if superseded.is_empty() {
            PodProcessTermination::Observed(Box::new(ObservedPodProcessExit {
                binding: binding.clone(),
                resource_version,
                exits,
            }))
        } else {
            PodProcessTermination::Superseded(Box::new(SupersededPodProcess {
                binding: binding.clone(),
                resource_version,
                exits,
                superseded,
            }))
        })
    })();
    ended.unwrap_or(PodProcessTermination::Unknown)
}

fn observe(binding: &PodProcessBinding, pod: &Pod) -> PodProcessTermination {
    if binding.restartable {
        return observe_restartable(binding, pod);
    }
    let proof = (|| {
        let statuses = matching_statuses(binding, pod)?;
        if !matches!(
            pod.status.as_ref()?.phase.as_deref(),
            Some("Succeeded" | "Failed")
        ) {
            return None;
        }
        let mut exits = Vec::with_capacity(statuses.len());
        for (status, expected) in statuses.into_iter().zip(&binding.containers) {
            exits.push(exact_exit(
                expected,
                status.state.as_ref()?.terminated.as_ref()?,
            )?);
        }
        Some((coordinate(pod.metadata.resource_version.as_deref())?, exits))
    })();
    proof.map_or(
        PodProcessTermination::Unknown,
        |(resource_version, exits)| {
            PodProcessTermination::Observed(Box::new(ObservedPodProcessExit {
                binding: binding.clone(),
                resource_version,
                exits,
            }))
        },
    )
}

#[cfg(test)]
mod tests;
