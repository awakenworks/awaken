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
    pod["metadata"]["finalizers"] = json!([CONTINUATION_CLEANUP_FINALIZER]);
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

/// The same retained Always Pod after its bound container `from` ended with
/// `code`/`reason` and kubelet started container `to` in place.
pub(super) fn restarted(pod: &Value, from: &str, to: &str, code: i32, reason: &str) -> Value {
    let mut pod = pod.clone();
    let status = &mut pod["status"]["containerStatuses"][0];
    let restarts = status["restartCount"].as_i64().unwrap_or(0) + 1;
    let started = status["state"]["running"]["startedAt"].clone();
    status["lastState"] = json!({"terminated":{
        "containerID": from, "exitCode": code, "reason": reason,
        "startedAt": started, "finishedAt": "2026-09-08T00:00:30Z"
    }});
    status["containerID"] = json!(to);
    status["restartCount"] = json!(restarts);
    status["state"] = json!({"running":{"startedAt": format!("2026-09-08T00:0{restarts}:00Z")}});
    pod["metadata"]["resourceVersion"] = json!(format!("r{restarts}"));
    pod
}

pub(super) fn retained_always() -> Value {
    let mut pod = running();
    pod["spec"]["restartPolicy"] = json!("Always");
    pod["metadata"]["finalizers"] = json!([WORKER_PROCESS_EVIDENCE_FINALIZER]);
    pod
}

/// A binding persisted while Always-policy Worker Pods were Pod-UID
/// incarnations. Its fields are exactly those of a current restartable binding.
pub(super) fn historical_restartable(binding: &PodProcessBinding) -> PodProcessBinding {
    let mut encoded = serde_json::to_value(binding).unwrap();
    encoded["restartable"] = json!(true);
    serde_json::from_value(encoded).unwrap()
}

#[test]
fn container_instance_identity_decides_alive_exact_exit_lost_and_new_incarnation() {
    // A Worker incarnation is one container instance of one Pod UID. Causes:
    // container state of the bound instance A; evidence retained (R) or not.
    // | Row | Pod status (same UID, same container name)  | R | effect                          |
    // | C1  | running A                                    | y | alive: rebinding gives the same binding, Unknown |
    // | C2  | running B, lastState terminated A (137)      | y | exact exit of A from lastState; B binds as a new incarnation |
    // | C3  | terminated A in state (back-off, not restarted) | y | exact exit of A                  |
    // | C4  | running C, lastState terminated B (twice)    | y | Superseded (no exit code); C binds |
    // | C5  | running B, no lastState                      | y | Superseded (no exit code)        |
    // | C6  | lastState names A but UID/name/node differ   | y | Unknown (Pod fence)              |
    // | C7  | lastState names A with a synthetic reason or another start, newer B running | y | Superseded |
    // | C8  | A terminated with a synthetic reason, no newer instance | y | lost: Unknown          |
    // | C9  | another ID whose start is not later than A's | y | lost: Unknown                    |
    // Superseded relies on kubelet running one instance per container name in
    // a Pod UID: a newer started instance proves the bound one stopped.
    // | R2a | Always Pod without retained evidence         | n | no binding: cannot become an incarnation |
    // | R2b | retention removed after binding              | n | Unknown                          |
    // restartCount is not part of identity: only the container ID and start.
    let live = retained_always();
    let binding = bind("ns", "worker", "uid-a", &decode(live.clone())).unwrap();
    assert!(binding.restartable);
    assert_eq!(
        binding.incarnation(),
        "a",
        "single container: its runtime hex"
    );
    assert_eq!(
        bind("ns", "worker", "uid-a", &decode(live.clone())).unwrap(),
        binding,
        "C1 alive"
    );
    assert_eq!(
        observe(&binding, &decode(live.clone())),
        PodProcessTermination::Unknown,
        "C1"
    );

    let once = restarted(&live, "containerd://a", "containerd://b", 137, "Error");
    let PodProcessTermination::Observed(receipt) = observe(&binding, &decode(once.clone())) else {
        panic!("C2 exact lastState exit");
    };
    assert_eq!(receipt.exits().len(), 1);
    assert_eq!(receipt.exits()[0].exit_code(), 137, "C2 exact code");
    assert_eq!(receipt.exits()[0].reason(), "Error", "C2 exact reason");
    assert!(
        receipt.evidence_ref().contains("\"exit_code\":137"),
        "C2 durable"
    );
    let PodProcessTermination::Observed(again) = observe(&binding, &decode(once.clone())) else {
        panic!("C2 replay");
    };
    assert_eq!(
        again.evidence_ref(),
        receipt.evidence_ref(),
        "C2 stable evidence"
    );
    let successor = bind("ns", "worker", "uid-a", &decode(once.clone())).unwrap();
    assert_eq!(successor.incarnation(), "b", "C2 new incarnation binds");
    assert_ne!(successor, binding);

    let mut backoff = live.clone();
    backoff["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
        "containerID":"containerd://a", "exitCode": 1, "reason":"Error",
        "startedAt":"2026-09-08T00:00:00Z", "finishedAt":"2026-09-08T00:00:01Z"
    }});
    assert!(
        matches!(
            observe(&binding, &decode(backoff)),
            PodProcessTermination::Observed(_)
        ),
        "C3"
    );

    let twice = restarted(&once, "containerd://b", "containerd://c", 137, "Error");
    let PodProcessTermination::Superseded(superseded) = observe(&binding, &decode(twice.clone()))
    else {
        panic!("C4 a newer instance supersedes A");
    };
    assert_eq!(superseded.binding(), &binding, "C4 subject");
    let mut rerestarted = restarted(&twice, "containerd://c", "containerd://d", 1, "Error");
    rerestarted["metadata"]["resourceVersion"] = json!("r9");
    let PodProcessTermination::Superseded(again) = observe(&binding, &decode(rerestarted)) else {
        panic!("C4 replay after another restart");
    };
    assert_eq!(
        again.evidence_ref(),
        superseded.evidence_ref(),
        "C4 evidence names only A, so later restarts replay it"
    );
    assert!(
        superseded
            .evidence_ref()
            .starts_with("k8s-container-superseded:v1:")
            && !superseded.evidence_ref().contains("exit_code"),
        "C4 no exit code"
    );
    assert_eq!(
        bind("ns", "worker", "uid-a", &decode(twice))
            .unwrap()
            .incarnation(),
        "c",
        "C4 current instance binds"
    );
    let mut no_last = once.clone();
    no_last["status"]["containerStatuses"][0]["lastState"] = json!({});
    assert!(
        matches!(
            observe(&binding, &decode(no_last)),
            PodProcessTermination::Superseded(_)
        ),
        "C5"
    );
    for (pointer, value) in [
        ("/metadata/uid", json!("uid-b")),
        ("/spec/nodeName", json!("node-b")),
        ("/spec/restartPolicy", json!("Never")),
        ("/status/containerStatuses/0/name", json!("other")),
    ] {
        let mut changed = once.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            observe(&binding, &decode(changed)),
            PodProcessTermination::Unknown,
            "C6 {pointer}"
        );
    }
    for (pointer, value) in [
        (
            "/status/containerStatuses/0/lastState/terminated/reason",
            json!("ContainerStatusUnknown"),
        ),
        (
            "/status/containerStatuses/0/lastState/terminated/startedAt",
            json!("2026-09-07T00:00:00Z"),
        ),
    ] {
        let mut changed = once.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            matches!(
                observe(&binding, &decode(changed)),
                PodProcessTermination::Superseded(_)
            ),
            "C7 {pointer}: no exact exit of A, but newer B runs"
        );
    }
    let mut synthetic = live.clone();
    synthetic["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
        "containerID":"containerd://a", "exitCode": 137, "reason":"ContainerStatusUnknown",
        "startedAt":"2026-09-08T00:00:00Z", "finishedAt":"2026-09-08T00:00:01Z"
    }});
    assert_eq!(
        observe(&binding, &decode(synthetic)),
        PodProcessTermination::Unknown,
        "C8"
    );
    let mut not_later = once.clone();
    not_later["status"]["containerStatuses"][0]["lastState"] = json!({});
    not_later["status"]["containerStatuses"][0]["state"]["running"]["startedAt"] =
        json!("2026-09-08T00:00:00Z");
    assert_eq!(
        observe(&binding, &decode(not_later)),
        PodProcessTermination::Unknown,
        "C9"
    );
    let mut unretained = live.clone();
    unretained["metadata"]["finalizers"] = json!([]);
    assert!(
        bind("ns", "worker", "uid-a", &decode(unretained)).is_err(),
        "R2a"
    );
    let mut released = once.clone();
    released["metadata"]["finalizers"] = json!([]);
    assert_eq!(
        observe(&binding, &decode(released)),
        PodProcessTermination::Unknown,
        "R2b"
    );
}

#[test]
fn historical_bindings_settle_by_their_recorded_policy() {
    // Migration rule. M1 a Pod-level restartable binding persisted before
    // container identity has exactly today's restartable fields, so it decodes
    // unchanged and settles by container ID: exact from `state`/`lastState`,
    // lost otherwise. M2 a historical Never binding keeps its original rule:
    // every exact exit plus a terminal phase; a container ID alone in
    // `lastState` of a Never Pod cannot exist. M3 a historical Pod-UID
    // incarnation still owns its binding (`belongs_to_incarnation`).
    let never = bind("ns", "worker", "uid-a", &decode(running())).unwrap();
    let mut retained_never = running();
    retained_never["metadata"]["finalizers"] = json!([WORKER_PROCESS_EVIDENCE_FINALIZER]);
    let historical =
        historical_restartable(&bind("ns", "worker", "uid-a", &decode(retained_never)).unwrap());
    let current = bind("ns", "worker", "uid-a", &decode(retained_always())).unwrap();
    assert_eq!(historical, current, "M1 identical persisted fields");
    assert_eq!(
        serde_json::from_value::<PodProcessBinding>(serde_json::to_value(&historical).unwrap())
            .unwrap(),
        historical,
        "M1 decodes"
    );
    let once = restarted(
        &retained_always(),
        "containerd://a",
        "containerd://b",
        137,
        "Error",
    );
    assert!(
        matches!(
            observe(&historical, &decode(once)),
            PodProcessTermination::Observed(_)
        ),
        "M1 historical restartable settles from lastState"
    );
    assert!(
        matches!(
            observe(&never, &decode(terminal())),
            PodProcessTermination::Observed(_)
        ),
        "M2 Never exit with terminal phase"
    );
    let mut never_live_phase = terminal();
    never_live_phase["status"]["phase"] = json!("Running");
    assert_eq!(
        observe(&never, &decode(never_live_phase)),
        PodProcessTermination::Unknown,
        "M2 Never still needs the terminal phase"
    );
    assert!(
        never.belongs_to_incarnation("uid-a", "uid-a"),
        "M3 Pod-UID incarnation"
    );
    assert!(
        never.belongs_to_incarnation("uid-a", "a"),
        "M3 container incarnation"
    );
    assert!(
        !never.belongs_to_incarnation("uid-a", "b"),
        "M3 another instance"
    );
    assert!(
        !never.belongs_to_incarnation("uid-b", "a"),
        "M3 another Pod"
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

type PodSpecMutation = (&'static str, fn(&mut k8s_openapi::api::core::v1::PodSpec));

#[test]
fn sandbox_namespace_predicate_rejects_only_boundary_weakening_shapes() {
    /* Namespace-shape decision table: C1=host network/PID/IPC requested;
     * C2=process namespace shared; C3=one ephemeral container injected;
     * C4=fields absent, explicitly false, and ephemeral list empty.
     * E1=C1|C2|C3 rejects the private Sandbox shape; E2=C4 remains
     * compatible with Kubernetes defaulting. Rules PS1 C1=>E1;
     * PS2 C2=>E1; PS3 C3=>E1; PS4 C4=>E2.
     */
    let mut compatible = k8s_openapi::api::core::v1::PodSpec {
        containers: Vec::new(),
        host_network: Some(false),
        host_pid: Some(false),
        host_ipc: Some(false),
        share_process_namespace: Some(false),
        ephemeral_containers: Some(Vec::new()),
        ..Default::default()
    };
    assert!(
        !has_forbidden_sandbox_namespace_shape(&compatible),
        "PS4/E2"
    );

    let mutations: [PodSpecMutation; 5] = [
        ("host network", |spec| spec.host_network = Some(true)),
        ("host PID", |spec| spec.host_pid = Some(true)),
        ("host IPC", |spec| spec.host_ipc = Some(true)),
        ("shared process namespace", |spec| {
            spec.share_process_namespace = Some(true)
        }),
        ("ephemeral container", |spec| {
            spec.ephemeral_containers = Some(vec![Default::default()])
        }),
    ];
    for (cause, mutate) in mutations {
        mutate(&mut compatible);
        assert!(
            has_forbidden_sandbox_namespace_shape(&compatible),
            "PS1-3/E1 {cause}"
        );
        compatible.host_network = Some(false);
        compatible.host_pid = Some(false);
        compatible.host_ipc = Some(false);
        compatible.share_process_namespace = Some(false);
        compatible.ephemeral_containers = Some(Vec::new());
    }
}
