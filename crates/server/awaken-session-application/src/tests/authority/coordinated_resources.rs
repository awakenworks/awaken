//! New Agent-thread dispatch shares the existing aggregate-owned Resource preparation.
use super::*;

#[tokio::test]
async fn coordination_projects_the_current_desired_files_before_child_admission() {
    // Causes: C1 exact executing source/roster; C2 active File generation 1;
    // C3 File addition is prepared as pending generation 2; C4 projection
    // succeeds/fails; C5 source validation succeeds/fails.
    // Effects: E1 child admission observes desired generation 2; E2 physical
    // active stays 1 and pending remains prepared (no application-side I/O);
    // E3 failed projection/source creates no activity or child admission.
    // | Rule | C3 | C4 | C5 | Result |
    // | F1 | no  | yes | yes | same active generation 1 projected |
    // | F2 | yes | yes | yes | generation 2 projected before child, E2 |
    // | F3 | yes | no  | yes | failure before child/activity |
    // | F4 | yes | yes | no  | failure before projection/child/activity |
    // The existing complete projection is the only staging port; this test
    // does not copy a resource selector or fabricate a physical publication.
    for (rule, pending, fail_projection, valid_source) in [
        ("F1", false, false, true),
        ("F2", true, false, true),
        ("F3", true, true, true),
        ("F4", true, false, false),
    ] {
        let id = format!("coordinate-files-{rule}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
            AgentAdmissionOutcome::Accepted,
        ]));
        runtime
            .projection_failure
            .store(fail_projection, Ordering::SeqCst);
        if !valid_source {
            runtime.reject_agent_message_source();
        }
        let mut session = coordinated_session(&id);
        session
            .resources
            .prepare(&session.session_id, file_resources("original"))
            .unwrap();
        session.resources.commit().unwrap();
        if pending {
            session
                .resources
                .prepare(&session.session_id, file_resources("confirmed"))
                .unwrap();
        }
        create(repo.as_ref(), session).await;
        let app = configured_admission_application(repo.clone(), runtime.clone());
        let result =
            awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
                &app,
                coordination_spawn_command(&id),
            )
            .await;
        if fail_projection || !valid_source {
            assert!(result.is_err(), "{rule}");
            assert!(runtime.admissions.lock().unwrap().is_empty(), "{rule}/E3");
            assert!(
                repo.get(&SessionId::from(&id))
                    .await
                    .unwrap()
                    .active_activity_epochs
                    .is_empty(),
                "{rule}/E3"
            );
            if !valid_source {
                assert!(
                    runtime.installed_projections.lock().unwrap().is_empty(),
                    "F4 no projection"
                );
            }
        } else {
            result.unwrap();
            let projections = runtime.installed_projections.lock().unwrap();
            assert_eq!(projections.len(), 1, "{rule} one canonical preparation");
            assert_eq!(
                projections[0].resource_revision,
                if pending { 2 } else { 1 },
                "{rule}/E1"
            );
            assert_eq!(
                projections[0].resources,
                if pending {
                    file_resources("confirmed")
                } else {
                    file_resources("original")
                },
                "{rule}/E1"
            );
            assert_eq!(
                runtime.admissions.lock().unwrap().len(),
                1,
                "{rule} admission follows projection"
            );
        }
        let persisted = repo.get(&SessionId::from(&id)).await.unwrap();
        assert_eq!(persisted.resources.active_revision(), 1, "{rule}/E2");
        assert_eq!(persisted.resources.pending.is_some(), pending, "{rule}/E2");
        assert!(
            persisted
                .resources
                .activations
                .iter()
                .all(|activation| activation.attempts == 0),
            "{rule} no physical attempt"
        );
    }
}
