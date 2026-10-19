// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
use super::*;
use serde_json::{Value, json};

pub(super) fn running() -> Value {
    json!({
        "metadata": {"namespace":"ns", "name":"worker", "uid":"uid-a", "resourceVersion":"1"},
        "spec": {
            "nodeName":"node-a", "restartPolicy":"Never",
            "containers":[{"name":"worker", "image":"fixture", "securityContext":{
                "allowPrivilegeEscalation":false, "capabilities":{"drop":["ALL"]}
            }}]
        },
        "status": {"phase":"Running", "containerStatuses":[{
            "name":"worker", "image":"fixture", "imageID":"sha256:fixture", "ready":true,
            "restartCount":0, "containerID":"containerd://a",
            "state":{"running":{"startedAt":"2026-09-08T00:00:00Z"}}
        }]}
    })
}

pub(super) fn terminal() -> Value {
    let mut pod = running();
    pod["metadata"]["resourceVersion"] = json!("2");
    pod["status"]["phase"] = json!("Failed");
    pod["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
        "containerID":"containerd://a", "exitCode":137, "reason":"Error",
        "startedAt":"2026-09-08T00:00:00Z", "finishedAt":"2026-09-08T00:00:01Z"
    }});
    pod
}

pub(super) fn decode(pod: Value) -> Pod {
    serde_json::from_value(pod).unwrap()
}

#[test]
fn original_create_guard_retains_only_the_exact_pre_entry_sandbox_process() {
    // DT-CREATE-RETENTION: C1 original Create guard was present at binding;
    // C2 exact live private Pod identity; C3 same guard remains at observation;
    // C4 every exact process exits. R1=C1+C2 -> retained Sandbox binding, never
    // Worker authentication/release authority. R2=C1+C2+C3+C4 -> local exit;
    // R3 missing/substituted guard -> no retained evidence. R4 changed UID,
    // node or runtime incarnation -> Unknown. No row installs a new guard or
    // upgrades an old unretained binding; terminal status cannot mint admission.
    let guard = awaken_container_contract::ORIGINAL_GUARD_FINALIZER;
    let mut live = running();
    live["metadata"]["finalizers"] = json!([guard]);
    let binding = bind("ns", "worker", "uid-a", &decode(live)).unwrap();
    assert!(
        binding.evidence_was_retained(),
        "R1 original Create retention"
    );
    assert!(!binding.worker_evidence_was_retained(), "R1 Sandbox only");
    let mut exited = terminal();
    exited["metadata"]["finalizers"] = json!([guard]);
    assert!(
        binding_retention_is_present(&binding, &decode(exited.clone())),
        "R2"
    );
    assert!(
        matches!(
            observe(&binding, &decode(exited.clone())),
            PodProcessTermination::Observed(_)
        ),
        "R2"
    );
    for finalizers in [
        json!([]),
        json!([WORKER_PROCESS_EVIDENCE_FINALIZER]),
        json!(["other.example/keep"]),
    ] {
        let mut changed = exited.clone();
        changed["metadata"]["finalizers"] = finalizers;
        assert!(
            !binding_retention_is_present(&binding, &decode(changed)),
            "R3"
        );
    }
    for (pointer, value) in [
        ("/metadata/uid", json!("foreign")),
        ("/spec/nodeName", json!("other")),
        (
            "/status/containerStatuses/0/containerID",
            json!("containerd://other"),
        ),
    ] {
        let mut changed = exited.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            observe(&binding, &decode(changed)),
            PodProcessTermination::Unknown,
            "R4 {pointer}"
        );
    }
    let unretained = bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    assert!(
        !unretained.evidence_was_retained(),
        "R3 no retrospective upgrade"
    );
}

#[test]
fn completed_init_is_enrolled_and_continuation_retention_stays_with_its_owner() {
    // C1 private non-restarting init completed with an exact successful exit;
    // C2 it is live/restarted/rebound/failed; C3 existing continuation finalizer.
    // I1=C1+C3 => retained binding includes init + every ordinary process.
    // I2=C2 => no binding/terminal witness. I3 changed init coordinates after
    // admission => Unknown. Reusing retention grants no right to release the
    // Session's harvest/PVC finalizer (stop adapter separately tests that edge).
    let mut pod = running();
    pod["metadata"]["finalizers"] =
        json!([super::super::continuation::CONTINUATION_CLEANUP_FINALIZER]);
    let mut init = pod["spec"]["containers"][0].clone();
    init["name"] = json!("project-inputs");
    pod["spec"]["initContainers"] = json!([init]);
    let mut status = terminal()["status"]["containerStatuses"][0].clone();
    status["name"] = json!("project-inputs");
    status["containerID"] = json!("containerd://init");
    status["state"]["terminated"]["containerID"] = json!("containerd://init");
    status["state"]["terminated"]["reason"] = json!("Completed");
    status["state"]["terminated"]["exitCode"] = json!(0);
    pod["status"]["initContainerStatuses"] = json!([status]);
    let binding = bind("ns", "worker", "uid-a", &decode(pod.clone())).unwrap();
    assert!(binding.evidence_was_retained());
    assert!(!binding.worker_evidence_was_retained());
    assert_eq!(binding.init_containers.len(), 1);
    let mut exited = terminal();
    exited["metadata"]["finalizers"] = pod["metadata"]["finalizers"].clone();
    exited["spec"]["initContainers"] = pod["spec"]["initContainers"].clone();
    exited["status"]["initContainerStatuses"] = pod["status"]["initContainerStatuses"].clone();
    assert!(matches!(
        observe(&binding, &decode(exited.clone())),
        PodProcessTermination::Observed(_)
    ));
    for (pointer, value) in [
        ("/status/initContainerStatuses/0/restartCount", json!(1)),
        (
            "/status/initContainerStatuses/0/state/terminated/exitCode",
            json!(1),
        ),
        (
            "/status/initContainerStatuses/0/state/terminated/reason",
            json!("Error"),
        ),
        (
            "/status/initContainerStatuses/0/containerID",
            json!("containerd://replacement"),
        ),
    ] {
        let mut bad = exited.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            observe(&binding, &decode(bad)),
            PodProcessTermination::Unknown,
            "I2/I3 {pointer}"
        );
    }
    pod["spec"]["initContainers"][0]["restartPolicy"] = json!("Always");
    assert!(
        bind("ns", "worker", "uid-a", &decode(pod)).is_err(),
        "I2 restartable sidecar init"
    );
}

#[test]
fn terminal_observation_requires_exact_complete_irreversible_process_evidence() {
    // Cause/effect graph: exact frozen identity AND private non-restarting scope
    // AND exact runtime exit => local observation. Any missing conjunct =>
    // Unknown. Neither phase, error code nor elapsed time is sufficient.
    // R1 complete exit => observed (success, failure and OOM alike).
    // R2 live/waiting/phase-only => Unknown. R3 identity drift => Unknown.
    // R4 restart/extra participant/escape capability => Unknown.
    // R5 synthetic or incomplete exit => Unknown. Cross each changed cause with
    // an otherwise valid R1 witness (MC/DC); no remote/Dispatch effect is allowed.
    let binding = bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    for reason in ["Completed", "Error", "OOMKilled"] {
        let mut pod = terminal();
        pod["status"]["containerStatuses"][0]["state"]["terminated"]["reason"] = json!(reason);
        assert!(
            matches!(
                observe(&binding, &decode(pod)),
                PodProcessTermination::Observed(_)
            ),
            "R1 {reason}"
        );
    }
    assert_eq!(
        observe(&binding, &decode(running())),
        PodProcessTermination::Unknown,
        "R2 live"
    );
    let cases = [
        ("/metadata/namespace", json!("foreign")),
        ("/metadata/name", json!("other")),
        ("/metadata/uid", json!("replacement")),
        ("/metadata/resourceVersion", Value::Null),
        ("/spec/nodeName", json!("node-b")),
        ("/spec/restartPolicy", json!("Always")),
        ("/spec/restartPolicy", json!("OnFailure")),
        ("/spec/restartPolicy", Value::Null),
        ("/status/phase", json!("Running")),
        ("/status/phase", Value::Null),
        ("/status/containerStatuses/0/name", json!("other")),
        (
            "/status/containerStatuses/0/containerID",
            json!("containerd://b"),
        ),
        ("/status/containerStatuses/0/restartCount", json!(1)),
        ("/status/containerStatuses/0/state", json!({})),
        (
            "/status/containerStatuses/0/state/terminated/containerID",
            Value::Null,
        ),
        (
            "/status/containerStatuses/0/state/terminated/containerID",
            json!("containerd://b"),
        ),
        (
            "/status/containerStatuses/0/state/terminated/reason",
            json!("ContainerStatusUnknown"),
        ),
        (
            "/status/containerStatuses/0/state/terminated/reason",
            Value::Null,
        ),
        (
            "/status/containerStatuses/0/state/terminated/startedAt",
            Value::Null,
        ),
        (
            "/status/containerStatuses/0/state/terminated/startedAt",
            json!("2026-09-08T00:00:01Z"),
        ),
        (
            "/status/containerStatuses/0/state/terminated/finishedAt",
            Value::Null,
        ),
        (
            "/status/containerStatuses/0/state/terminated/finishedAt",
            json!("2026-09-07T00:00:00Z"),
        ),
        (
            "/spec/containers/0/securityContext/allowPrivilegeEscalation",
            json!(true),
        ),
        (
            "/spec/containers/0/securityContext/capabilities/drop",
            json!([]),
        ),
    ];
    for (pointer, value) in cases {
        let mut pod = terminal();
        *pod.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            observe(&binding, &decode(pod)),
            PodProcessTermination::Unknown,
            "R2..R5 {pointer}"
        );
    }
    for field in ["hostPID", "hostIPC", "hostNetwork", "shareProcessNamespace"] {
        let mut pod = terminal();
        pod["spec"][field] = json!(true);
        assert_eq!(
            observe(&binding, &decode(pod)),
            PodProcessTermination::Unknown,
            "R4 {field}"
        );
    }
    for (field, value) in [
        ("containers", json!([])),
        (
            "initContainers",
            json!([{"name":"init", "image":"fixture"}]),
        ),
        (
            "ephemeralContainers",
            json!([{"name":"debug", "image":"fixture"}]),
        ),
        (
            "volumes",
            json!([{"name":"escape", "hostPath":{"path":"/"}}]),
        ),
    ] {
        let mut pod = terminal();
        pod["spec"][field] = value;
        assert_eq!(
            observe(&binding, &decode(pod)),
            PodProcessTermination::Unknown,
            "R4 {field}"
        );
    }
    let mut pod = terminal();
    let extra = pod["spec"]["containers"][0].clone();
    pod["spec"]["containers"]
        .as_array_mut()
        .unwrap()
        .push(extra);
    assert_eq!(
        observe(&binding, &decode(pod)),
        PodProcessTermination::Unknown,
        "R4 extra participant"
    );
}

#[test]
fn binding_must_precede_execution_and_cannot_be_reconstructed_from_terminal_status() {
    // R6 running exact private scope => bind; R7 terminal/deleting/missing start
    // or foreign lookup => reject. This prevents post-crash identity guesses.
    let binding = bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    // R7b caller's authenticated UID is mandatory and must survive the read;
    // a same-name replacement cannot become the old caller's process domain.
    for uid in ["", "replacement"] {
        assert!(
            bind("ns", "worker", uid, &decode(running())).is_err(),
            "R7b"
        );
    }
    let serialized = serde_json::to_vec(&binding).unwrap();
    assert_eq!(
        serde_json::from_slice::<PodProcessBinding>(&serialized).unwrap(),
        binding
    );
    assert!(
        bind("ns", "worker", "uid-a", &decode(terminal())).is_err(),
        "R7 historical exit"
    );
    assert!(
        bind("other", "worker", "uid-a", &decode(running())).is_err(),
        "R7 namespace"
    );
    assert!(
        bind("ns", "other", "uid-a", &decode(running())).is_err(),
        "R7 name"
    );
    for pointer in [
        "/metadata/uid",
        "/spec/nodeName",
        "/status/containerStatuses/0/containerID",
        "/status/containerStatuses/0/state/running/startedAt",
    ] {
        let mut pod = running();
        *pod.pointer_mut(pointer).unwrap() = Value::Null;
        assert!(
            bind("ns", "worker", "uid-a", &decode(pod)).is_err(),
            "R7 {pointer}"
        );
    }
    let mut pod = running();
    pod["metadata"]["deletionTimestamp"] = json!("2026-09-08T00:00:01Z");
    assert!(
        bind("ns", "worker", "uid-a", &decode(pod)).is_err(),
        "R7 deletion"
    );
}

#[test]
fn deployment_process_exit_requires_frozen_policy_retention_and_deletion() {
    // Causes: C1 exact private original container set/UID/node/start; C2 Always
    // policy frozen before effects; C3 retained binding and live finalizer;
    // C4 accepted deletion intent; C5 every exact runtime exit; C6 zero restarts.
    // E1(C1..C6) local exit observed; E2(!C3 before admission) bind rejected;
    // E3(!C1|!C3|!C4|!C5|!C6 after admission) Unknown, no retirement.
    // Rules D1 all => observed; D2 natural exit => Unknown; D3 deletion/live =>
    // Unknown; D4 change one necessary cause => Unknown (MC/DC). D5 a legacy
    // Never binding cannot acquire Always semantics by later Pod observation.
    let mut live = running();
    live["spec"]["restartPolicy"] = json!("Always");
    assert!(
        bind("ns", "worker", "uid-a", &decode(live.clone())).is_err(),
        "D2 admission"
    );
    live["metadata"]["finalizers"] = json!([WORKER_PROCESS_EVIDENCE_FINALIZER]);
    let binding = bind("ns", "worker", "uid-a", &decode(live.clone())).unwrap();
    let serialized = serde_json::to_value(&binding).unwrap();
    assert_eq!(serialized["restartable"], true);
    assert_eq!(
        serde_json::from_value::<PodProcessBinding>(serialized).unwrap(),
        binding
    );
    let mut exited = terminal();
    exited["spec"]["restartPolicy"] = json!("Always");
    exited["metadata"]["finalizers"] = live["metadata"]["finalizers"].clone();
    assert_eq!(
        observe(&binding, &decode(exited.clone())),
        PodProcessTermination::Unknown,
        "D2 natural exit"
    );
    exited["metadata"]["deletionTimestamp"] = json!("2026-09-08T00:00:02Z");
    assert!(
        matches!(
            observe(&binding, &decode(exited.clone())),
            PodProcessTermination::Observed(_)
        ),
        "D1"
    );
    let mut deleting_live = live.clone();
    deleting_live["metadata"]["deletionTimestamp"] =
        exited["metadata"]["deletionTimestamp"].clone();
    assert_eq!(
        observe(&binding, &decode(deleting_live)),
        PodProcessTermination::Unknown,
        "D3"
    );
    for (pointer, value) in [
        ("/metadata/uid", json!("replacement")),
        ("/metadata/finalizers", json!([])),
        ("/metadata/resourceVersion", Value::Null),
        ("/spec/restartPolicy", json!("Never")),
        ("/status/containerStatuses/0/restartCount", json!(1)),
        (
            "/status/containerStatuses/0/containerID",
            json!("containerd://replacement"),
        ),
        (
            "/status/containerStatuses/0/state/terminated/reason",
            json!("ContainerStatusUnknown"),
        ),
    ] {
        let mut changed = exited.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            observe(&binding, &decode(changed)),
            PodProcessTermination::Unknown,
            "D4 {pointer}"
        );
    }
    let legacy = bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("restartable")
            .is_none()
    );
    assert_eq!(
        observe(&legacy, &decode(exited)),
        PodProcessTermination::Unknown,
        "D5"
    );
}

#[test]
fn deletion_and_legacy_exit_information_do_not_upgrade_unknown_to_stopped() {
    // R8 deleting + live => Unknown; R9 Failed + no exact exit => Unknown;
    // R10 deleting + exact completed exit => local observed only. Deletion is
    // neither required nor sufficient; old receipts never identify a new UID.
    let binding = bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    for terminal_exit in [false, true] {
        let mut pod = if terminal_exit { terminal() } else { running() };
        pod["metadata"]["deletionTimestamp"] = json!("2026-09-08T00:00:02Z");
        assert_eq!(
            matches!(
                observe(&binding, &decode(pod)),
                PodProcessTermination::Observed(_)
            ),
            terminal_exit
        );
    }
    let mut pod = running();
    pod["status"]["phase"] = json!("Failed");
    assert_eq!(
        observe(&binding, &decode(pod)),
        PodProcessTermination::Unknown,
        "R9 node lost"
    );
}

#[test]
fn all_containers_must_match_the_frozen_set_and_exit_independently_of_list_order() {
    // R11 two running containers => freeze both; R12 main exits but projector
    // lives => Unknown; R13 both exact exits => observed. R14 missing/extra/
    // duplicate status or container => Unknown. R15 mere API list reordering
    // preserves identity. This is the live k3d-discovered projector regression.
    fn add_projector(pod: &mut Value) {
        let mut spec = pod["spec"]["containers"][0].clone();
        spec["name"] = json!("projector");
        pod["spec"]["containers"].as_array_mut().unwrap().push(spec);
        let mut status = pod["status"]["containerStatuses"][0].clone();
        status["name"] = json!("projector");
        status["containerID"] = json!("containerd://projector");
        if let Some(terminated) = status["state"].get_mut("terminated") {
            terminated["containerID"] = json!("containerd://projector");
        }
        pod["status"]["containerStatuses"]
            .as_array_mut()
            .unwrap()
            .push(status);
    }
    let mut live = running();
    add_projector(&mut live);
    let binding = bind("ns", "worker", "uid-a", &decode(live.clone())).unwrap();
    let mut stopped = terminal();
    add_projector(&mut stopped);
    let mut partial = stopped.clone();
    partial["status"]["containerStatuses"][1] = live["status"]["containerStatuses"][1].clone();
    assert_eq!(
        observe(&binding, &decode(partial)),
        PodProcessTermination::Unknown,
        "R12"
    );
    assert!(
        matches!(
            observe(&binding, &decode(stopped.clone())),
            PodProcessTermination::Observed(_)
        ),
        "R13"
    );
    stopped["status"]["containerStatuses"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert!(
        matches!(
            observe(&binding, &decode(stopped.clone())),
            PodProcessTermination::Observed(_)
        ),
        "R15"
    );
    for pointer in ["/spec/containers", "/status/containerStatuses"] {
        let mut missing = stopped.clone();
        missing
            .pointer_mut(pointer)
            .unwrap()
            .as_array_mut()
            .unwrap()
            .pop();
        assert_eq!(
            observe(&binding, &decode(missing)),
            PodProcessTermination::Unknown,
            "R14 missing {pointer}"
        );
        let mut duplicate = stopped.clone();
        let values = duplicate
            .pointer_mut(pointer)
            .unwrap()
            .as_array_mut()
            .unwrap();
        values[1] = values[0].clone();
        assert_eq!(
            observe(&binding, &decode(duplicate)),
            PodProcessTermination::Unknown,
            "R14 duplicate {pointer}"
        );
    }
}
