use super::*;

/// Wire written by the closed payload enum that previously lived in the
/// neutral contract, generated from that implementation (never hand-edited).
const HISTORICAL_WIRE: &[(&str, &str)] = &[
    (
        "container_v1_docker",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v1","container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"]}}"#,
    ),
    (
        "container_v1_k8s_legacy",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v1","container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"kubernetes_continuation","claim_uid":"claim-a"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]}}"#,
    ),
    (
        "container_v2_k8s",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v2","previous":{"container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"kubernetes_continuation_v2","pod_uid":"pod-uid-a","claim_uid":"claim-a"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]},"adoption_fingerprint":"fp-adoption","realization_fingerprint":"fp-realization","owned_paths":["/workspace/repo"]},"memory_materializations":[{"store_id":"memory-a","mount_path":"/mnt/memory","heads":[{"path":"notes.md","id":"head-1","version":3,"content_sha256":"sha-1"}]}]}"#,
    ),
    (
        "container_v2_k8s_no_claim",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v2","previous":{"container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"kubernetes_continuation_v2","pod_uid":"pod-uid-b"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]},"adoption_fingerprint":"fp-adoption","realization_fingerprint":"fp-realization","owned_paths":[]}}"#,
    ),
    (
        "container_v2_host_bind",
        r#"{"sandbox_id":"s-c","restoration":{"effect_id":"effect-a","generation_id":"generation-a","checkpoint_id":"checkpoint-a","checkpoint_digest":"sha256:digest-a","sandbox_spec_fingerprint":"spec-a","checkpoint_exclusions_fingerprint":"exclusions-a"},"payload":{"schema":"container_v2","previous":{"container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"host_bind_restoration","staging_root":"/provider/staging/a"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]},"adoption_fingerprint":"fp-adoption","realization_fingerprint":"fp-realization","owned_paths":["/workspace/repo"]}}"#,
    ),
];

fn text(name: &str) -> &'static str {
    HISTORICAL_WIRE
        .iter()
        .find_map(|(row, text)| (*row == name).then_some(*text))
        .unwrap()
}

fn golden(name: &str) -> pc::SandboxHandle {
    serde_json::from_str(text(name)).unwrap()
}

fn fingerprint(value: &str) -> pc::SandboxRealizationFingerprint {
    serde_json::from_value(serde_json::json!(value)).unwrap()
}

fn container_v1(runtime_handle: Option<ContainerContinuationHandle>) -> ContainerSandboxHandleV1 {
    ContainerSandboxHandleV1 {
        container_id: "container-a".into(),
        outputs_path: "/mnt/session/outputs".into(),
        base_env: serde_json::from_value(serde_json::json!([
            {"name": "TZ", "value": {"kind": "inline", "value": "UTC"}, "visibility": "process"},
            {"name": "TOKEN", "value": {"kind": "secret", "reference": "ref-1"}, "visibility": "egress_only"}
        ]))
        .unwrap(),
        live_input_projection: true,
        continuation_excluded_paths: vec!["/mnt/memory".into()],
        runtime_handle,
        sandbox_control_incarnation: Some(SandboxControlIncarnation::kubernetes_pod("pod-uid-a").unwrap()),
        control_services: [SandboxControlServiceKind::RepositoryGitCredential]
            .into_iter()
            .collect(),
    }
}

fn container_v2(
    runtime_handle: Option<ContainerContinuationHandle>,
    owned_paths: Vec<String>,
) -> ContainerSandboxHandleV2 {
    ContainerSandboxHandleV2 {
        previous: container_v1(runtime_handle),
        adoption_fingerprint: fingerprint("fp-adoption"),
        realization_fingerprint: fingerprint("fp-realization"),
        owned_paths,
    }
}

#[test]
fn historical_container_handles_decode_to_the_same_typed_payloads_and_bytes() {
    // Old format -> new model -> re-encode. C1 each historical container shape
    // (Docker/Podman id, legacy claim-only Kubernetes, current Pod+claim and
    // Pod-only Kubernetes, restoring HostBind). E1 the codec decodes the exact
    // typed locator; E2 the typed writer produces byte-identical wire; E3 the
    // physical incarnation and adoption evidence keep their former decisions.
    let memory = pc::MemoryMaterializationEvidence::new(
        "memory-a",
        "/mnt/memory",
        vec![pc::MemoryMaterializationHead {
            path: "notes.md".into(),
            id: "head-1".into(),
            version: 3,
            content_sha256: "sha-1".into(),
        }],
    )
    .unwrap();
    let k8s_v2 = ContainerContinuationHandle::KubernetesContinuationV2 {
        pod_uid: "pod-uid-a".into(),
        claim_uid: Some("claim-a".into()),
    };
    let written = [
        (
            "container_v1_docker",
            pc::SandboxHandle::container(
                "s-c",
                ContainerSandboxHandleV1 {
                    sandbox_control_incarnation: None,
                    control_services: BTreeSet::new(),
                    ..container_v1(None)
                },
            ),
        ),
        (
            "container_v1_k8s_legacy",
            pc::SandboxHandle::container(
                "s-c",
                container_v1(Some(ContainerContinuationHandle::KubernetesContinuation {
                    claim_uid: "claim-a".into(),
                })),
            ),
        ),
        (
            "container_v2_k8s",
            pc::SandboxHandle::container_v2(
                "s-c",
                container_v2(Some(k8s_v2), vec!["/workspace/repo".into()]),
            )
            .with_memory_materializations(vec![memory])
            .unwrap(),
        ),
        (
            "container_v2_k8s_no_claim",
            pc::SandboxHandle::container_v2(
                "s-c",
                container_v2(
                    Some(ContainerContinuationHandle::KubernetesContinuationV2 {
                        pod_uid: "pod-uid-b".into(),
                        claim_uid: None,
                    }),
                    Vec::new(),
                ),
            ),
        ),
    ];
    for (name, handle) in written {
        assert_eq!(
            serde_json::to_string(&handle).unwrap(),
            text(name),
            "E2 {name}"
        );
        assert_eq!(golden(name), handle, "E1 {name}");
        assert_eq!(
            golden(name).container_payload().unwrap(),
            handle.container_payload().unwrap(),
            "E1 {name}"
        );
    }
    let incarnations: [(&str, Result<&str, &str>, Option<&str>); 5] = [
        ("container_v1_docker", Err("Unsupported"), None),
        ("container_v1_k8s_legacy", Err("Unsupported"), None),
        ("container_v2_k8s", Ok("pod-uid-a"), Some("fp-adoption")),
        (
            "container_v2_k8s_no_claim",
            Ok("pod-uid-b"),
            Some("fp-adoption"),
        ),
        (
            "container_v2_host_bind",
            Err("Invalid"),
            Some("fp-adoption"),
        ),
    ];
    for (name, incarnation, adoption) in incarnations {
        let handle = golden(name);
        match (handle.container_physical_incarnation(), incarnation) {
            (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "E3 {name}"),
            (Err(pc::SandboxError::Unsupported(_)), Err("Unsupported"))
            | (Err(pc::SandboxError::Invalid(_)), Err("Invalid")) => {}
            (actual, expected) => panic!("E3 {name}: {actual:?} vs {expected:?}"),
        }
        assert_eq!(
            handle.container_adoption_fingerprint().unwrap(),
            adoption.map(fingerprint),
            "E3 {name}"
        );
    }
    let docker_v2 = pc::SandboxHandle::container_v2("s-c", container_v2(None, Vec::new()));
    assert_eq!(
        docker_v2.container_physical_incarnation().unwrap(),
        "container-a",
        "E3 Docker"
    );
    assert_eq!(docker_v2.provider_kind(), "container");
}

#[test]
fn legacy_container_defaults_normalize_only_through_the_owning_codec() {
    // C1 a pre-default container V1 payload omits `continuation_excluded_paths`.
    // E1 the neutral envelope keeps the stored bytes; E2 the codec decodes the
    // historical default; E3 a provider-rebuilt handle writes the normalized
    // shape, exactly as the former typed decoder re-encoded it.
    let stored = r#"{"sandbox_id":"s","payload":{"schema":"container_v1","container_id":"c","outputs_path":"/o","base_env":[],"live_input_projection":false}}"#;
    let decoded: pc::SandboxHandle = serde_json::from_str(stored).unwrap();
    assert_eq!(serde_json::to_string(&decoded).unwrap(), stored, "E1");
    let payload = decoded.container_payload().unwrap();
    assert!(payload.continuation_excluded_paths.is_empty(), "E2");
    assert_eq!(
        serde_json::to_string(&pc::SandboxHandle::container("s", payload)).unwrap(),
        r#"{"sandbox_id":"s","payload":{"schema":"container_v1","container_id":"c","outputs_path":"/o","base_env":[],"live_input_projection":false,"continuation_excluded_paths":[]}}"#,
        "E3"
    );
}

#[test]
fn container_codec_rejects_foreign_and_malformed_payloads() {
    let edit = |name: &str, edit: &dyn Fn(&mut serde_json::Value)| {
        let mut wire: serde_json::Value = serde_json::from_str(text(name)).unwrap();
        edit(&mut wire["payload"]);
        serde_json::from_value::<pc::SandboxHandle>(wire)
    };
    let missing_adoption = edit("container_v2_k8s", &|payload| {
        payload
            .as_object_mut()
            .unwrap()
            .remove("adoption_fingerprint");
    })
    .expect("the neutral envelope does not interpret the container locator");
    assert!(
        missing_adoption.container_adoption_fingerprint().is_err()
            && missing_adoption.container_payload().is_err()
            && missing_adoption.container_physical_incarnation().is_err(),
        "R1 current container V2 never accepts missing adoption evidence"
    );
    let empty_pod = edit("container_v2_k8s", &|payload| {
        payload["previous"]["runtime_handle"]["pod_uid"] = serde_json::json!(" ");
    })
    .unwrap();
    assert!(
        matches!(
            empty_pod.container_physical_incarnation(),
            Err(pc::SandboxError::Invalid(_))
        ),
        "R2 empty Pod incarnation"
    );
    let foreign: pc::SandboxHandle = serde_json::from_str(
        r#"{"sandbox_id":"s","payload":{"schema":"local_v1","outputs_path":"/o","base_env":[],"continuation_excluded_paths":[]}}"#,
    )
    .unwrap();
    assert!(
        matches!(
            foreign.container_payload(),
            Err(pc::SandboxError::Invalid(_))
        ),
        "R3 another provider"
    );
    assert!(foreign.container_adoption_fingerprint().is_err(), "R3");
    assert!(
        pc::SandboxHandle::new("container", "s")
            .container_payload()
            .is_err(),
        "R4 unmanaged"
    );
    let future = edit("container_v2_k8s", &|payload| {
        payload["schema"] = serde_json::json!("container_v3");
    })
    .unwrap();
    assert!(future.container_payload().is_err(), "R5 unknown version");
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySandboxHandle {
    #[allow(dead_code)]
    sandbox_id: String,
    #[allow(dead_code)]
    payload: LegacySandboxHandlePayload,
}

#[derive(serde::Deserialize)]
#[serde(tag = "schema", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum LegacySandboxHandlePayload {
    Unmanaged {
        #[allow(dead_code)]
        provider_kind: String,
    },
    ContainerV1(LegacyContainerSandboxHandleV1),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyContainerSandboxHandleV1 {
    #[allow(dead_code)]
    container_id: String,
    #[allow(dead_code)]
    outputs_path: String,
    #[allow(dead_code)]
    base_env: Vec<pc::EnvVar>,
    #[allow(dead_code)]
    live_input_projection: bool,
    #[allow(dead_code)]
    #[serde(default)]
    continuation_excluded_paths: Vec<String>,
    #[allow(dead_code)]
    runtime_handle: Option<LegacyContainerContinuationHandle>,
    #[allow(dead_code)]
    #[serde(default)]
    sandbox_control_incarnation: Option<SandboxControlIncarnation>,
    #[allow(dead_code)]
    #[serde(default)]
    control_services: BTreeSet<SandboxControlServiceKind>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum LegacyContainerContinuationHandle {
    KubernetesContinuation {
        #[allow(dead_code)]
        claim_uid: String,
    },
}

fn future_evidence() -> serde_json::Value {
    serde_json::json!({
        "effect_id": "effect-a",
        "generation_id": "generation-a",
        "checkpoint_id": "checkpoint-a",
        "checkpoint_digest": "sha256:digest-a",
        "sandbox_spec_fingerprint": "spec-a",
        "checkpoint_exclusions_fingerprint": "exclusions-a"
    })
}

fn future_container(runtime_handle: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "sandbox_id": "sandbox-a",
        "restoration": future_evidence(),
        "payload": {
            "schema": "container_v1",
            "container_id": "container-a",
            "outputs_path": "/outputs",
            "base_env": [],
            "live_input_projection": true,
            "continuation_excluded_paths": [],
            "runtime_handle": runtime_handle
        }
    })
}

#[test]
fn sandbox_control_incarnation_is_typed_and_legacy_handle_compatible() {
    /* Incarnation wire cause/effect table:
     * C1=legacy ContainerV1 handle omits control evidence; C2=valid Pod
     * UID; C3=empty/line-injected UID; C4=unknown incarnation kind.
     * E1=decode as None and omit on re-encode; E2=lossless typed roundtrip;
     * E3=reject before adoption. Rules: I1 C1=>E1; I2 C2=>E2;
     * I3 C3|C4=>E3.
     */
    let legacy = serde_json::json!({
        "sandbox_id": "sandbox-a",
        "payload": {
            "schema": "container_v1",
            "container_id": "pod-a",
            "outputs_path": "/outputs",
            "base_env": [],
            "live_input_projection": false,
            "continuation_excluded_paths": []
        }
    });
    let decoded: pc::SandboxHandle = serde_json::from_value(legacy).expect("I1/E1");
    let payload = decoded.container_payload().expect("I1 container payload");
    assert!(payload.sandbox_control_incarnation.is_none(), "I1/E1");
    assert!(payload.control_services.is_empty(), "I1/E1");
    assert!(
        ["sandbox_control_incarnation", "control_services"]
            .into_iter()
            .all(|field| !serde_json::to_string(&decoded).unwrap().contains(field)),
        "I1/E1 empty evidence omitted"
    );

    let incarnation = SandboxControlIncarnation::kubernetes_pod("pod-uid-a").unwrap();
    let encoded = serde_json::to_value(&incarnation).unwrap();
    assert_eq!(
        serde_json::from_value::<SandboxControlIncarnation>(encoded).unwrap(),
        incarnation,
        "I2/E2"
    );
    assert!(
        SandboxControlIncarnation::kubernetes_pod("").is_err(),
        "I3/E3"
    );
    assert!(
        SandboxControlIncarnation::kubernetes_pod("uid\nother").is_err(),
        "I3/E3"
    );
    assert!(
        serde_json::from_value::<SandboxControlIncarnation>(serde_json::json!({
            "kind": "opaque",
            "uid": "pod-uid-a"
        }))
        .is_err(),
        "I3/E3 closed kind"
    );
}

/*
 * Nested rollout decision table. C1 legacy Kubernetes locator; C2 current
 * KubernetesContinuationV2 locator with Pod and optional claim identity; C3
 * future HostBind locator plus Some evidence; C4 the actual old nested enum
 * reader. Effects: E1 C1 stays readable, E2 the new reader losslessly accepts
 * C2, E3 the new reader losslessly accepts C3, E4 C4 rejects C2/C3 rather
 * than silently dropping either physical locator. Rules: R8=C1=>E1,
 * R9=C2=>E2, R10=C3=>E3, R11=(C2|C3)+C4=>E4.
 */
#[test]
fn future_host_bind_is_reader_only_and_legacy_nested_enum_rejects_it() {
    let current_kubernetes = future_container(serde_json::json!({
        "kind": "kubernetes_continuation_v2",
        "pod_uid": "pod-uid-a",
        "claim_uid": "claim-uid-a"
    }));
    let decoded_kubernetes: pc::SandboxHandle =
        serde_json::from_value(current_kubernetes.clone()).expect("R9/E2 decode");
    match decoded_kubernetes
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
        .expect("R9/E2 runtime locator")
    {
        ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, claim_uid } => {
            assert_eq!(pod_uid, "pod-uid-a", "R9/E2 Pod identity");
            assert_eq!(claim_uid.as_deref(), Some("claim-uid-a"), "R9/E2 claim");
        }
        ContainerContinuationHandle::KubernetesContinuation { .. }
        | ContainerContinuationHandle::HostBindRestoration(_) => panic!("R9/E2 wrong kind"),
    }
    assert_eq!(
        serde_json::to_value(&decoded_kubernetes).unwrap(),
        current_kubernetes,
        "R9/E2"
    );

    let future = future_container(serde_json::json!({
        "kind": "host_bind_restoration",
        "staging_root": "/provider/staging/a"
    }));
    let decoded: pc::SandboxHandle = serde_json::from_value(future.clone()).expect("R10/E3 decode");
    assert!(decoded.restoration().is_some(), "R10/E3 evidence");
    let runtime_handle = decoded
        .container_payload()
        .unwrap()
        .runtime_handle
        .expect("R10/E3 runtime locator");
    match &runtime_handle {
        ContainerContinuationHandle::HostBindRestoration(locator) => {
            assert_eq!(locator.staging_root(), "/provider/staging/a", "R10/E3")
        }
        ContainerContinuationHandle::KubernetesContinuation { .. }
        | ContainerContinuationHandle::KubernetesContinuationV2 { .. } => {
            panic!("R10/E3 wrong kind")
        }
    }
    assert_eq!(serde_json::to_value(decoded).unwrap(), future, "R10/E3");

    let mut legacy_kubernetes = future.clone();
    legacy_kubernetes
        .as_object_mut()
        .unwrap()
        .remove("restoration");
    legacy_kubernetes["payload"]["runtime_handle"] = serde_json::json!({
        "kind": "kubernetes_continuation",
        "claim_uid": "claim-a"
    });
    serde_json::from_value::<LegacySandboxHandle>(legacy_kubernetes).expect("R8/E1");

    let mut legacy_kubernetes_v2 = current_kubernetes;
    legacy_kubernetes_v2
        .as_object_mut()
        .unwrap()
        .remove("restoration");
    assert!(
        serde_json::from_value::<LegacySandboxHandle>(legacy_kubernetes_v2).is_err(),
        "R11/E4 current Kubernetes locator"
    );

    let mut legacy_host_bind = future;
    legacy_host_bind
        .as_object_mut()
        .unwrap()
        .remove("restoration");
    assert!(
        serde_json::from_value::<LegacySandboxHandle>(legacy_host_bind).is_err(),
        "R11/E4 HostBind locator"
    );
}
