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

/*
 * Phase-A writer decision table. Causes: C1 unmanaged, C2 legacy and C3 current
 * provider payloads through the one neutral constructor every provider codec
 * uses (each codec pins its own typed writer bytes). Effect E1: every current
 * constructor emits legacy None (the field is absent), so only a later gated
 * composition can write Some. Rules R1=C1=>E1, R2=C2=>E1, R3=C3=>E1.
 */
fn fixture_locator() -> crate::test_support::FixtureHandleLocator {
    crate::test_support::FixtureHandleLocator {
        outputs_path: "/outputs".into(),
        physical_incarnation: "physical-a".into(),
    }
}

#[test]
fn every_phase_a_constructor_omits_restoration() {
    let handles = [
        SandboxHandle::new("fake", "sandbox-a"),
        crate::test_support::fixture_legacy_handle("sandbox-a", &fixture_locator()),
        crate::test_support::fixture_current_handle(
            "sandbox-a",
            &fixture_locator(),
            &serde_json::from_value(serde_json::json!("fingerprint-a")).unwrap(),
            &SandboxEffectFence::new("effect-a", "owner-a", "runtime-a", 1, u64::MAX).unwrap(),
            &["/workspace/a".to_owned()],
        ),
    ];
    for (rule, handle) in ["R1", "R2", "R3"].into_iter().zip(handles) {
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
