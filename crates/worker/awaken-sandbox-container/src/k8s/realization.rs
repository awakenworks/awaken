use std::fmt::Debug;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::Pod;
use kube::api::PostParams;
use kube::{Api, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::error::{api_conflict, backend};
use crate::RuntimeError;

const REALIZATION_DIGEST_ANNOTATION: &str = "awaken.dev/realization-digest";
const POD_READY_TIMEOUT: Duration = Duration::from_secs(120);
const POD_READY_POLL: Duration = Duration::from_millis(250);

/// Stamp the exact desired Kubernetes object before its first API write. A retry
/// may reuse an existing object only when this immutable realization fingerprint
/// matches; a same-name object with different bytes/spec fails closed.
pub(crate) fn stamp_realization<K>(object: &mut K) -> Result<(), RuntimeError>
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

pub(super) fn realization_digest_matches<K>(desired: &K, existing: &K) -> Result<bool, RuntimeError>
where
    K: Resource<DynamicType = ()>,
{
    let name = desired.name_any();
    let expected = desired
        .annotations()
        .get(REALIZATION_DIGEST_ANNOTATION)
        .ok_or_else(|| {
            backend(format!(
                "desired {} `{name}` has no realization digest",
                K::kind(&())
            ))
        })?;
    Ok(existing.annotations().get(REALIZATION_DIGEST_ANNOTATION) == Some(expected))
}

pub(crate) fn verify_realization<K>(desired: &K, observed: &K) -> Result<(), RuntimeError>
where
    K: Resource<DynamicType = ()>,
{
    if !realization_digest_matches(desired, observed)?
        || observed.meta().deletion_timestamp.is_some()
    {
        return Err(backend(format!(
            "Kubernetes {} differs from the exact immutable realization",
            K::kind(&())
        )));
    }
    Ok(())
}

/// Bind one exact restore identity to a Kubernetes physical object before its
/// immutable realization digest is calculated. Replays can therefore reuse a
/// 409 object only when both its specification and restore tuple match.
pub(crate) fn stamp_restoration<K>(object: &mut K, evidence: &pc::SandboxRestorationEvidence)
where
    K: Resource<DynamicType = ()>,
{
    object
        .meta_mut()
        .annotations
        .get_or_insert_with(Default::default)
        .extend(
            crate::restoration_metadata(evidence)
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
}

pub(crate) fn stamp_restoration_plan<K>(object: &mut K, plan_fingerprint: &str)
where
    K: Resource<DynamicType = ()>,
{
    object
        .meta_mut()
        .annotations
        .get_or_insert_with(Default::default)
        .insert(
            crate::RESTORE_PLAN_LABEL.to_string(),
            plan_fingerprint.to_owned(),
        );
}

pub(crate) fn restoration_plan_fingerprint<K>(object: &K) -> Option<&str>
where
    K: Resource<DynamicType = ()>,
{
    object
        .annotations()
        .get(crate::RESTORE_PLAN_LABEL)
        .map(String::as_str)
}

pub(crate) fn verify_restoration<K>(
    object: &K,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<(), RuntimeError>
where
    K: Resource<DynamicType = ()>,
{
    if restoration_evidence(object)?.as_ref() != Some(evidence) {
        return Err(backend(format!(
            "Kubernetes {} belongs to a different exact restore effect",
            K::kind(&())
        )));
    }
    Ok(())
}

pub(crate) fn restoration_evidence<K>(
    object: &K,
) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError>
where
    K: Resource<DynamicType = ()>,
{
    let annotations = object.annotations();
    crate::restoration_evidence_from_metadata(
        |key| annotations.get(key).cloned(),
        &format!("Kubernetes {}", K::kind(&())),
    )
}

/// Stamp a Pod's immutable realization without treating the process-local runtime
/// owner as part of that identity. The owner label is a transferable liveness
/// lease: a replacement Worker must be able to adopt the same frozen Session Pod
/// while every executable/mount/security field remains digest-fenced.
pub(super) fn stamp_pod_realization(pod: &mut Pod) -> Result<(), RuntimeError> {
    let owner = pod
        .metadata
        .labels
        .as_mut()
        .and_then(|labels| labels.remove(crate::RUNTIME_OWNER_LABEL));
    let result = stamp_realization(pod);
    if let Some(owner) = owner {
        pod.metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert(crate::RUNTIME_OWNER_LABEL.to_string(), owner);
    }
    result
}

/// Create once, or verify and reuse the exact object produced by a concurrent or
/// retried realization. Kubernetes 409 is not success by itself: the immutable
/// fingerprint must match and the existing object must not be terminating.
pub(super) async fn create_or_verify<K>(api: &Api<K>, desired: &K) -> Result<K, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
{
    Ok(create_or_verify_with_status(api, desired).await?.object)
}

pub(crate) struct CreateOutcome<K> {
    pub object: K,
    pub created: bool,
}

pub(super) async fn create_or_verify_with_status<K>(
    api: &Api<K>,
    desired: &K,
) -> Result<CreateOutcome<K>, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
{
    match api.create(&PostParams::default(), desired).await {
        Ok(created) => Ok(CreateOutcome {
            object: created,
            created: true,
        }),
        Err(error) if api_conflict(&error) => {
            let name = desired.name_any();
            let existing = api.get(&name).await.map_err(backend)?;
            if !realization_digest_matches(desired, &existing)? {
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
            Ok(CreateOutcome {
                object: existing,
                created: false,
            })
        }
        Err(error) => Err(backend(error)),
    }
}

/// Create or reuse one realization and then verify its API-observed immutable
/// projection. The ordinary realization digest remains the first 409 fence;
/// callers with API-defaulted objects add one canonical projection verifier so
/// copying that annotation onto a different spec can never authorize reuse.
pub(crate) async fn create_or_verify_exact<K, F>(
    api: &Api<K>,
    desired: &K,
    verify: F,
) -> Result<K, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
    F: FnOnce(&K, &K) -> Result<(), RuntimeError>,
{
    let object = create_or_verify(api, desired).await?;
    verify(desired, &object)?;
    Ok(object)
}

pub(crate) async fn create_or_verify_with_status_exact<K, F>(
    api: &Api<K>,
    desired: &K,
    verify: F,
) -> Result<CreateOutcome<K>, RuntimeError>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()> + Serialize,
    F: FnOnce(&K, &K) -> Result<(), RuntimeError>,
{
    let outcome = create_or_verify_with_status(api, desired).await?;
    verify(desired, &outcome.object)?;
    Ok(outcome)
}

/// Transfer only the process-local runtime-owner lease on one already
/// realization-fenced Pod. Kubernetes resourceVersion supplies the CAS fence;
/// the expected UID prevents a same-name replacement from being adopted.
pub(super) async fn transfer_runtime_owner(
    api: &Api<Pod>,
    mut pod: Pod,
    expected_uid: &str,
    next_owner: &str,
) -> Result<Pod, RuntimeError> {
    if pod.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(backend(
            "Kubernetes Sandbox Pod changed before runtime-owner transfer",
        ));
    }
    if pod
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(crate::RUNTIME_OWNER_LABEL))
        .map(String::as_str)
        == Some(next_owner)
    {
        return Ok(pod);
    }
    let name = pod
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    pod.metadata
        .labels
        .get_or_insert_with(Default::default)
        .insert(
            crate::RUNTIME_OWNER_LABEL.to_string(),
            next_owner.to_owned(),
        );
    let replaced = api
        .replace(&name, &PostParams::default(), &pod)
        .await
        .map_err(backend)?;
    if replaced.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(backend(
            "Kubernetes Sandbox Pod changed during runtime-owner transfer",
        ));
    }
    Ok(replaced)
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

    let Some(containers) = status.and_then(|status| status.container_statuses.as_ref()) else {
        return PodReadiness::Waiting(format!("Pod phase is {phase}"));
    };
    let required = match pod
        .spec
        .as_ref()
        .map(|spec| spec.containers.as_slice())
        .filter(|declared| !declared.is_empty())
    {
        Some(declared) => {
            let mut required = Vec::with_capacity(declared.len());
            for expected in declared {
                let Some(status) = containers
                    .iter()
                    .find(|status| status.name == expected.name)
                else {
                    return PodReadiness::Waiting(format!(
                        "{} container has no status yet",
                        expected.name
                    ));
                };
                required.push(status);
            }
            required
        }
        None => containers.iter().collect(),
    };
    let agent = required.iter().find(|status| status.name == "agent");
    for container in &required {
        let state = container.state.as_ref();
        if let Some(terminated) = state.and_then(|state| state.terminated.as_ref()) {
            let reason = terminated.reason.as_deref().unwrap_or("terminated");
            return PodReadiness::Failed(format!(
                "{} container {reason} with exit code {}",
                container.name, terminated.exit_code
            ));
        }
        if let Some(waiting) = state.and_then(|state| state.waiting.as_ref()) {
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
                    | "ContainerCannotRun"
                    | "CrashLoopBackOff"
            ) {
                return PodReadiness::Failed(format!(
                    "{} container {reason}: {message}",
                    container.name
                ));
            }
        }
    }
    let all_ready = !required.is_empty()
        && required.iter().all(|container| {
            container.ready
                && container
                    .state
                    .as_ref()
                    .is_some_and(|state| state.running.is_some())
        });
    if phase == "Running" && agent.is_some() && all_ready {
        return PodReadiness::Ready;
    }
    if let Some((container, waiting)) = required.iter().find_map(|container| {
        container
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
            .map(|waiting| (container, waiting))
    }) {
        let reason = waiting.reason.as_deref().unwrap_or("waiting");
        let message = waiting.message.as_deref().unwrap_or_default();
        return PodReadiness::Waiting(format!("{} container {reason}: {message}", container.name));
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

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateRunning, ContainerStateTerminated,
        ContainerStateWaiting, ContainerStatus, PodSpec, PodStatus,
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

    fn sidecar(
        name: &str,
        ready: bool,
        waiting: Option<&str>,
        terminated: Option<i32>,
    ) -> ContainerStatus {
        ContainerStatus {
            name: name.into(),
            ready,
            image: "sidecar:test".into(),
            image_id: String::new(),
            restart_count: 0,
            started: Some(ready),
            state: Some(ContainerState {
                running: ready.then(ContainerStateRunning::default),
                waiting: waiting.map(|reason| ContainerStateWaiting {
                    reason: Some(reason.into()),
                    ..Default::default()
                }),
                terminated: terminated.map(|exit_code| ContainerStateTerminated {
                    exit_code,
                    reason: Some("Error".into()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn pod_readiness_distinguishes_ready_transient_and_terminal_states() {
        /* Cause/effect decision table. Causes: C1 Running+agent-ready; C2 Pending
         * or nonterminal container wait; C3 image/config wait failure; C4 terminal
         * phase; C5 every required sidecar is running+ready; C6 a sidecar is
         * waiting fatally or has terminated; C7 a declared sidecar has no status
         * yet. Effects: E1 Ready; E2
         * Waiting/Provisioning; E3 Failed with the owning container and reason.
         * Rules: P1 C1+C5=>E1; P2 C2=>E2; P3 C3|C4|C6=>E3;
         * P4 C1+(!C5|C7)=>E2. */
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

        let mut all_ready = pod("Running", true, None);
        all_ready
            .status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()
            .push(sidecar("memoryd-0", true, None, None));
        assert_eq!(pod_readiness(&all_ready), PodReadiness::Ready, "P1");

        let mut starting_sidecar = pod("Running", true, None);
        starting_sidecar
            .status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()
            .push(sidecar(
                "input-projector",
                false,
                Some("ContainerCreating"),
                None,
            ));
        assert!(
            matches!(pod_readiness(&starting_sidecar), PodReadiness::Waiting(_)),
            "P4 an agent cannot make an incomplete Pod ready"
        );

        let mut missing_sidecar_status = pod("Running", true, None);
        missing_sidecar_status.spec = Some(PodSpec {
            containers: vec![
                Container {
                    name: "agent".into(),
                    ..Default::default()
                },
                Container {
                    name: "memoryd-0".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        assert!(
            matches!(
                pod_readiness(&missing_sidecar_status),
                PodReadiness::Waiting(reason) if reason.contains("memoryd-0")
            ),
            "P4 a declared sidecar without status cannot be ignored"
        );

        for (name, waiting, terminated) in [
            ("memoryd-0", Some("CrashLoopBackOff"), None),
            ("input-projector", None, Some(9)),
        ] {
            let mut failed_sidecar = pod("Running", true, None);
            failed_sidecar
                .status
                .as_mut()
                .unwrap()
                .container_statuses
                .as_mut()
                .unwrap()
                .push(sidecar(name, false, waiting, terminated));
            let result = pod_readiness(&failed_sidecar);
            assert!(matches!(result, PodReadiness::Failed(_)), "P3 {result:?}");
            assert!(
                matches!(result, PodReadiness::Failed(reason) if reason.contains(name)),
                "failure detection identifies the owning sidecar"
            );
        }
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

    #[test]
    fn runtime_owner_is_transferable_but_pod_realization_remains_fenced() {
        /* Worker-restart adoption decision table — F3:
         * C1 two Workers project the same frozen Pod; C2 only the process-local
         * runtime owner differs; C3 an executable Pod field is same/different.
         * C1+C2+same(C3) => E1 equal immutable digest while both owner labels are
         * retained for CAS transfer. C1+C2+different(C3) => E2 different digest,
         * so owner transfer cannot authorize a changed realization.
         */
        let mut first = Pod::default();
        first.metadata.name = Some("session".into());
        first.metadata.labels = Some(std::collections::BTreeMap::from([(
            crate::RUNTIME_OWNER_LABEL.into(),
            "worker-incarnation-a".into(),
        )]));
        first.spec = Some(PodSpec {
            containers: vec![Container {
                name: "agent".into(),
                image: Some("agent:v1".into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut replacement = first.clone();
        replacement.metadata.labels.as_mut().unwrap().insert(
            crate::RUNTIME_OWNER_LABEL.into(),
            "worker-incarnation-b".into(),
        );
        stamp_pod_realization(&mut first).unwrap();
        stamp_pod_realization(&mut replacement).unwrap();
        assert_eq!(first.annotations(), replacement.annotations(), "E1 digest");
        assert_ne!(
            first.metadata.labels, replacement.metadata.labels,
            "E1 lease"
        );

        let mut changed = replacement.clone();
        changed.spec.as_mut().unwrap().containers[0].image = Some("agent:v2".into());
        changed.metadata.annotations = None;
        stamp_pod_realization(&mut changed).unwrap();
        assert_ne!(first.annotations(), changed.annotations(), "E2");
    }
}
