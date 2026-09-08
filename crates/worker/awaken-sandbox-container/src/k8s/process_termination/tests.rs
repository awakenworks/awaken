use super::*;
use serde_json::{Value, json};

fn running() -> Value {
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

fn terminal() -> Value {
    let mut pod = running();
    pod["metadata"]["resourceVersion"] = json!("2");
    pod["status"]["phase"] = json!("Failed");
    pod["status"]["containerStatuses"][0]["state"] = json!({"terminated":{
        "containerID":"containerd://a", "exitCode":137, "reason":"Error",
        "startedAt":"2026-09-08T00:00:00Z", "finishedAt":"2026-09-08T00:00:01Z"
    }});
    pod
}

fn decode(pod: Value) -> Pod {
    serde_json::from_value(pod).unwrap()
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
    let binding = bind("ns", "worker", &decode(running())).unwrap();
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
    let binding = bind("ns", "worker", &decode(running())).unwrap();
    let serialized = serde_json::to_vec(&binding).unwrap();
    assert_eq!(
        serde_json::from_slice::<PodProcessBinding>(&serialized).unwrap(),
        binding
    );
    assert!(
        bind("ns", "worker", &decode(terminal())).is_err(),
        "R7 historical exit"
    );
    assert!(
        bind("other", "worker", &decode(running())).is_err(),
        "R7 namespace"
    );
    assert!(bind("ns", "other", &decode(running())).is_err(), "R7 name");
    for pointer in [
        "/metadata/uid",
        "/spec/nodeName",
        "/status/containerStatuses/0/containerID",
        "/status/containerStatuses/0/state/running/startedAt",
    ] {
        let mut pod = running();
        *pod.pointer_mut(pointer).unwrap() = Value::Null;
        assert!(bind("ns", "worker", &decode(pod)).is_err(), "R7 {pointer}");
    }
    let mut pod = running();
    pod["metadata"]["deletionTimestamp"] = json!("2026-09-08T00:00:01Z");
    assert!(bind("ns", "worker", &decode(pod)).is_err(), "R7 deletion");
}

#[test]
fn deletion_and_legacy_exit_information_do_not_upgrade_unknown_to_stopped() {
    // R8 deleting + live => Unknown; R9 Failed + no exact exit => Unknown;
    // R10 deleting + exact completed exit => local observed only. Deletion is
    // neither required nor sufficient; old receipts never identify a new UID.
    let binding = bind("ns", "worker", &decode(running())).unwrap();
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
    let binding = bind("ns", "worker", &decode(live.clone())).unwrap();
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
