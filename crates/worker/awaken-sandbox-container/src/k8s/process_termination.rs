// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Independent observation of one frozen, private Pod process domain.
//!
//! This evidence covers local processes only. It does not discharge a Dispatch
//! attempt, prove external requests stopped, or grant replay authority. In
//! particular, Pod absence, deletion and phase Failed are not stop receipts.

use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde::{Deserialize, Serialize};

use super::{api_not_found, backend, has_forbidden_sandbox_namespace_shape};
use crate::RuntimeError;

mod stop;
pub use stop::PodProcessStopper;

/// Installed by the trusted Worker deployment before execution admission, and
/// retained until the exact attempt evidence has been durably consumed. This is
/// not the Session continuation-cleanup finalizer and must not release its PVCs.
pub const WORKER_PROCESS_EVIDENCE_FINALIZER: &str = "awaken.dev/worker-process-evidence";

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
    /// An Always-policy Pod needs accepted deletion intent as well as exact
    /// exits. Absence preserves the original Never-policy binding contract.
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
    Observed(ObservedPodProcessExit),
}

/// Subject-bound evidence minted only by authenticated observation. Private
/// fields and no Deserialize implementation prevent arbitrary receipt input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedPodProcessExit {
    binding: PodProcessBinding,
    resource_version: String,
}

impl ObservedPodProcessExit {
    pub fn binding(&self) -> &PodProcessBinding {
        &self.binding
    }

    pub fn resource_version(&self) -> &str {
        &self.resource_version
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
        use super::finalizer::{ExactFinalizerAdmission, update_exact_pod_finalizer};
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
    /// Every ordinary container is included. Init, ephemeral, independently
    /// restartable containers and prior restarts remain outside this proof
    /// domain. An Always-policy Pod additionally requires retained evidence.
    pub async fn bind_pod_process(
        &self,
        name: &str,
        expected_uid: &str,
    ) -> Result<PodProcessBinding, RuntimeError> {
        self.observed_pod_binding(name, expected_uid)
            .await
            .map(|(_, binding)| binding)
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
        if matching.next().is_some() || status.restart_count != 0 {
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
    super::continuation::CONTINUATION_CLEANUP_FINALIZER,
    super::create_ledger::ORIGINAL_GUARD_FINALIZER,
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
        let restartable = pod.spec.as_ref()?.restart_policy.as_deref() == Some("Always");
        if restartable && !evidence_is_retained(pod) {
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

fn observe(binding: &PodProcessBinding, pod: &Pod) -> PodProcessTermination {
    let proof = (|| {
        if binding.restartable
            && (!binding.retained
                || !binding_retention_is_present(binding, pod)
                || pod.metadata.deletion_timestamp.is_none())
        {
            return None;
        }
        let statuses = matching_statuses(binding, pod)?;
        if !matches!(
            pod.status.as_ref()?.phase.as_deref(),
            Some("Succeeded" | "Failed")
        ) {
            return None;
        }
        for (status, expected) in statuses.into_iter().zip(&binding.containers) {
            let state = status.state.as_ref()?;
            let terminated = state.terminated.as_ref()?;
            // Node-loss controllers may synthesize Failed/ContainerStatusUnknown.
            // Require an exact runtime exit, not a phase or a synthetic exit code.
            if !matches!(
                terminated.reason.as_deref(),
                Some("Completed" | "Error" | "OOMKilled")
            ) || terminated.container_id.as_deref() != Some(expected.container_id.as_str())
                || terminated.finished_at.as_ref()? < terminated.started_at.as_ref()?
            {
                return None;
            }
        }
        coordinate(pod.metadata.resource_version.as_deref())
    })();
    proof.map_or(PodProcessTermination::Unknown, |resource_version| {
        PodProcessTermination::Observed(ObservedPodProcessExit {
            binding: binding.clone(),
            resource_version,
        })
    })
}

#[cfg(test)]
mod tests;
