//! New Agent-thread dispatch shares the existing aggregate-owned Resource preparation.
use super::*;

#[tokio::test]
async fn coordination_projects_the_current_desired_files_before_child_admission() {
    // Causes: C1 exact executing source/roster; C2 active File generation 1;
    // C3 File addition is prepared as pending generation 2; C4 projection
    // succeeds/fails; C5 source validation succeeds/fails.
    // C6 historical activity membership is complete/omitted. Effects: E1 child admission observes desired generation 2; E2 physical
    // active stays 1 and pending remains prepared (no application-side I/O);
    // E3 failed projection/source creates no activity or child admission.
    // | Rule | C3 | C4 | C5 | Result |
    // | F1 | no  | yes | yes | same active generation 1 projected |
    // | F2 | yes | yes | yes | generation 2 projected before child, E2 |
    // | F3 | yes | no  | yes | failure before child/activity |
    // | F4 | yes | yes | no  | failure before projection/child/activity |
    // | F5 | old missing history | yes | yes | Unknown, no recovery/fresh effects |
    // The existing complete projection is the only staging port; this test
    // does not copy a resource selector or fabricate a physical publication.
    for (rule, pending, fail_projection, valid_source, unknown_history) in [
        ("F1", false, false, true, false),
        ("F2", true, false, true, false),
        ("F3", true, true, true, false),
        ("F4", true, false, false, false),
        ("F5", true, false, true, true),
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
        // Original Missing is scripted independently of the attempted enqueue.
        runtime.set_admission_observations([Ok(None)]);
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
        if unknown_history {
            // Original legacy bytes, not fresh empty history synthesized later.
            let mut wire = serde_json::to_value(session).unwrap();
            wire.as_object_mut().unwrap().remove("run_activities");
            session = serde_json::from_value(wire).unwrap();
        }
        create(repo.as_ref(), session).await;
        let app = configured_admission_application(repo.clone(), runtime.clone());
        let result =
            awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
                &app,
                coordination_spawn_command(&id),
            )
            .await;
        if fail_projection || !valid_source || unknown_history {
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
            if !valid_source || unknown_history {
                assert!(
                    runtime.installed_projections.lock().unwrap().is_empty(),
                    "F4/F5 no projection"
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
        runtime.set_admission_observations([Ok(None), Ok(None)]);
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
        let child = awaken_agent_contract::agent::thread::Id("cold-child".into());
        let run = awaken_agent_contract::agent::run::Id("cold-child-run".into());
        // This is a new claimed child, not legacy evidence reconstructed later.
        let (_, epoch) = app
            .activities
            .begin_activity_for_operation_with_policy(
                &SessionId::from(&id),
                "cold-child-report",
                false,
                None,
                None,
                Some((&child, &run)),
            )
            .await
            .unwrap();
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

#[tokio::test]
async fn coordinated_agent_admission_preserves_activity_across_ambiguous_delivery() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C0 committed source validation accepts/rejects; C1
    // Runtime admission is accepted, definitively rejected, ambiguously
    // unavailable, or returns a mismatched receipt; C2 an ambiguous
    // operation is retried with the same identity; C3 its committed child boundary
    // later arrives; C4 a terminal child atomically admits its deterministic
    // primary report continuation; C5 that report Run later reaches its committed
    // boundary. Effects: E1 accepted work retains one active epoch; E2 only a
    // BadRequest settles immediately; E0 invalid source reaches neither activity
    // nor Runtime admission; E3 unavailable/internal outcomes retain the
    // epoch as durable recovery evidence; E4 exact retry reuses the same child Run
    // and epoch; E5 Awaiting settles that one epoch exactly; E6 C4 transfers the
    // same epoch without an intermediate Idle; E7 C5 settles it exactly once;
    // E8 admission reserves 24 ordinary-child slots because the public 25-Thread
    // limit includes the primary Thread (Advisor consultations are exempt).
    //
    // | Rule | Runtime result | Exact retry | Boundary | Effect |
    // |---|---|---|---|---|
    // | S0 | source mismatch | no | none | E0 |
    // | S1 | accepted | no | pending | E1+E8 |
    // | S2 | BadRequest | no | none | E2 |
    // | S3 | Unavailable | accepted | pending | E3+E4 |
    // | S4 | mismatched receipt | no | unknown | E3 |
    // | S5 | S3 accepted retry | yes | Awaiting | E5 |
    // | S6 | accepted | no | child Ended | E6 |
    // | S7 | report accepted | no | primary Ended | E7 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("Agent admission repository"),
    );
    for id in [
        "send-invalid-source",
        "send-accepted",
        "send-rejected",
        "send-ambiguous",
        "send-mismatch",
    ] {
        create(repo.as_ref(), coordinated_session(id)).await;
    }

    let invalid_source_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    invalid_source_runtime.reject_agent_message_source();
    let invalid_source =
        configured_admission_application(repo.clone(), invalid_source_runtime.clone());
    let invalid_source_error =
        awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
            &invalid_source,
            coordination_spawn_command("send-invalid-source"),
        )
        .await
        .expect_err("S0 source mismatch");
    assert_eq!(
        invalid_source_error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "S0/E0"
    );
    assert!(
        invalid_source_runtime.admissions.lock().unwrap().is_empty(),
        "S0/E0 no Runtime admission"
    );
    assert!(
        repo.get(&SessionId::from("send-invalid-source"))
            .await
            .expect("S0 state")
            .active_activity_epochs
            .is_empty(),
        "S0/E0 no Session activity"
    );

    let accepted_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::Accepted,
    ]));
    accepted_runtime.set_admission_observations([Ok(None), Ok(None)]);
    let accepted = configured_admission_application(repo.clone(), accepted_runtime.clone());
    awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
        &accepted,
        coordination_spawn_command("send-accepted"),
    )
    .await
    .expect("S1 accepted");
    let accepted_epoch = accepted_runtime.admissions.lock().unwrap()[0]
        .session_activity_epoch
        .expect("fresh original activity");
    assert_eq!(
        accepted_runtime.admissions.lock().unwrap()[0].max_unarchived_threads,
        24,
        "S1/E8 primary plus ordinary children cannot exceed 25 Threads"
    );
    assert_eq!(
        repo.get(&SessionId::from("send-accepted"))
            .await
            .expect("S1 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([accepted_epoch]),
        "S1/E1"
    );

    // S1R: Live A plus the original activity returns ACK without fresh
    // projection, begin or admission. Error is unknown, never Missing; current
    // source validation still precedes this data-only recovery projection.
    let accepted_before = repo.get(&SessionId::from("send-accepted")).await.unwrap();
    accepted_runtime.set_admission_observations([Ok(Some(
        awaken_session_execution_contract::SessionAgentMessageReceipt {
            thread_id: accepted_runtime.admissions.lock().unwrap()[0]
                .thread_id
                .clone(),
        },
    ))]);
    accepted_runtime
        .projection_failure
        .store(true, Ordering::SeqCst);
    awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
        &accepted,
        coordination_spawn_command("send-accepted"),
    )
    .await
    .expect("S1R original ACK");
    assert_eq!(
        accepted_runtime.admissions.lock().unwrap().len(),
        1,
        "S1R no reenqueue"
    );
    assert_eq!(
        accepted_runtime.installed_projections.lock().unwrap().len(),
        1,
        "S1R no fresh projection"
    );
    assert_eq!(
        repo.get(&SessionId::from("send-accepted")).await.unwrap(),
        accepted_before,
        "S1R no root mutation"
    );
    accepted_runtime.set_admission_observations([Err(RunError::bad_request(
        "observer cannot prove acceptance",
    ))]);
    let unknown =
        awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
            &accepted,
            coordination_spawn_command("send-accepted"),
        )
        .await
        .expect_err("S1U unknown stays pending");
    assert_eq!(
        unknown.kind,
        awaken_session_types::RunErrorKind::Unavailable
    );
    assert_eq!(
        repo.get(&SessionId::from("send-accepted")).await.unwrap(),
        accepted_before,
        "S1U stutter"
    );
    let reads = accepted_runtime.admission_reads.load(Ordering::SeqCst);
    accepted_runtime.reject_agent_message_source();
    awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
        &accepted,
        coordination_spawn_command("send-accepted"),
    )
    .await
    .expect_err("S1A current source still required");
    assert_eq!(
        accepted_runtime.admission_reads.load(Ordering::SeqCst),
        reads,
        "S1A validation first"
    );
    accepted_runtime.source_valid.store(true, Ordering::SeqCst);
    accepted_runtime
        .projection_failure
        .store(false, Ordering::SeqCst);
    accepted_runtime.set_admission_observations([Ok(None)]);

    let rejected_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::BadRequest,
    ]));
    rejected_runtime.set_admission_observations([Ok(None)]);
    let rejected = configured_admission_application(repo.clone(), rejected_runtime);
    let rejected_error =
        awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
            &rejected,
            coordination_spawn_command("send-rejected"),
        )
        .await
        .expect_err("S2 definitive rejection");
    assert_eq!(
        rejected_error.kind,
        awaken_session_types::RunErrorKind::BadRequest
    );
    assert!(
        repo.get(&SessionId::from("send-rejected"))
            .await
            .expect("S2 state")
            .active_activity_epochs
            .is_empty(),
        "S2/E2"
    );

    let ambiguous_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::Unavailable,
        AgentAdmissionOutcome::Accepted,
    ]));
    ambiguous_runtime.set_admission_observations([Ok(None), Ok(None)]);
    let ambiguous = configured_admission_application(repo.clone(), ambiguous_runtime.clone());
    awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
        &ambiguous,
        coordination_spawn_command("send-ambiguous"),
    )
    .await
    .expect_err("S3 ambiguous response");
    let ambiguous_epoch = ambiguous_runtime.admissions.lock().unwrap()[0]
        .session_activity_epoch
        .expect("fresh original activity");
    assert_eq!(
        repo.get(&SessionId::from("send-ambiguous"))
            .await
            .expect("S3 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([ambiguous_epoch]),
        "S3/E3"
    );
    awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
        &ambiguous,
        coordination_spawn_command("send-ambiguous"),
    )
    .await
    .expect("S3/S4 exact retry accepted");
    let (child, run) = {
        let admissions = ambiguous_runtime.admissions.lock().unwrap();
        assert_eq!(admissions.len(), 2, "S3/S4 two delivery attempts");
        assert_eq!(admissions[0].thread_id, admissions[1].thread_id, "S4/E4");
        assert_eq!(admissions[0].run_id, admissions[1].run_id, "S4/E4");
        assert_eq!(
            admissions[0].session_activity_epoch, admissions[1].session_activity_epoch,
            "S4/E4"
        );
        (
            admissions[0].thread_id.clone(),
            admissions[0].run_id.clone(),
        )
    };

    let mismatch_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::MismatchedReceipt,
    ]));
    mismatch_runtime.set_admission_observations([Ok(None)]);
    let mismatch = configured_admission_application(repo.clone(), mismatch_runtime.clone());
    let mismatch_error =
        awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
            &mismatch,
            coordination_spawn_command("send-mismatch"),
        )
        .await
        .expect_err("S4 mismatched receipt");
    assert_eq!(
        mismatch_error.kind,
        awaken_session_types::RunErrorKind::Internal
    );
    let mismatch_epoch = mismatch_runtime.admissions.lock().unwrap()[0]
        .session_activity_epoch
        .expect("fresh original activity");
    assert_eq!(
        repo.get(&SessionId::from("send-mismatch"))
            .await
            .expect("S4 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([mismatch_epoch]),
        "S4/E3"
    );

    ambiguous_runtime.commit_boundary("send-ambiguous", &child, &run, RunState::Awaiting, "");
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &ambiguous,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: "send-ambiguous".into(),
            source_thread_id: child,
            source_run_id: run,
            source_agent_id: "coord-child".into(),
            session_activity_epoch: ambiguous_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S5 committed Awaiting boundary");
    assert!(
        repo.get(&SessionId::from("send-ambiguous"))
            .await
            .expect("S5 state")
            .active_activity_epochs
            .is_empty(),
        "S5/E5"
    );

    let accepted_admission = accepted_runtime.admissions.lock().unwrap()[0].clone();
    accepted_runtime.commit_boundary(
        "send-accepted",
        &accepted_admission.thread_id,
        &accepted_admission.run_id,
        RunState::Ended(EndCause::NaturalEnd),
        "research complete",
    );
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: "send-accepted".into(),
            source_thread_id: accepted_admission.thread_id.clone(),
            source_run_id: accepted_admission.run_id.clone(),
            source_agent_id: "coord-child".into(),
            session_activity_epoch: accepted_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S6 terminal child hands off to the report continuation");
    {
        let continuations = accepted_runtime.continuations.lock().unwrap();
        assert_eq!(continuations.len(), 1, "S6/E6 one report continuation");
        assert_eq!(
            continuations[0].session_activity_epoch, accepted_epoch,
            "S6/E6 transfers the exact child epoch"
        );
        assert!(
            continuations[0]
                .message
                .text_content()
                .contains("research complete"),
            "S6/E6 report is derived from the committed child snapshot"
        );
    }
    let handed_off = repo
        .get(&SessionId::from("send-accepted"))
        .await
        .expect("S6 state");
    assert_eq!(
        handed_off.active_activity_epochs,
        std::collections::BTreeSet::from([accepted_epoch]),
        "S6/E6 no intermediate aggregate Idle"
    );
    assert_eq!(
        handed_off.execution,
        SessionExecutionState::Running,
        "S6/E6"
    );

    // S6R: report ACK was lost after original acceptance. Neither usage /
    // interval observation nor root preparation may run again on Some.
    let report_before = repo.get(&SessionId::from("send-accepted")).await.unwrap();
    let projection_count = accepted_runtime.installed_projections.lock().unwrap().len();
    accepted_runtime.set_admission_observations([Ok(Some(
        awaken_session_execution_contract::SessionAgentMessageReceipt {
            thread_id: ThreadId("send-accepted".into()),
        },
    ))]);
    accepted_runtime
        .projection_failure
        .store(true, Ordering::SeqCst);
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: "send-accepted".into(),
            source_thread_id: accepted_admission.thread_id.clone(),
            source_run_id: accepted_admission.run_id.clone(),
            source_agent_id: "coord-child".into(),
            session_activity_epoch: accepted_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S6R original report ACK");
    assert_eq!(
        accepted_runtime.continuations.lock().unwrap().len(),
        1,
        "S6R no report replay effect"
    );
    assert_eq!(
        accepted_runtime.installed_projections.lock().unwrap().len(),
        projection_count,
        "S6R no fresh projection"
    );
    assert_eq!(
        repo.get(&SessionId::from("send-accepted")).await.unwrap(),
        report_before,
        "S6R no usage / interval mutation"
    );

    accepted_runtime.commit_boundary(
        "send-accepted",
        &ThreadId("send-accepted".into()),
        &RunId("coord-report-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "coordinator acknowledged report",
    );
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: "send-accepted".into(),
            source_thread_id: ThreadId("send-accepted".into()),
            source_run_id: RunId("coord-report-run".into()),
            source_agent_id: "coord-root".into(),
            session_activity_epoch: accepted_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S7 primary report boundary settles the transferred epoch");
    let completed = repo
        .get(&SessionId::from("send-accepted"))
        .await
        .expect("S7 state");
    assert!(completed.active_activity_epochs.is_empty(), "S7/E7");
    assert_eq!(completed.execution, SessionExecutionState::Idle, "S7/E7");
    assert_eq!(
        accepted_runtime.continuations.lock().unwrap().len(),
        1,
        "S7/E7 primary boundary must not recurse into another report"
    );
    // S7M: confirmed Missing cannot revive an operation whose original activity
    // has already settled; only sufficient accepted evidence permits an ACK.
    accepted_runtime.set_admission_observations([Ok(None)]);
    let missing =
        awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
            &accepted,
            coordination_spawn_command("send-accepted"),
        )
        .await
        .expect_err("S7M settled Missing");
    assert_eq!(
        missing.kind,
        awaken_session_types::RunErrorKind::Unavailable
    );
    assert_eq!(
        repo.get(&SessionId::from("send-accepted")).await.unwrap(),
        completed,
        "S7M no new epoch"
    );
}
