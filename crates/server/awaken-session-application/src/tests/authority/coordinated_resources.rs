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
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
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

#[tokio::test]
async fn cold_child_report_projects_frozen_root_and_files_before_continuation() {
    // Causes: C1 a committed natural child report; C2 no resident Coordinator
    // projection after restart; C3 aggregate owns root revision 1 and prepared
    // File generation 2; C4 projection succeeds/fails. Effects: E1 complete
    // pinned root/model/File facts precede continuation; E2 no physical effects;
    // E3 unavailable preparation retains the activity for retry, no admission.
    // Rules CR1=C1+C2+C3+C4=>E1+E2; CR2=C1+C2+C3+!C4=>E2+E3.
    // Retry CR2 with C4 restored=>CR1, same activity and committed report.
    for fail_projection in [false, true] {
        let id = format!("cold-report-files-{fail_projection}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
        runtime
            .projection_failure
            .store(fail_projection, Ordering::SeqCst);
        let mut session = coordinated_session(&id);
        session
            .resources
            .prepare(&session.session_id, file_resources("original"))
            .unwrap();
        session.resources.commit().unwrap();
        session
            .resources
            .prepare(&session.session_id, file_resources("confirmed"))
            .unwrap();
        create(repo.as_ref(), session).await;
        let app = configured_admission_application(repo.clone(), runtime.clone());
        let (_, epoch) = app
            .begin_activity_for_operation(&SessionId::from(&id), "cold-child-report")
            .await
            .unwrap();
        let child = awaken_agent_contract::agent::thread::Id("cold-child".into());
        let run = awaken_agent_contract::agent::run::Id("cold-child-run".into());
        runtime.commit_boundary(
            &id,
            &child,
            &run,
            awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            ),
            "report files ready",
        );
        let command = awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: id.as_str().into(),
            source_thread_id: child,
            source_run_id: run,
            source_agent_id: "coord-child".into(),
            session_activity_epoch: epoch,
            cancellation_requested: false,
        };
        let result = awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app, command.clone()).await;
        if fail_projection {
            assert_eq!(
                result.unwrap_err().kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "CR2 retryable"
            );
            assert!(runtime.continuations.lock().unwrap().is_empty(), "CR2/E3");
            runtime.projection_failure.store(false, Ordering::SeqCst);
            awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
                &app, command).await.expect("CR2 recovery");
        } else {
            result.expect("CR1");
        }
        // Finish each synchronous observation before the repository await.
        // Retaining either mutex there can block the recovery it observes.
        {
            let projections = runtime.installed_projections.lock().unwrap();
            let projection = projections.last().expect("CR1 complete frozen projection");
            assert_eq!(projection.baseline.agent_id, "coord-root");
            assert_eq!(projection.baseline.agent_revision, Some(1));
            assert_eq!(projection.baseline.model, "coord-model");
            let publication = projection
                .agent_publication
                .as_ref()
                .expect("exact root publication");
            assert_eq!(publication.metadata.source.agent_id.0, "coord-root");
            assert_eq!(publication.metadata.source.revision, 1);
            assert_eq!(projection.resource_revision, 2);
            assert_eq!(projection.resources, file_resources("confirmed"));
        }
        {
            let continuations = runtime.continuations.lock().unwrap();
            assert_eq!(
                continuations.len(),
                1,
                "one continuation after successful preparation"
            );
            assert_eq!(
                continuations[0].session_activity_epoch, epoch,
                "same activity on recovery"
            );
        }
        let persisted = repo.get(&SessionId::from(&id)).await.unwrap();
        assert!(
            persisted.active_activity_epochs.contains(&epoch),
            "not falsely idle before root result"
        );
        assert_eq!(persisted.resources.active_revision(), 1, "E2");
        assert!(
            persisted
                .resources
                .activations
                .iter()
                .all(|a| a.attempts == 0),
            "E2"
        );
    }
}
