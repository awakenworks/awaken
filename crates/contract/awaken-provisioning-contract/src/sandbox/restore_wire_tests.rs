use super::*;

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
    base_env: Vec<crate::EnvVar>,
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
    control_services: std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
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
    let decoded: SandboxHandle = serde_json::from_value(legacy).expect("I1/E1");
    let SandboxHandlePayload::ContainerV1(payload) = &decoded.payload else {
        panic!("I1 container payload")
    };
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
 * Phase-A writer decision table. Causes: C1 unmanaged, C2 local, C3 namespace,
 * C4 container typed constructors. Effect E1: every current constructor emits
 * legacy None (the field is absent), so only a later gated composition can write
 * Some. Rules R1=C1=>E1, R2=C2=>E1, R3=C3=>E1, R4=C4=>E1.
 */
#[test]
fn every_phase_a_constructor_omits_restoration() {
    let handles = [
        SandboxHandle::new("fake", "sandbox-a"),
        SandboxHandle::local(
            "sandbox-a",
            LocalSandboxHandleV1 {
                outputs_path: "/outputs".into(),
                base_env: Vec::new(),
                continuation_excluded_paths: Vec::new(),
                deny_tool_egress: false,
            },
        ),
        SandboxHandle::namespace(
            NamespaceProviderKind::Bubblewrap,
            "sandbox-a",
            NamespaceSandboxHandleV1 {
                outputs_path: "/outputs".into(),
                base_env: Vec::new(),
                network: crate::NetworkPolicy::None,
                control_services: Default::default(),
            },
        ),
        SandboxHandle::container(
            "sandbox-a",
            ContainerSandboxHandleV1 {
                container_id: "container-a".into(),
                outputs_path: "/outputs".into(),
                base_env: Vec::new(),
                live_input_projection: false,
                continuation_excluded_paths: Vec::new(),
                runtime_handle: None,
                sandbox_control_incarnation: None,
                control_services: Default::default(),
            },
        ),
    ];
    for (rule, handle) in ["R1", "R2", "R3", "R4"].into_iter().zip(handles) {
        assert!(handle.restoration().is_none(), "{rule}/E1");
        assert!(
            serde_json::to_value(handle)
                .unwrap()
                .get("restoration")
                .is_none(),
            "{rule}/E1"
        );
    }
    assert_eq!(
        serde_json::to_string(&SandboxHandle::new("fake", "sandbox-a")).unwrap(),
        r#"{"sandbox_id":"sandbox-a","payload":{"schema":"unmanaged","provider_kind":"fake"}}"#,
        "R1/E1 exact legacy bytes"
    );
}

/*
 * Evidence grammar decision table. C1 complete six-field Some; C2 one field
 * missing; C3 one unknown field; C4 old deny-unknown reader. Effects: E1 new
 * reader is lossless/read-only, E2 incomplete or widened evidence is rejected,
 * E3 old readers fail forward. R5=C1=>E1, R6=(C2|C3)=>E2, R7=C1+C4=>E3.
 */
#[test]
fn future_evidence_is_lossless_closed_and_fail_forward() {
    let mut future = serde_json::to_value(SandboxHandle::new("fake", "sandbox-a")).unwrap();
    future
        .as_object_mut()
        .unwrap()
        .insert("restoration".into(), future_evidence());
    let decoded: SandboxHandle = serde_json::from_value(future.clone()).expect("R5/E1 decode");
    let evidence = decoded.restoration().expect("R5/E1 evidence");
    assert_eq!(evidence.effect_id(), "effect-a", "R5/E1");
    assert_eq!(evidence.generation_id(), "generation-a", "R5/E1");
    assert_eq!(evidence.checkpoint_id(), "checkpoint-a", "R5/E1");
    assert_eq!(evidence.checkpoint_digest(), "sha256:digest-a", "R5/E1");
    assert_eq!(evidence.sandbox_spec_fingerprint(), "spec-a", "R5/E1");
    assert_eq!(
        evidence.checkpoint_exclusions_fingerprint(),
        "exclusions-a",
        "R5/E1"
    );
    assert_eq!(serde_json::to_value(&decoded).unwrap(), future, "R5/E1");
    assert!(
        serde_json::from_value::<LegacySandboxHandle>(future.clone()).is_err(),
        "R7/E3"
    );

    let mut partial = future.clone();
    partial["restoration"]
        .as_object_mut()
        .unwrap()
        .remove("checkpoint_digest");
    assert!(
        serde_json::from_value::<SandboxHandle>(partial).is_err(),
        "R6/E2 missing"
    );

    let mut unknown = future;
    unknown["restoration"]
        .as_object_mut()
        .unwrap()
        .insert("ambient_target".into(), serde_json::json!("forbidden"));
    assert!(
        serde_json::from_value::<SandboxHandle>(unknown).is_err(),
        "R6/E2 unknown"
    );
}

/*
 * Nested rollout decision table. C1 legacy Kubernetes locator; C2 future
 * HostBind locator plus Some evidence; C3 the actual old nested enum reader.
 * Effects: E1 C1 stays readable, E2 new reader losslessly accepts C2, E3 C3
 * rejects C2 rather than silently dropping the physical locator. Rules:
 * R8=C1=>E1, R9=C2=>E2, R10=C2+C3=>E3.
 */
#[test]
fn future_host_bind_is_reader_only_and_legacy_nested_enum_rejects_it() {
    let future = future_container(serde_json::json!({
        "kind": "host_bind_restoration",
        "staging_root": "/provider/staging/a"
    }));
    let decoded: SandboxHandle = serde_json::from_value(future.clone()).expect("R9/E2 decode");
    assert!(decoded.restoration().is_some(), "R9/E2 evidence");
    let runtime_handle = decoded
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
        .expect("R9/E2 runtime locator");
    match runtime_handle {
        ContainerContinuationHandle::HostBindRestoration(locator) => {
            assert_eq!(locator.staging_root(), "/provider/staging/a", "R9/E2")
        }
        ContainerContinuationHandle::KubernetesContinuation { .. } => panic!("R9/E2 wrong kind"),
    }
    assert_eq!(serde_json::to_value(decoded).unwrap(), future, "R9/E2");

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

    let mut legacy_host_bind = future;
    legacy_host_bind
        .as_object_mut()
        .unwrap()
        .remove("restoration");
    assert!(
        serde_json::from_value::<LegacySandboxHandle>(legacy_host_bind).is_err(),
        "R10/E3"
    );
}
