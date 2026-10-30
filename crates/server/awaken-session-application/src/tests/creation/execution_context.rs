use super::*;
use awaken_session_configuration_contract::{SessionContextSeed, SessionTrigger};

#[tokio::test]
async fn profiled_context_admission_freezes_once_and_rejects_invalid_authoring() {
    // Causes: C1 absent/valid/invalid timezone; C2 reserved prompt; C3 public
    // metadata claims Deployment; C4 exact create replay. Effects: E1 one
    // trusted interactive seed, UTC/explicit zone; E2 reject before root; E3
    // replay retains the exact baseline and does not append another seed.
    // Rules C1 valid/absent+C3=>E1; C1 invalid or C2=>E2; C4=>E3.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let mut app = application(repository.clone(), Arc::new(AdmissionEnvironment));
    app.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    for (id, zone) in [
        ("context-default", None),
        ("context-explicit", Some("Asia/Singapore")),
    ] {
        let command = || {
            let mut command = profiled_session_command(id, None);
            command
                .metadata
                .insert("awaken.deployment_id".into(), "untrusted".into());
            if let Some(zone) = zone {
                command
                    .metadata
                    .insert("awaken.timezone".into(), zone.into());
            }
            command.idempotency = Some(awaken_session_contract::IdempotencyRecord {
                key: format!("create:{id}"),
                payload_hash: format!("authoring:{id}"),
            });
            command
        };
        let created = app
            .creation_commands
            .accept_profiled_session(command())
            .await
            .unwrap();
        let baseline = created.frozen_baseline().unwrap().clone();
        let seed = SessionContextSeed::from_prompts(&baseline.prompts).unwrap();
        assert_eq!(seed.timezone(), zone.unwrap_or("UTC"), "E1");
        assert!(
            seed.encode().contains("\"source\":\"interactive\""),
            "C3 ignored"
        );
        assert_eq!(
            baseline
                .prompts
                .iter()
                .filter(|prompt| SessionContextSeed::is_seed_prompt(prompt))
                .count(),
            1
        );
        let replay = app
            .creation_commands
            .accept_profiled_session(command())
            .await
            .unwrap();
        assert_eq!(
            replay.frozen_baseline(),
            Some(&baseline),
            "E3 exact frozen replay"
        );
        assert_eq!(
            repository
                .get(&SessionId::from(id))
                .await
                .unwrap()
                .frozen_baseline(),
            Some(&baseline),
            "durable readback"
        );
    }
    for (id, reserved) in [("invalid-zone", false), ("reserved-source", true)] {
        let mut command = profiled_session_command(id, None);
        if reserved {
            command.prompts.push(
                SessionContextSeed::from_metadata(
                    SessionTrigger::Interactive {},
                    &Default::default(),
                    None,
                )
                .unwrap()
                .encode(),
            );
        } else {
            command
                .metadata
                .insert("awaken.timezone".into(), "invalid/zone".into());
        }
        assert!(
            matches!(app.creation_commands.accept_profiled_session(command).await, Err(SessionCreationError::Rejected(error))
            if error.kind == awaken_session_types::RunErrorKind::BadRequest),
            "E2"
        );
        assert!(
            matches!(
                repository.get(&SessionId::from(id)).await,
                Err(awaken_session_contract::SessionRepositoryError::NotFound)
            ),
            "E2 no root"
        );
    }
}
