use super::*;
use crate::kubernetes_handle::{KubernetesProjectedKind, KubernetesProjectedObject};

#[test]
fn original_projected_set_cutover_preserves_history_and_rejects_partial_evidence() {
    // Feature closure F1/F2: with K8s off the common codec/values still compile;
    // only the physical adapter's attempt accessor is gated. With K8s on the
    // same decoder and physical membership checks compile. The canonical
    // process-assembly-production and all-feature Clippy graphs cover these
    // causes; the wire decision table below keeps one codec in both graphs.
    // Causes: C1 complete empty/nonempty original set; C2 missing/blank UID,
    // duplicate key, foreign namespace/Pod, partial origin or missing list;
    // C3 wire schema 3 versus historical schema 1/2. Effects: E1 C1+schema3
    // roundtrips and retains ordinary fingerprints; E2 any C2 refuses decoding
    // before an effect; E3 C1 cannot be relabelled schema1/2. Rows H1=complete
    // empty, H2=complete nonempty -> E1; H3-H10=each independent bad axis ->E2;
    // H11=old schema+new carrier ->E3. Historical golden bytes below remain
    // unchanged; missing historical evidence stays None, never complete empty.
    // HC1: current V3's outer Control UID must equal the same original Pod UID;
    // mismatched identities reject before effect. Historical V1/V2 golden rows
    // remain readable byte-for-byte and are never upgraded into V3 evidence.
    for objects in [
        Vec::new(),
        vec![KubernetesProjectedObject {
            kind: KubernetesProjectedKind::Secret,
            name: "credential-0".into(),
            uid: "secret-uid".into(),
            owner_references_fingerprint: "owner-digest".into(),
        }],
    ] {
        let participants = KubernetesProjectedParticipants::new(
            "default".into(),
            "pod-uid".into(),
            objects,
            Some("original-attempt".into()),
            None,
        )
        .unwrap();
        let mut payload = container_v2(
            Some(ContainerContinuationHandle::KubernetesContinuationV3 {
                pod_uid: "pod-uid".into(),
                claim_uid: None,
                participants: Box::new(participants),
            }),
            Vec::new(),
        );
        // Current V3 has one original UID; the historical V1/V2 helper keeps
        // its original golden UID and bytes independently below.
        payload.previous.sandbox_control_incarnation =
            Some(SandboxControlIncarnation::kubernetes_pod("pod-uid").unwrap());
        let current = pc::SandboxHandle::container_v2("scope", payload);
        let encoded = serde_json::to_value(&current).unwrap();
        assert_eq!(
            encoded["payload"]["schema"], "container_v3",
            "H1/H2 atomic writer cutover"
        );
        let decoded: pc::SandboxHandle = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(
            decoded.container_payload().unwrap(),
            current.container_payload().unwrap()
        );
        assert_eq!(decoded.container_physical_incarnation().unwrap(), "pod-uid");
        for axis in [
            "missing-list",
            "missing-pod",
            "blank-uid",
            "duplicate",
            "partial-origin",
            "foreign-pod",
            "partial-fingerprint",
            "invalid-name",
            "old-schema",
            "outer-restore",
            "control-uid",
        ] {
            let mut changed = encoded.clone();
            let projection = &mut changed["payload"]["previous"]["runtime_handle"]["participants"];
            match axis {
                "control-uid" => {
                    changed["payload"]["previous"]["sandbox_control_incarnation"]["uid"] =
                        "foreign-control-pod".into()
                }
                "missing-list" => {
                    projection.as_object_mut().unwrap().remove("objects");
                }
                "missing-pod" => {
                    projection.as_object_mut().unwrap().remove("pod_uid");
                }
                "blank-uid" => {
                    projection["objects"] = serde_json::json!([{"kind":"secret","name":"credential-0","uid":" ","owner_references_fingerprint":"owner-digest"}])
                }
                "duplicate" => {
                    projection["objects"] = serde_json::json!([{"kind":"secret","name":"credential-0","uid":"a","owner_references_fingerprint":"owner-digest"},{"kind":"secret","name":"credential-0","uid":"b","owner_references_fingerprint":"owner-digest"}])
                }
                "partial-origin" => {
                    projection["origin"] =
                        serde_json::json!({"kind":"restoration","plan_fingerprint":"plan"})
                }
                "foreign-pod" => projection["pod_uid"] = "another-pod".into(),
                "invalid-name" => {
                    projection["objects"] = serde_json::json!([{"kind":"secret","name":"credential?query=1","uid":"a","owner_references_fingerprint":"owner-digest"}])
                }
                "partial-fingerprint" => {
                    changed["payload"]
                        .as_object_mut()
                        .unwrap()
                        .remove("adoption_fingerprint");
                }
                "old-schema" => changed["payload"]["schema"] = "container_v2".into(),
                "outer-restore" => changed["restoration"] = future_evidence(),
                _ => unreachable!(),
            }
            let refused = serde_json::from_value::<pc::SandboxHandle>(changed)
                .and_then(|handle| handle.container_payload().map_err(serde::de::Error::custom));
            assert!(refused.is_err(), "H3-H11/{axis}");
        }
    }
}

#[test]
fn restored_original_set_never_creates_ordinary_realization_authority() {
    // H12 causes: exact restore tuple+plan+original complete set, with no normal
    // Create attempt/fingerprints. Effects: schema3 roundtrip keeps the restore
    // origin, adoption fingerprint remains None, ordinary incarnation authority
    // is refused. H13 inserting a normal attempt alongside restore -> reject.
    let evidence: pc::SandboxRestorationEvidence =
        serde_json::from_value(future_evidence()).unwrap();
    let participants = KubernetesProjectedParticipants::new(
        "default".into(),
        "pod-uid".into(),
        Vec::new(),
        None,
        Some((evidence.clone(), "plan".into())),
    )
    .unwrap();
    let mut payload = container_v1(Some(
        ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid: "pod-uid".into(),
            claim_uid: None,
            participants: Box::new(participants),
        },
    ));
    payload.sandbox_control_incarnation =
        Some(SandboxControlIncarnation::kubernetes_pod("pod-uid").unwrap());
    let handle = pc::SandboxHandle::container("scope", payload);
    let mut value = serde_json::to_value(&handle).unwrap();
    value["restoration"] = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["payload"]["schema"], "container_v3");
    let decoded: pc::SandboxHandle = serde_json::from_value(value.clone()).unwrap();
    assert!(decoded.container_payload().is_ok(), "H12");
    assert!(decoded.container_adoption_fingerprint().unwrap().is_none());
    assert!(decoded.container_physical_incarnation().is_err());
    let mut wrong_outer = value.clone();
    wrong_outer["restoration"]["effect_id"] = "foreign-effect".into();
    let decoded: pc::SandboxHandle = serde_json::from_value(wrong_outer).unwrap();
    assert!(
        decoded.container_payload().is_err(),
        "H14 inner/outer origin mismatch"
    );
    let mut changed = value;
    changed["payload"]["previous"]["runtime_handle"]["participants"]["origin"]["attempt"] =
        "fabricated-normal".into();
    let decoded: pc::SandboxHandle = serde_json::from_value(changed).unwrap();
    assert!(decoded.container_payload().is_err(), "H13");
}

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
        | ContainerContinuationHandle::KubernetesContinuationV3 { .. }
        | ContainerContinuationHandle::HostBindRestoration(_) => panic!("R9/E2 wrong kind"),
        ContainerContinuationHandle::KubernetesGuardedClaim(_) => panic!("R9/E2 wrong kind"),
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
        | ContainerContinuationHandle::KubernetesContinuationV2 { .. }
        | ContainerContinuationHandle::KubernetesContinuationV3 { .. } => {
            panic!("R10/E3 wrong kind")
        }
        ContainerContinuationHandle::KubernetesGuardedClaim(_) => panic!("R10/E3 wrong kind"),
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
#[test]
fn guarded_claim_handle_keeps_one_typed_terminal_incarnation() {
    // Causes: C1 a joined Pod reservation has one Isolated UID and a retained
    // original claim UID; C2 the terminal-only handle is encoded and reopened;
    // C3 the reservation is InFlight, its UID is empty, or the claim UID is
    // missing; C4 the large guarded variant uses in-memory indirection.
    // Effects: E1 C1+C2+C4 preserves both immutable identities, the predecessor
    // fence and the exact serialized wire; E2 C3 refuses before a physical-
    // incarnation claim. Rules H1=C1+C2+C4->E1, H2=C3->E2. Ordinary adoption
    // is owned by the K8s runtime purpose table, not duplicated in this test.
    use crate::{CreateTicket, CreateTicketRequest, TicketOrigin, TicketResult, TicketStatus};
    let predecessor = awaken_provisioning_contract::SandboxEffectFence::new(
        "old-effect",
        "old-worker",
        "old-boot",
        1,
        u64::MAX,
    )
    .unwrap();
    let ticket = CreateTicket {
        request: CreateTicketRequest::new(
            "old-group",
            "pods",
            "default",
            "awaken-scope",
            "a".repeat(64),
        )
        .unwrap(),
        status: TicketStatus::Finished(TicketResult::Isolated {
            uid: "guard-uid".into(),
        }),
        origin: TicketOrigin::TerminalGuard,
        isolation_id: "0123456789abcdef0123456789abcdef".into(),
        isolation_pending: false,
    };
    let guarded =
        KubernetesGuardedClaimHandle::new("scope", &predecessor, ticket.clone(), "claim-uid")
            .unwrap();
    assert_eq!(
        serde_json::to_string(&Box::new(guarded.clone())).unwrap(),
        serde_json::to_string(&guarded).unwrap(),
        "H1 boxed storage preserves the typed wire"
    );
    let handle = pc::SandboxHandle::container_v2(
        "scope",
        container_v2(
            Some(ContainerContinuationHandle::KubernetesGuardedClaim(
                Box::new(guarded.clone()),
            )),
            Vec::new(),
        ),
    );
    let reopened: pc::SandboxHandle =
        serde_json::from_str(&serde_json::to_string(&handle).unwrap()).unwrap();
    assert_eq!(reopened, handle, "H1 typed replay");
    assert_eq!(
        reopened.container_physical_incarnation().unwrap(),
        "guard-uid",
        "H1"
    );
    assert_eq!(guarded.claim_uid, "claim-uid", "H1");
    assert_eq!(guarded.predecessor, predecessor, "H1");

    let mut in_flight = ticket.clone();
    in_flight.status = TicketStatus::InFlight;
    assert!(
        KubernetesGuardedClaimHandle::new("scope", &predecessor, in_flight, "claim-uid").is_err(),
        "H2"
    );
    assert!(
        KubernetesGuardedClaimHandle::new("scope", &predecessor, ticket.clone(), "").is_err(),
        "H2"
    );
    let mut no_uid = ticket;
    no_uid.status = TicketStatus::Finished(TicketResult::Isolated { uid: String::new() });
    assert!(
        KubernetesGuardedClaimHandle::new("scope", &predecessor, no_uid.clone(), "claim-uid")
            .is_err(),
        "H2 empty guard UID at construction"
    );
    let malformed = KubernetesGuardedClaimHandle {
        scope: "scope".into(),
        predecessor,
        guard: no_uid,
        claim_uid: "claim-uid".into(),
    };
    let malformed_handle = pc::SandboxHandle::container_v2(
        "scope",
        container_v2(
            Some(ContainerContinuationHandle::KubernetesGuardedClaim(
                Box::new(malformed),
            )),
            Vec::new(),
        ),
    );
    assert!(
        malformed_handle.container_physical_incarnation().is_err(),
        "H2 empty guard UID"
    );
}

#[test]
fn closed_container_observation_requires_the_original_physical_incarnation() {
    // Causes: C1 provider-decoded current/legacy handle; C2 closed/non-closed
    // observation; C3 exact/foreign/missing physical incarnation. Effects:
    // R1 current+closed+exact -> accept; R2 current+closed+(foreign|missing)
    // -> Conflict; R3 non-closed -> Invalid; R4 no handle incarnation -> refuse.
    // The table has no provider I/O and cannot grant disposal authority.
    let handle = pc::SandboxHandle::container_v2(
        "scope",
        container_v2(
            Some(ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "pod-uid-a".into(),
                claim_uid: None,
            }),
            Vec::new(),
        ),
    );
    let legacy = pc::SandboxHandle::container(
        "scope",
        ContainerSandboxHandleV1 {
            sandbox_control_incarnation: None,
            runtime_handle: None,
            ..container_v1(None)
        },
    );
    for observation in [
        pc::SandboxObservation::Ready,
        pc::SandboxObservation::Provisioning { deadline: None },
        pc::SandboxObservation::Incompatible {
            reason: "foreign".into(),
        },
        pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: None,
        },
        pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: Some("pod-uid-a".into()),
        },
        pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: Some("foreign".into()),
        },
        pc::SandboxObservation::Terminal {
            physical_incarnation: "pod-uid-a".into(),
        },
        pc::SandboxObservation::Terminal {
            physical_incarnation: "foreign".into(),
        },
        pc::SandboxObservation::Disposing {
            physical_incarnation: "pod-uid-a".into(),
        },
        pc::SandboxObservation::Disposing {
            physical_incarnation: "foreign".into(),
        },
    ] {
        let expected = matches!(&observation,
            pc::SandboxObservation::DefinitivelyUnavailable { physical_incarnation: Some(uid) }
            | pc::SandboxObservation::Terminal { physical_incarnation: uid }
            | pc::SandboxObservation::Disposing { physical_incarnation: uid } if uid == "pod-uid-a");
        assert_eq!(
            validate_container_closed_observation(&handle, &observation).is_ok(),
            expected,
            "R1-R3 {observation:?}"
        );
        assert!(
            validate_container_closed_observation(&legacy, &observation).is_err(),
            "R4 {observation:?}"
        );
    }
}
