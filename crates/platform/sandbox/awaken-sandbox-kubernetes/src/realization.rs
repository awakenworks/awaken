// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Kubernetes realization phase machine.
use std::time::Duration;

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Resource, ResourceExt};

use awaken_container_contract::RuntimeError;
use awaken_worker_fleet_kubernetes::backend;
use awaken_worker_fleet_kubernetes::object_realization::REALIZATION_DIGEST_ANNOTATION;
pub(crate) use awaken_worker_fleet_kubernetes::object_realization::{
    create_or_verify, create_or_verify_exact, create_or_verify_with_status,
    create_or_verify_with_status_exact, realization_digest_matches, stamp_realization,
};

const POD_READY_TIMEOUT: Duration = Duration::from_secs(120);
const POD_READY_POLL: Duration = Duration::from_millis(250);

pub(crate) fn verify_realization<K>(
    desired: &K,
    observed: &K,
    disposal: Option<&pc::SandboxDisposalAuthorization>,
) -> Result<(), RuntimeError>
where
    K: Resource<DynamicType = ()>,
{
    if let Some(authorization) = disposal {
        authorization.validate().map_err(backend)?;
    }
    if !realization_digest_matches(desired, observed)?
        || (disposal.is_none() && observed.meta().deletion_timestamp.is_some())
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
            awaken_sandbox_container::engine::restoration_metadata(evidence)
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
            awaken_sandbox_container::engine::RESTORE_PLAN_LABEL.to_string(),
            plan_fingerprint.to_owned(),
        );
}

pub(crate) fn restoration_plan_fingerprint<K>(object: &K) -> Option<&str>
where
    K: Resource<DynamicType = ()>,
{
    object
        .annotations()
        .get(awaken_sandbox_container::engine::RESTORE_PLAN_LABEL)
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
    awaken_sandbox_container::engine::restoration_evidence_from_metadata(
        |key| annotations.get(key).cloned(),
        &format!("Kubernetes {}", K::kind(&())),
    )
}

/// Stamp a Pod's immutable realization without treating the process-local runtime
/// owner as part of that identity. The owner label is a transferable liveness
/// lease: a replacement Worker must be able to adopt the same frozen Session Pod
/// while every executable/mount/security field remains digest-fenced.
pub(super) fn stamp_pod_realization(pod: &mut Pod) -> Result<(), RuntimeError> {
    // Re-stamping desired inputs must not hash the preceding digest into itself.
    if let Some(annotations) = pod.metadata.annotations.as_mut() {
        annotations.remove(REALIZATION_DIGEST_ANNOTATION);
        if annotations.is_empty() {
            pod.metadata.annotations = None;
        }
    }
    let owner =
        pod.metadata.labels.as_mut().and_then(|labels| {
            labels.remove(awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL)
        });
    let result = stamp_realization(pod);
    if let Some(owner) = owner {
        pod.metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert(
                awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.to_string(),
                owner,
            );
    }
    result
}

pub(super) use awaken_container_contract::K8sPodReadiness as PodReadiness;

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

/// Project the API's scheduler condition through the one canonical refusal
/// predicate. Readiness waiting and original-create disposal use the same
/// Pending + PodScheduled=False + Unschedulable law, without another policy.
pub(super) fn pod_unschedulable(pod: &Pod) -> Option<String> {
    let status = pod.status.as_ref();
    let scheduled = status
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .find(|condition| condition.type_ == "PodScheduled")
        .map(
            |condition| awaken_container_contract::K8sPodScheduledCondition {
                status: &condition.status,
                reason: condition.reason.as_deref(),
                message: condition.message.as_deref(),
            },
        );
    awaken_container_contract::k8s_pod_unschedulable(
        status.and_then(|status| status.phase.as_deref()),
        scheduled,
    )
}

/// The Pod's creation instant as the API server recorded it: durable
/// evidence that outlives the creating process, so a later observer measures
/// the same wait the creator did.
pub(super) fn pod_created_ms(pod: &Pod) -> Option<u64> {
    let created = pod
        .metadata
        .creation_timestamp
        .as_ref()
        .and_then(|time| serde_json::to_value(time).ok())?;
    awaken_time::utc::rfc3339_to_epoch_millis(created.as_str()?)
}

/// The provider's deadline over a Pod it still observes provisioning, read
/// on the composed clock: the bound the creating attempt's own readiness wait
/// applies. A Pod without a creation instant or a runtime composed without a
/// clock has no deadline, so the wait fails closed to a bounded retry.
pub(super) fn pod_provisioning_deadline(
    pod: &Pod,
    unschedulable_wait: Duration,
    clock: Option<&std::sync::Arc<dyn awaken_time::WallClock>>,
) -> Option<awaken_provisioning_contract::ProvisioningDeadline> {
    let created = pod_created_ms(pod)?;
    let observed_at = awaken_time::WallClockExt::now_ms(clock?.as_ref());
    Some(awaken_provisioning_contract::ProvisioningDeadline::new(
        awaken_container_contract::k8s_pod_provisioning_deadline_ms(
            created,
            unschedulable_wait,
            POD_READY_TIMEOUT,
        ),
        observed_at,
    ))
}

/// Await readiness of Pod `name`. While the scheduler refuses to place it the
/// wait is bounded by `unschedulable_wait` measured from Pod creation (on the
/// composed clock, never shorter than this wait) and ends in
/// `CapacityUnavailable`; `POD_READY_TIMEOUT` bounds only the start-up of a
/// scheduled Pod, measured from its last unschedulable observation (ADR-0092).
pub(super) async fn await_pod_ready(
    api: &Api<Pod>,
    name: &str,
    unschedulable_wait: Duration,
    clock: Option<&std::sync::Arc<dyn awaken_time::WallClock>>,
    expected_uid: &str,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    use awaken_container_contract::K8sPodWait;
    let (waited, mut startup) = (
        awaken_time::mono::Stopwatch::start(),
        awaken_time::mono::Stopwatch::start(),
    );
    loop {
        admit()?;
        let observed = api.get(name).await;
        admit()?;
        let pod = observed.map_err(backend)?;
        if pod.metadata.uid.as_deref() != Some(expected_uid) {
            return Err(backend(
                "Kubernetes readiness changed the exact Pod incarnation",
            ));
        }
        let unschedulable = pod_unschedulable(&pod);
        if unschedulable.is_some() {
            startup = awaken_time::mono::Stopwatch::start();
        }
        let age = pod_created_ms(&pod).zip(clock).map(|(created, clock)| {
            awaken_container_contract::k8s_pod_age(
                created,
                awaken_time::WallClockExt::now_ms(clock.as_ref()),
            )
        });
        match awaken_container_contract::k8s_pod_wait(
            pod_readiness(&pod),
            unschedulable,
            age.unwrap_or_default().max(waited.elapsed()),
            unschedulable_wait,
            startup.elapsed(),
            POD_READY_TIMEOUT,
        ) {
            K8sPodWait::Ready => return Ok(()),
            K8sPodWait::Poll => {}
            K8sPodWait::CannotStart(reason) => {
                return Err(backend(format!(
                    "Pod `{name}` cannot become ready: {reason}"
                )));
            }
            K8sPodWait::StartupTimedOut(reason) => {
                return Err(backend(format!(
                    "Pod `{name}` did not become ready within {}s: {reason}",
                    POD_READY_TIMEOUT.as_secs()
                )));
            }
            K8sPodWait::Unschedulable(message) => {
                return Err(RuntimeError::CapacityUnavailable(format!(
                    "Pod `{name}` was not scheduled within {}s: {message}",
                    unschedulable_wait.as_secs()
                )));
            }
        }
        tokio::time::sleep(POD_READY_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::api_defaults::normalize_pod_spec;
    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateRunning, ContainerStateTerminated,
        ContainerStateWaiting, ContainerStatus, PersistentVolumeClaimVolumeSource, PodSpec,
        PodStatus, Volume, VolumeMount,
    };

    use super::*;

    struct FrozenClock(u64);

    impl awaken_time::WallClock for FrozenClock {
        fn now(&self) -> awaken_time::UtcTime {
            awaken_time::UtcTime::from_epoch_ms(self.0)
        }
    }

    #[test]
    fn a_provisioning_pod_carries_the_creators_deadline_only_with_durable_evidence() {
        // Decision table: C1 the Pod records its creation instant; C2 the
        // runtime was composed with a clock. Effect: the observation's
        // deadline = creation + scheduler bound + start-up timeout, read at
        // the clock; without either fact no deadline (bounded retry).
        let created = 1_709_164_800_000;
        let mut pod = Pod::default();
        pod.metadata.creation_timestamp =
            Some(serde_json::from_value(serde_json::json!("2024-02-29T00:00:00Z")).unwrap());
        let clock: std::sync::Arc<dyn awaken_time::WallClock> =
            std::sync::Arc::new(FrozenClock(created + 5_000));
        let wait = Duration::from_secs(300);
        assert_eq!(
            pod_provisioning_deadline(&pod, wait, Some(&clock)),
            Some(awaken_provisioning_contract::ProvisioningDeadline::new(
                created + 300_000 + POD_READY_TIMEOUT.as_millis() as u64,
                created + 5_000,
            )),
            "D1 creation instant and clock"
        );
        assert_eq!(
            pod_provisioning_deadline(&pod, wait, None),
            None,
            "D2 no clock"
        );
        assert_eq!(
            pod_provisioning_deadline(&Pod::default(), wait, Some(&clock)),
            None,
            "D3 no creation instant"
        );
    }

    #[test]
    fn pod_api_default_normalization_preserves_semantic_differences() {
        // Cause/effect graph: C1 apiserver adds documented container, pull,
        // scheduling, and false read-only defaults; C2 the desired security
        // projection is unchanged/changed. Effects: E1 C1+C2 unchanged yields
        // one identical normalized PodSpec; E2 any changed security field stays
        // unequal. Rules N1 defaults only=>E1; N2 defaults+privilege change=>E2.
        let mut desired = PodSpec {
            containers: vec![Container {
                name: "agent".into(),
                image: Some("sandbox:v1".into()),
                env: Some(Vec::new()),
                volume_mounts: Some(vec![VolumeMount {
                    name: "state".into(),
                    mount_path: "/state".into(),
                    read_only: Some(false),
                    ..Default::default()
                }]),
                security_context: Some(k8s_openapi::api::core::v1::SecurityContext {
                    allow_privilege_escalation: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "state".into(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: "claim".into(),
                    read_only: Some(false),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut observed = desired.clone();
        let container = &mut observed.containers[0];
        container.image_pull_policy = Some("IfNotPresent".into());
        container.resources = Some(Default::default());
        container.termination_message_path = Some("/dev/termination-log".into());
        container.termination_message_policy = Some("File".into());
        observed.node_name = Some("worker-a".into());
        observed.dns_policy = Some("ClusterFirst".into());
        observed.enable_service_links = Some(true);
        observed.scheduler_name = Some("default-scheduler".into());
        observed.service_account = Some("default".into());
        observed.service_account_name = Some("default".into());
        observed.termination_grace_period_seconds = Some(30);
        observed.preemption_policy = Some("PreemptLowerPriority".into());
        observed.priority = Some(0);

        normalize_pod_spec(&mut desired, false);
        normalize_pod_spec(&mut observed, true);
        assert_eq!(observed, desired, "N1/E1");

        observed.containers[0]
            .security_context
            .as_mut()
            .unwrap()
            .allow_privilege_escalation = Some(true);
        assert_ne!(observed, desired, "N2/E2");
    }

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
            awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.into(),
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
            awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.into(),
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
