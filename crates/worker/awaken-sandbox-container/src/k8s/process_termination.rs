//! Independent observation of one non-restarting, private Pod process domain.
//!
//! This evidence covers local processes only. It does not discharge a Dispatch
//! attempt, prove external requests stopped, or grant replay authority. In
//! particular, Pod absence, deletion and phase Failed are not stop receipts.

use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use serde::{Deserialize, Serialize};

use super::{K8sRuntime, api_not_found, backend, has_forbidden_sandbox_namespace_shape};
use crate::RuntimeError;

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

impl K8sRuntime {
    /// Obtain the exact running process coordinate through the existing control
    /// client. The caller must persist it before allowing any external effects.
    /// Every ordinary container is included. The first profile rejects init,
    /// ephemeral and independently restartable containers instead of silently
    /// omitting them from its proof domain.
    pub async fn bind_pod_process(&self, name: &str) -> Result<PodProcessBinding, RuntimeError> {
        let pod = self.pods().get(name).await.map_err(backend)?;
        bind(&self.namespace, name, &pod)
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
        match self.pods().get(&binding.name).await {
            Ok(pod) => Ok(observe(binding, &pod)),
            Err(error) if api_not_found(&error) => Ok(PodProcessTermination::Unknown),
            Err(error) => Err(backend(error)),
        }
    }
}

fn process_statuses(pod: &Pod) -> Option<Vec<&ContainerStatus>> {
    let spec = pod.spec.as_ref()?;
    if spec.restart_policy.as_deref() != Some("Never")
        || has_forbidden_sandbox_namespace_shape(spec)
        || spec.containers.is_empty()
        || spec.init_containers.as_ref().is_some_and(|v| !v.is_empty())
        || spec
            .volumes
            .as_ref()
            .is_some_and(|v| v.iter().any(|v| v.host_path.is_some()))
    {
        return None;
    }
    let status = pod.status.as_ref()?;
    if status
        .init_container_statuses
        .as_ref()
        .is_some_and(|v| !v.is_empty())
        || status
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
        let security = container.security_context.as_ref()?;
        let capabilities = security.capabilities.as_ref()?;
        if container.name.is_empty()
            || container.restart_policy.is_some()
            || security.privileged == Some(true)
            || security.allow_privilege_escalation != Some(false)
            || !capabilities
                .drop
                .as_ref()
                .is_some_and(|v| v.iter().any(|v| v == "ALL"))
            || capabilities.add.as_ref().is_some_and(|v| !v.is_empty())
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
    value.filter(|value| !value.is_empty()).map(str::to_owned)
}

fn bind(namespace: &str, name: &str, pod: &Pod) -> Result<PodProcessBinding, RuntimeError> {
    let binding = (|| {
        if pod.metadata.deletion_timestamp.is_some()
            || pod.metadata.namespace.as_deref() != Some(namespace)
            || pod.metadata.name.as_deref() != Some(name)
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
        Some(PodProcessBinding {
            namespace: coordinate(Some(namespace))?,
            name: coordinate(Some(name))?,
            uid: coordinate(pod.metadata.uid.as_deref())?,
            node: coordinate(pod.spec.as_ref()?.node_name.as_deref())?,
            containers,
        })
    })();
    binding.ok_or_else(|| backend("Pod has no exact non-restarting private process binding"))
}

fn observe(binding: &PodProcessBinding, pod: &Pod) -> PodProcessTermination {
    let proof = (|| {
        let statuses = process_statuses(pod)?;
        let spec = pod.spec.as_ref()?;
        if pod.metadata.namespace.as_deref() != Some(binding.namespace.as_str())
            || pod.metadata.name.as_deref() != Some(binding.name.as_str())
            || pod.metadata.uid.as_deref() != Some(binding.uid.as_str())
            || spec.node_name.as_deref() != Some(binding.node.as_str())
            || statuses.len() != binding.containers.len()
            || !matches!(
                pod.status.as_ref()?.phase.as_deref(),
                Some("Succeeded" | "Failed")
            )
        {
            return None;
        }
        for (status, expected) in statuses.into_iter().zip(&binding.containers) {
            let state = status.state.as_ref()?;
            if status.name != expected.name
                || status.container_id.as_deref() != Some(expected.container_id.as_str())
                || state.running.is_some()
                || state.waiting.is_some()
            {
                return None;
            }
            let terminated = state.terminated.as_ref()?;
            // Node-loss controllers may synthesize Failed/ContainerStatusUnknown.
            // Require an exact runtime exit, not a phase or a synthetic exit code.
            if !matches!(
                terminated.reason.as_deref(),
                Some("Completed" | "Error" | "OOMKilled")
            ) || terminated.container_id.as_deref() != Some(expected.container_id.as_str())
                || terminated.started_at.as_ref()? != &expected.started_at
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
