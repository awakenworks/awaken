use std::fmt::Debug;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::PostParams;
use kube::{Api, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{api_conflict, api_not_found, backend};
use crate::RuntimeError;

const REALIZATION_DIGEST_ANNOTATION: &str = "awaken.dev/realization-digest";
const POD_READY_TIMEOUT: Duration = Duration::from_secs(120);
const POD_READY_POLL: Duration = Duration::from_millis(250);
const POD_DELETE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stamp the exact desired Kubernetes object before its first API write. A retry
/// may reuse an existing object only when this immutable realization fingerprint
/// matches; a same-name object with different bytes/spec fails closed.
pub(super) fn stamp_realization<K>(object: &mut K) -> Result<(), RuntimeError>
where
    K: Resource<DynamicType = ()> + Serialize,
{
    let encoded = serde_json::to_vec(object).map_err(backend)?;
    let digest = blake3::hash(&encoded).to_hex().to_string();
    object
        .meta_mut()
        .annotations
        .get_or_insert_with(Default::default)
        .insert(REALIZATION_DIGEST_ANNOTATION.into(), digest);
    Ok(())
}

/// Create once, or verify and reuse the exact object produced by a concurrent or
/// retried realization. Kubernetes 409 is not success by itself: the immutable
/// fingerprint must match and the existing object must not be terminating.
pub(super) async fn create_or_verify<K>(api: &Api<K>, desired: &K) -> Result<K, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
{
    match api.create(&PostParams::default(), desired).await {
        Ok(created) => Ok(created),
        Err(error) if api_conflict(&error) => {
            let name = desired.name_any();
            let existing = api.get(&name).await.map_err(backend)?;
            let expected = desired
                .annotations()
                .get(REALIZATION_DIGEST_ANNOTATION)
                .ok_or_else(|| {
                    backend(format!(
                        "desired {} `{name}` has no realization digest",
                        K::kind(&())
                    ))
                })?;
            let actual = existing.annotations().get(REALIZATION_DIGEST_ANNOTATION);
            if actual != Some(expected) {
                return Err(backend(format!(
                    "existing {} `{name}` belongs to a different realization",
                    K::kind(&())
                )));
            }
            if existing.meta().deletion_timestamp.is_some() {
                return Err(backend(format!(
                    "existing {} `{name}` is terminating",
                    K::kind(&())
                )));
            }
            Ok(existing)
        }
        Err(error) => Err(backend(error)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PodReadiness {
    Ready,
    Waiting(String),
    Failed(String),
}

pub(super) fn pod_readiness(pod: &Pod) -> PodReadiness {
    if pod.metadata.deletion_timestamp.is_some() {
        return PodReadiness::Failed("Pod is terminating".into());
    }
    let status = pod.status.as_ref();
    let phase = status
        .and_then(|status| status.phase.as_deref())
        .unwrap_or("Unknown");
    if matches!(phase, "Failed" | "Succeeded") {
        return PodReadiness::Failed(format!("Pod reached terminal phase {phase}"));
    }

    let agent = status
        .and_then(|status| status.container_statuses.as_ref())
        .and_then(|statuses| statuses.iter().find(|status| status.name == "agent"));
    if phase == "Running"
        && agent.is_some_and(|status| {
            status.ready
                && status
                    .state
                    .as_ref()
                    .is_some_and(|state| state.running.is_some())
        })
    {
        return PodReadiness::Ready;
    }
    if let Some(waiting) = agent
        .and_then(|status| status.state.as_ref())
        .and_then(|state| state.waiting.as_ref())
    {
        let reason = waiting.reason.as_deref().unwrap_or("waiting");
        let message = waiting.message.as_deref().unwrap_or_default();
        if matches!(
            reason,
            "ErrImagePull"
                | "ImagePullBackOff"
                | "InvalidImageName"
                | "CreateContainerConfigError"
                | "CreateContainerError"
                | "RunContainerError"
        ) {
            return PodReadiness::Failed(format!("agent container {reason}: {message}"));
        }
        return PodReadiness::Waiting(format!("agent container {reason}: {message}"));
    }
    PodReadiness::Waiting(format!("Pod phase is {phase}"))
}

pub(super) async fn await_pod_ready(api: &Api<Pod>, name: &str) -> Result<(), RuntimeError> {
    let deadline = tokio::time::Instant::now() + POD_READY_TIMEOUT;
    loop {
        let pod = api.get(name).await.map_err(backend)?;
        match pod_readiness(&pod) {
            PodReadiness::Ready => return Ok(()),
            PodReadiness::Failed(reason) => {
                return Err(backend(format!(
                    "Pod `{name}` cannot become ready: {reason}"
                )));
            }
            PodReadiness::Waiting(reason) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(backend(format!(
                        "Pod `{name}` did not become ready within {}s: {reason}",
                        POD_READY_TIMEOUT.as_secs()
                    )));
                }
            }
        }
        tokio::time::sleep(POD_READY_POLL).await;
    }
}

pub(super) async fn await_pod_deleted(api: &Api<Pod>, name: &str) -> Result<(), RuntimeError> {
    let deadline = tokio::time::Instant::now() + POD_DELETE_TIMEOUT;
    loop {
        match api.get(name).await {
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
            Ok(_) if tokio::time::Instant::now() >= deadline => {
                return Err(backend(format!(
                    "Pod `{name}` was not deleted within {}s",
                    POD_DELETE_TIMEOUT.as_secs()
                )));
            }
            Ok(_) => tokio::time::sleep(POD_READY_POLL).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateWaiting, ContainerStatus, PodStatus,
    };

    use super::*;

    fn pod(phase: &str, ready: bool, waiting: Option<&str>) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some(phase.into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "agent".into(),
                    ready,
                    image: "sandbox:test".into(),
                    image_id: String::new(),
                    restart_count: 0,
                    started: Some(ready),
                    state: Some(ContainerState {
                        running: ready.then(ContainerStateRunning::default),
                        waiting: waiting.map(|reason| ContainerStateWaiting {
                            reason: Some(reason.into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pod_readiness_distinguishes_ready_transient_and_terminal_states() {
        /* Cause/effect decision table. Causes: C1 Running+agent-ready; C2 Pending
         * or nonterminal container wait; C3 image/config wait failure; C4 terminal
         * phase. Effects: E1 Ready; E2 Waiting/Provisioning; E3 Failed with reason.
         * Rules: P1 C1=>E1; P2 C2=>E2; P3 C3|C4=>E3. */
        assert_eq!(
            pod_readiness(&pod("Running", true, None)),
            PodReadiness::Ready,
            "P1"
        );
        assert!(
            matches!(
                pod_readiness(&pod("Pending", false, Some("ContainerCreating"))),
                PodReadiness::Waiting(_)
            ),
            "P2"
        );
        assert!(
            matches!(
                pod_readiness(&pod("Pending", false, Some("ImagePullBackOff"))),
                PodReadiness::Failed(_)
            ),
            "P3 image"
        );
        assert!(
            matches!(
                pod_readiness(&pod("Failed", false, None)),
                PodReadiness::Failed(_)
            ),
            "P3 phase"
        );
    }

    #[test]
    fn realization_digest_changes_with_desired_content() {
        /* Fingerprint rule F1: identical desired objects receive one stable digest;
         * F2: changing immutable desired data changes it, so 409 reuse cannot adopt
         * a same-name object from another realization. */
        let mut first = Pod::default();
        first.metadata.name = Some("same".into());
        let mut identical = first.clone();
        stamp_realization(&mut first).unwrap();
        stamp_realization(&mut identical).unwrap();
        assert_eq!(first.annotations(), identical.annotations(), "F1");

        let mut changed = Pod::default();
        changed.metadata.name = Some("different".into());
        stamp_realization(&mut changed).unwrap();
        assert_ne!(first.annotations(), changed.annotations(), "F2");
    }
}
