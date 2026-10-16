// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Codec decision rows reject contradictory empty-set carriers; no physical outcome is claimed.
//! Empty projection provenance remains bound to its one original handle.
use super::*;
use crate::ContainerHandleCodec as _;

#[test]
fn frozen_plan_empty_codec_rejects_members_and_conflicting_envelope_fingerprints() {
    // Causes: C1 canonical namespace/Pod UID; C2 zero original API objects;
    // C3 complete origin fingerprints; C4 outer envelope fingerprints agree.
    // Effects: E1 exact terminal-only round trip; E2 reject malformed evidence.
    // R1=C1+C2+C3+C4 => E1. R2=!C2 or !C3 => E2 during set decode.
    // R3=!C4 => E2 in the sole Container handle codec, before provider I/O.
    let adoption: pc::SandboxRealizationFingerprint =
        serde_json::from_value(serde_json::json!("original-adoption")).unwrap();
    let realization: pc::SandboxRealizationFingerprint =
        serde_json::from_value(serde_json::json!("original-realization")).unwrap();
    let original = KubernetesProjectedParticipants::frozen_plan_empty(
        "default".into(),
        "original-pod".into(),
        adoption.clone(),
        realization.clone(),
    )
    .unwrap();
    let wire = serde_json::to_value(&original).unwrap();
    let decoded: KubernetesProjectedParticipants = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded, original, "R1 exact wire");
    assert!(decoded.terminal_only(), "R1 grants no execution");
    for row in ["member", "missing-adoption", "missing-realization"] {
        let mut changed = wire.clone();
        match row {
            "member" => {
                changed["objects"] = serde_json::json!([{
                    "kind":"config_map", "name":"original-member", "uid":"original-member-uid",
                    "owner_references_fingerprint":"original-owner"
                }])
            }
            "missing-adoption" => {
                changed["origin"]
                    .as_object_mut()
                    .unwrap()
                    .remove("adoption");
            }
            "missing-realization" => {
                changed["origin"]
                    .as_object_mut()
                    .unwrap()
                    .remove("realization");
            }
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<KubernetesProjectedParticipants>(changed).is_err(),
            "R2 {row}"
        );
    }
    for row in ["exact", "adoption", "realization"] {
        let changed: pc::SandboxRealizationFingerprint =
            serde_json::from_value(serde_json::json!("foreign-fingerprint")).unwrap();
        let handle = pc::SandboxHandle::container_v2(
            "original-session",
            crate::ContainerSandboxHandleV2 {
                previous: crate::ContainerSandboxHandleV1 {
                    container_id: "original-pod-name".into(),
                    outputs_path: "/outputs".into(),
                    base_env: vec![],
                    live_input_projection: true,
                    continuation_excluded_paths: vec![],
                    runtime_handle: Some(
                        crate::ContainerContinuationHandle::KubernetesContinuationV3 {
                            pod_uid: "original-pod".into(),
                            claim_uid: None,
                            participants: Box::new(original.clone()),
                        },
                    ),
                    sandbox_control_incarnation: None,
                    control_services: Default::default(),
                },
                adoption_fingerprint: if row == "adoption" {
                    changed.clone()
                } else {
                    adoption.clone()
                },
                realization_fingerprint: if row == "realization" {
                    changed
                } else {
                    realization.clone()
                },
                owned_paths: vec![],
            },
        );
        assert_eq!(
            handle.container_payload().is_ok(),
            row == "exact",
            "R1/R3 {row}"
        );
    }
}
