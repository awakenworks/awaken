use super::*;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_contract::{SessionEnvironmentEffectIntent, SessionEnvironmentEffectKind};

fn lease(owner: &str, incarnation: &str) -> SessionRealizationLease {
    SessionRealizationLease {
        owner: owner.into(),
        runtime_incarnation: incarnation.into(),
        epoch: 3,
        expires_at_unix_ms: 1_900_000_000_000,
    }
}

fn intent(
    kind: SessionEnvironmentEffectKind,
    lease: Option<SessionRealizationLease>,
) -> SessionEnvironmentEffectIntent {
    SessionEnvironmentEffectIntent::new("session-1", kind, lease)
}

fn materialization(
    origin: Option<&SessionEnvironmentEffectIntent>,
    pending: Option<&SessionEnvironmentEffectIntent>,
) -> SessionEnvironmentMaterialization {
    let mut facts = serde_json::Map::new();
    if let Some(origin) = origin {
        facts.insert("origin".into(), serde_json::to_value(origin).unwrap());
    }
    if let Some(pending) = pending {
        facts.insert("pending".into(), serde_json::to_value(pending).unwrap());
    }
    serde_json::from_value(serde_json::Value::Object(facts)).unwrap()
}

fn resident(effect_id: Option<&str>) -> SessionEnvironmentState {
    SessionEnvironmentState::Resident {
        binding: "binding".into(),
        effect_id: effect_id.map(str::to_owned),
        generation: None,
        idle_since_unix_ms: None,
    }
}

fn handle(fence: Option<(&str, &str)>) -> awaken_provisioning_contract::SandboxHandle {
    let mut payload = serde_json::json!({
        "schema": "local_v2",
        "previous": {"outputs_path": "/mnt/session/outputs"},
    });
    if let Some((owner, incarnation)) = fence {
        payload["realization_fingerprint"] = "fp-realization".into();
        payload["owned_paths"] = serde_json::json!([]);
        payload["effect_fence"] = serde_json::json!({
            "operation_id": "op-1", "owner": owner, "runtime_incarnation": incarnation,
            "epoch": 3, "expires_at_unix_ms": 1_900_000_000_000_u64,
        });
    }
    serde_json::from_value(serde_json::json!({"sandbox_id": "session-1", "payload": payload}))
        .unwrap()
}

#[test]
fn coordinator_projects_only_the_recorded_end_of_the_materializing_incarnation() {
    let create = intent(
        SessionEnvironmentEffectKind::Create,
        Some(lease("worker-a", "worker-a:1:boot-a")),
    );
    let rebuild = intent(
        SessionEnvironmentEffectKind::Rebuild {
            source_generation_id: None,
        },
        Some(lease("worker-b", "worker-b:1:boot-b")),
    );
    let unleased = intent(SessionEnvironmentEffectKind::Create, None);
    let adopt = intent(
        SessionEnvironmentEffectKind::Adopt,
        Some(lease("worker-c", "worker-c:1:boot-c")),
    );
    let facts = materialization(Some(&create), Some(&rebuild));
    let a = Some(("worker-a".to_string(), "worker-a:1:boot-a".to_string()));
    let b = Some(("worker-b".to_string(), "worker-b:1:boot-b".to_string()));
    for (rule, environment, materialization, expected) in [
        (
            "BE1 unmaterialized",
            SessionEnvironmentState::Unmaterialized,
            Some(&facts),
            None,
        ),
        ("BE1 no effect id", resident(None), Some(&facts), None),
        (
            "BE2 unknown materialization",
            resident(Some(create.effect_id())),
            None,
            None,
        ),
        (
            "BE2 adopted binding",
            resident(Some(adopt.effect_id())),
            Some(&facts),
            None,
        ),
        (
            "BE2 a recorded Adopt is not a materialization",
            resident(Some(adopt.effect_id())),
            Some(&materialization(Some(&adopt), None)),
            None,
        ),
        (
            "BE3 no lease",
            resident(Some(unleased.effect_id())),
            Some(&materialization(Some(&unleased), None)),
            None,
        ),
        (
            "BE4 origin Create",
            resident(Some(create.effect_id())),
            Some(&facts),
            a.clone(),
        ),
        (
            "BE4 pending Rebuild",
            resident(Some(rebuild.effect_id())),
            Some(&facts),
            b,
        ),
    ] {
        assert_eq!(
            BoundSourceEnd::bound_incarnation(&environment, materialization),
            expected,
            "{rule}"
        );
    }

    let exact = WorkerProcessExit::new("kubernetes", "binding", "exit").unwrap();
    let superseded = WorkerProcessExit::ended(
        "kubernetes",
        "binding",
        "superseded",
        WorkerProcessEnd::Superseded,
    )
    .unwrap();
    for (rule, receipt, expected) in [
        ("BE4 unrecorded", None, None),
        ("BE5", Some(&exact), Some(RecordedProcessEnd::ExactExit)),
        (
            "BE6",
            Some(&superseded),
            Some(RecordedProcessEnd::Superseded),
        ),
    ] {
        let projected =
            BoundSourceEnd::from_registry("worker-a".into(), "worker-a:1:boot-a".into(), receipt);
        assert_eq!(projected.as_ref().map(|end| end.end), expected, "{rule}");
        if let Some(projected) = projected {
            assert_eq!(
                (
                    projected.owner.as_str(),
                    projected.runtime_incarnation.as_str()
                ),
                ("worker-a", "worker-a:1:boot-a"),
                "{rule} names the incarnation it read"
            );
        }
    }
}

#[test]
fn worker_reads_the_end_only_for_the_handle_fenced_by_that_incarnation() {
    let end = BoundSourceEnd {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:1:boot-a".into(),
        end: RecordedProcessEnd::Superseded,
    };
    for (rule, fence, expected) in [
        ("BW1 legacy handle", None, None),
        (
            "BW2 another owner",
            Some(("worker-b", "worker-a:1:boot-a")),
            None,
        ),
        (
            "BW2 another incarnation",
            Some(("worker-a", "worker-a:2:boot-b")),
            None,
        ),
        (
            "BW3 the projected incarnation",
            Some(("worker-a", "worker-a:1:boot-a")),
            Some(RecordedProcessEnd::Superseded),
        ),
    ] {
        assert_eq!(end.end_for(&handle(fence)), expected, "{rule}");
    }
}

#[test]
fn a_projection_without_the_fact_decodes_and_the_fact_round_trips() {
    // Wire rows: a directive from a Coordinator that predates the fact omits
    // it and decodes as unrecorded; the fact itself refuses unknown members.
    let end = BoundSourceEnd {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:1:boot-a".into(),
        end: RecordedProcessEnd::ExactExit,
    };
    let encoded = serde_json::to_value(&end).unwrap();
    assert_eq!(encoded["end"], "exact_exit");
    assert_eq!(
        serde_json::from_value::<BoundSourceEnd>(encoded.clone()).unwrap(),
        end
    );
    let mut extra = encoded;
    extra["exit_code"] = 0.into();
    assert!(serde_json::from_value::<BoundSourceEnd>(extra).is_err());
    let projection = crate::session_realization::tests::legacy_resource_projection(true);
    let mut wire = serde_json::to_value(&projection).unwrap();
    assert!(
        wire.get("bound_source_end").is_none(),
        "absent fact is not serialized"
    );
    wire.as_object_mut().unwrap().remove("bound_source_end");
    let decoded: crate::FrozenSessionProjection = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.bound_source_end, None);
}
