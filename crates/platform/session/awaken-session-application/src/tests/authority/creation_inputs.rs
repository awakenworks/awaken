//! The canonical creation finalizer uses the original resolver, pin and abort.
use super::*;
use crate::{SessionCreateRequestError, SessionPreparationError};
use awaken_resource_contract::{BindingId, InputBinding, InputResourceId, ResourceAccess};

fn attachment(target: InputResourceId) -> crate::session_resource::SessionInputAttachment {
    crate::session_resource::SessionInputAttachment {
        binding: InputBinding {
            binding_id: BindingId::from("finalized-input"),
            target,
            mount_path: "/workspace/source".into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        },
        replaces: None,
    }
}

fn holder() -> awaken_agent_contract::credential::PlaintextHolder {
    awaken_credential_contract::CredentialRealizationProfile::self_hosted_native().resource_holder
}

fn skill(version: u64) -> crate::session_resource::ResolvedSkillBinding {
    crate::session_resource::ResolvedSkillBinding {
        kind: Default::default(),
        skill_id: "finalized-skill".into(),
        version,
        bundle_sha256: "original-immutable-digest".into(),
    }
}

#[tokio::test]
async fn creation_finalization_preserves_resolution_skill_and_pin_failure_stages() {
    // O2/O4 + F1/F3/F4: a real configured, credential-bound Repository
    // exists in the original Registry, but there is deliberately no credential
    // source. R1 missing Memory resolution + malformed Skill + missing source
    // -> original resolver Rejected/input-resolution, never Skill/pin.
    // R2 existing Repository + malformed Skill + missing source -> original
    // BadRequest/Skill-binding, never pin. R3 valid Skill + missing source ->
    // original Preparation(Rejected)/credential-pinning. All failures precede
    // root insertion and preserve the exact first error through original abort.
    let sessions = authority_repository("creation finalizer failure store").await;
    let registry = crate::tests::creation::ephemeral_resource_registry().await;
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let mut app = application(
        sessions.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.set_resource_registry(registry.clone());
    app.set_credential_material_ingress(ingress.clone());
    for (rule, missing, version, expected_stage) in [
        ("F1", true, 0, "input resolution"),
        ("F3", false, 0, "Skill binding"),
        ("F4", false, 1, "credential pinning"),
    ] {
        let id = format!("finalizer-{rule}");
        let configured = app
            .repository_participants
            .configure(token_repository_input(&id, rule))
            .await
            .unwrap();
        let target = if missing {
            InputResourceId::MemoryStore("missing-finalizer-memory".into())
        } else {
            InputResourceId::Repository(configured.repository_id.clone())
        };
        let (first, stage) = app
            .creation_commands
            .finalize_creation_inputs(
                "workspace",
                &[],
                &[attachment(target)],
                Some(vec![skill(version)]),
                &holder(),
                SessionCreateRequestError::from,
            )
            .await
            .unwrap_err();
        assert_eq!(stage, expected_stage, "{rule} original diagnostic");
        match &first {
            SessionCreateRequestError::Preparation(SessionPreparationError::Rejected(error)) => {
                assert_eq!(rule, "F4");
                assert_eq!(
                    error.message,
                    "Repository credential requires a configured credential vault"
                );
            }
            SessionCreateRequestError::Rejected(error) => {
                assert_ne!(rule, "F4");
                assert_eq!(
                    error.kind,
                    awaken_runtime_contract::runs::RunErrorKind::BadRequest
                );
                assert!(if missing {
                    error.message.contains("missing-finalizer-memory")
                } else {
                    error.message.contains("version zero")
                });
            }
            other => panic!("{rule} changed original error variant: {other:?}"),
        }
        let original = first.to_string();
        app.repository_participants
            .compensate(
                &id,
                std::slice::from_ref(&configured),
                &format!("Managed Session Repository compensation remains pending after {stage}"),
            )
            .await;
        assert_eq!(
            first.to_string(),
            original,
            "{rule} first error survives abort"
        );
        assert_eq!(
            registry
                .find_repository("workspace", configured.repository_id.as_str())
                .await
                .unwrap()
                .unwrap()
                .state,
            awaken_resource_contract::ResourceState::Deleted,
            "{rule} original Applied compensation"
        );
        assert!(
            matches!(
                sessions.get(&SessionId::from(&id)).await,
                Err(awaken_session_contract::SessionRepositoryError::NotFound)
            ),
            "{rule} no root"
        );
    }
    assert_eq!(
        ingress.retirements().len(),
        3,
        "F1/F3/F4 original credentials retired"
    );
}

#[tokio::test]
async fn creation_finalization_retains_exact_successful_bindings_and_skill_selection() {
    // F2/F5: no Skills, explicit empty Skills, and a valid immutable Skill pin
    // all pass through the same real resolver and pin-all compiler. File inputs
    // need no Registry/Vault; their access is canonical ReadOnly. No participant
    // or root is created by finalization, and the caller's pin error projector
    // cannot run on any successful row. Some(empty) remains a supplied selection.
    let sessions = authority_repository("creation finalizer success store").await;
    let app = application(
        sessions.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    for (rule, skills) in [
        ("F2-none", None),
        ("F2-empty", Some(Vec::new())),
        ("F5", Some(vec![skill(7)])),
    ] {
        let inputs = [attachment(InputResourceId::File("original-file".into()))];
        let selected = skills.clone().unwrap_or_default();
        let result = app
            .creation_commands
            .finalize_creation_inputs::<SessionCreateRequestError>(
                "workspace",
                &[],
                &inputs,
                skills,
                &holder(),
                |_| panic!("{rule} successful pin projected a failure"),
            )
            .await
            .unwrap();
        assert_eq!(
            result.skills(),
            selected,
            "{rule} immutable Skill selection"
        );
        let [input] = result.inputs() else {
            panic!("{rule} original input missing")
        };
        assert_eq!(
            input.binding_id, inputs[0].binding.binding_id,
            "{rule} binding identity"
        );
        assert_eq!(
            input.mount_path, inputs[0].binding.mount_path,
            "{rule} path"
        );
        assert_eq!(
            input.access,
            ResourceAccess::ReadOnly,
            "{rule} canonical File access"
        );
        assert_eq!(input.created_at_unix_ms, None);
    }
    assert!(
        matches!(
            sessions.get(&SessionId::from("finalizer-success")).await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "F5 no invented root"
    );
}

#[tokio::test]
async fn creation_finalization_failure_cannot_dispose_replayed_adopted_or_unreadable_participants()
{
    // O9 + F6: one real finalization failure -> original first error.
    // Compensation considers Applied/Replayed provenance and original durable
    // adoption before disposal: Applied+absent retires; Replayed, adopted, or
    // unreadable root preserves. A credential-retirement failure retains its
    // participant and never replaces that first error. No new cleanup receipt
    // or root is manufactured. Registry/Vault are the existing fixture owners.
    for rule in [
        "applied",
        "replayed",
        "adopted",
        "unreadable",
        "retire-failed",
    ] {
        let id = format!("finalizer-compensation-{rule}");
        let durable = authority_repository("finalizer compensation durable store").await;
        let sessions = Arc::new(FaultingSessionRepository::new(durable.clone()));
        let registry = crate::tests::creation::ephemeral_resource_registry().await;
        let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
        let mut app = application(
            sessions.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        app.set_resource_registry(registry.clone());
        app.set_credential_material_ingress(ingress.clone());
        let configured = app
            .repository_participants
            .configure(token_repository_input(&id, rule))
            .await
            .unwrap();
        let configured = if rule == "replayed" {
            app.repository_participants
                .configure(token_repository_input(&id, rule))
                .await
                .unwrap()
        } else {
            configured
        };
        if rule == "adopted" {
            let mut root = persisted(&id, false, "idle");
            root.resources = awaken_session_contract::SessionResourceState::from_active(
                repository_retirement_resources(
                    &id,
                    configured
                        .credential
                        .as_ref()
                        .map(|entry| entry.credential.clone()),
                ),
            );
            create(durable.as_ref(), root).await;
        }
        if rule == "unreadable" {
            sessions.failed_gets.lock().unwrap().insert(id.clone());
        }
        if rule == "retire-failed" {
            ingress.fail_next_retirement();
        }
        let (first, stage) = app
            .creation_commands
            .finalize_creation_inputs::<SessionCreateRequestError>(
                "workspace",
                &[],
                &[attachment(InputResourceId::MemoryStore(
                    "missing-finalizer-memory".into(),
                ))],
                None,
                &holder(),
                SessionCreateRequestError::from,
            )
            .await
            .unwrap_err();
        let original = first.to_string();
        app.repository_participants
            .compensate(
                &id,
                std::slice::from_ref(&configured),
                &format!("Managed Session Repository compensation remains pending after {stage}"),
            )
            .await;
        assert_eq!(first.to_string(), original, "F6/{rule} original failure");
        let expected = if matches!(rule, "applied" | "retire-failed") {
            awaken_resource_contract::ResourceState::Deleted
        } else {
            awaken_resource_contract::ResourceState::Active
        };
        assert_eq!(
            registry
                .find_repository("workspace", configured.repository_id.as_str())
                .await
                .unwrap()
                .unwrap()
                .state,
            expected,
            "F6/{rule} exact original Registry disposition"
        );
        assert_eq!(
            ingress.retirements().len(),
            usize::from(rule == "applied"),
            "F6/{rule} credential disposition"
        );
        assert_eq!(
            durable.get(&SessionId::from(&id)).await.is_ok(),
            rule == "adopted",
            "F6/{rule} no fabricated root"
        );
    }
}
