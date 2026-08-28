use std::fmt::Debug;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, PostParams, Preconditions};
use kube::{Api, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::error::{api_conflict, api_not_found, backend};
use crate::RuntimeError;

const REALIZATION_DIGEST_ANNOTATION: &str = "awaken.dev/realization-digest";
const POD_READY_TIMEOUT: Duration = Duration::from_secs(120);
const POD_READY_POLL: Duration = Duration::from_millis(250);
// Retained Session Pods intentionally keep Kubernetes' default 30-second
// termination grace. The observation fence must extend beyond that grace plus
// apiserver/kubelet propagation; using the same value creates a guaranteed
// boundary race even when deletion is healthy.
const POD_DELETE_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Reap a terminal Pod left behind by eviction or node loss before realizing a
/// new attempt under the same deterministic runtime id. A live or provisioning
/// Pod is never replaced: its realization digest still decides whether the
/// caller may adopt it. The UID/resourceVersion preconditions prevent a stale
/// observer from deleting a concurrently-created incarnation with the same
/// name.
pub(super) async fn reap_terminal_pod(api: &Api<Pod>, name: &str) -> Result<(), RuntimeError> {
    let existing = match api.get(name).await {
        Ok(pod) => pod,
        Err(error) if api_not_found(&error) => return Ok(()),
        Err(error) => return Err(backend(error)),
    };
    let Some(preconditions) = terminal_pod_preconditions(&existing) else {
        return Ok(());
    };
    match api
        .delete(name, &DeleteParams::default().preconditions(preconditions))
        .await
    {
        Ok(_) => await_pod_deleted(api, name).await,
        Err(error) if api_not_found(&error) => Ok(()),
        Err(error) => Err(backend(error)),
    }
}

fn terminal_pod_preconditions(pod: &Pod) -> Option<Preconditions> {
    let terminal_phase = pod
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .is_some_and(|phase| matches!(phase, "Failed" | "Succeeded"));
    // The input projector and other service sidecars may keep the Pod phase
    // `Running` after the workload-owning agent has terminated. For this adapter
    // the agent is the lifecycle root, so its terminated state is equally terminal.
    let terminal_agent = pod
        .status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .and_then(|statuses| statuses.iter().find(|status| status.name == "agent"))
        .and_then(|status| status.state.as_ref())
        .is_some_and(|state| state.terminated.is_some());
    let terminal = terminal_phase || terminal_agent;
    if !terminal || pod.metadata.deletion_timestamp.is_some() {
        return None;
    }
    let uid = pod.metadata.uid.clone()?;
    let resource_version = pod.metadata.resource_version.clone()?;
    Some(Preconditions {
        uid: Some(uid),
        resource_version: Some(resource_version),
    })
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
    fn deletion_observation_outlives_kubernetes_default_grace() {
        assert!(
            POD_DELETE_TIMEOUT > Duration::from_secs(30),
            "the API observation fence must not expire at the same instant as Kubernetes' default grace"
        );
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

    #[test]
    fn only_terminal_pods_are_safe_to_replace_with_identity_preconditions() {
        /* Recovery decision table. R1 Pending/Running with a live agent => preserve; R2 a Pod
         * already being deleted => preserve; R3 Failed/Succeeded with complete
         * UID/resourceVersion, or Running with a terminated lifecycle-root agent,
         * => replace under that exact fence; R4 terminal but missing either identity
         * coordinate => preserve. This covers the
         * DiskPressure eviction that previously left deterministic names stuck
         * behind `different realization` forever without permitting an unfenced
         * same-name deletion. */
        for phase in ["Pending", "Running"] {
            let mut existing = pod(phase, phase == "Running", None);
            existing.metadata.uid = Some("live-uid".into());
            assert_eq!(terminal_pod_preconditions(&existing), None, "R1 {phase}");
        }

        let mut deleting = pod("Failed", false, None);
        deleting.metadata.deletion_timestamp =
            Some(serde_json::from_str("\"2026-08-03T00:45:35Z\"").expect("valid timestamp"));
        assert_eq!(terminal_pod_preconditions(&deleting), None, "R2");

        for phase in ["Failed", "Succeeded"] {
            let mut terminal = pod(phase, false, None);
            terminal.metadata.uid = Some(format!("{phase}-uid"));
            terminal.metadata.resource_version = Some("21413".into());
            assert_eq!(
                terminal_pod_preconditions(&terminal),
                Some(Preconditions {
                    uid: Some(format!("{phase}-uid")),
                    resource_version: Some("21413".into()),
                }),
                "R3 {phase}"
            );
        }

        let mut sidecar_held_running = pod("Running", false, None);
        sidecar_held_running.metadata.uid = Some("terminated-agent-uid".into());
        sidecar_held_running.metadata.resource_version = Some("21414".into());
        sidecar_held_running
            .status
            .as_mut()
            .unwrap()
            .container_statuses = Some(vec![sidecar("agent", false, None, Some(0))]);
        assert_eq!(
            terminal_pod_preconditions(&sidecar_held_running),
            Some(Preconditions {
                uid: Some("terminated-agent-uid".into()),
                resource_version: Some("21414".into()),
            }),
            "R3 a live sidecar cannot keep a terminated agent realization adoptable"
        );

        for missing in ["uid", "resourceVersion"] {
            let mut terminal = pod("Failed", false, None);
            terminal.metadata.uid = (missing != "uid").then(|| "observed-uid".into());
            terminal.metadata.resource_version =
                (missing != "resourceVersion").then(|| "21413".into());
            assert_eq!(
                terminal_pod_preconditions(&terminal),
                None,
                "R4 missing {missing} must preserve the Pod instead of issuing an unfenced delete"
            );
        }
    }
}
