//! A lost root CAS is reported as a replay, never as an applied repository publication or disposition.
use super::*;

/// Build one idle Session that owns exactly one writable Repository input, the
/// fixture the profiled release command publishes from.
async fn publication_race_fixture(
    session_id: &str,
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<FaultingSessionRepository>,
    Arc<RecordingCleanupRuntime>,
    Arc<SessionApplication>,
) {
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("publication race repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let mut session = persisted(session_id, false, "idle");
    session.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: format!("{session_id}-source"),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    session.resources = awaken_session_state_contract::SessionResourceState::from_active(
        repository_resources("source", "repo-1"),
    );
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    (durable, repo, runtime, app)
}

fn publication_command(session_id: &str) -> SessionArchiveWithRepositoryPublicationCommand {
    SessionArchiveWithRepositoryPublicationCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        archived_at: "2026-09-19T00:00:00Z".into(),
        lifecycle_fact: ManagedLifecycleFact {
            id: format!("{session_id}:terminated"),
            object_id: session_id.into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: 1,
            runtime_interval: None,
        },
        repository: SessionRepositoryPublicationSelector {
            binding_id: awaken_resource_contract::BindingId::from("source"),
            expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
                branch: "awf/issue-coding".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                expected_prior_commit: None,
            },
        },
    }
}

#[tokio::test]
async fn concurrent_repository_publication_archive_cannot_report_the_losing_cas_as_applied() {
    // The twin of the Delete fence. Causes: C1 two profiled release commands
    // are compiled from one root revision; C2 the delayed candidate actually
    // reaches the root CAS and the store answers Replayed (not Conflict, which
    // would be absorbed by the pre-CAS Archived check instead and so would not
    // observe this rule at all). Effects: E1 exactly one call reports the
    // transition; E2 the losing call returns the winner's complete aggregate
    // and its own exact publication receipt; E3 the terminal lifecycle fact and
    // the physical publication effect each happen once.
    // R1 = C1 & C2 => E1 & E2 & E3.
    let session_id = "publication-cas-race";
    let (durable, repo, runtime, app) = publication_race_fixture(session_id).await;
    let command = publication_command(session_id);
    let origin = durable.get(&SessionId::from(session_id)).await.unwrap();

    let pause = repo.pause_next_mutation("session:archive-with-repository-publication:");
    let delayed = Arc::clone(&app);
    let delayed_command = command.clone();
    let loser = tokio::spawn(async move {
        delayed
            .archive_with_repository_publication(delayed_command)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
        .await
        .expect("C1 delayed candidate reaches the root CAS");
    let candidate = pause.candidate.lock().unwrap().clone().unwrap();
    assert_eq!(
        candidate.expected_revision, origin.revision,
        "C1 both requests really start from the one origin revision"
    );

    let winner = app
        .archive_with_repository_publication(command)
        .await
        .expect("E1 winner releases");
    assert!(winner.mutation.transitioned(), "R1/E1 winner applied");
    let winner_root = durable.get(&SessionId::from(session_id)).await.unwrap();
    let facts_after_winner = durable.pending_lifecycle().await.unwrap();
    let effects_after_winner = runtime.terminal_effect_order.lock().unwrap().clone();

    pause.release.notify_one();
    let loser = tokio::time::timeout(std::time::Duration::from_secs(5), loser)
        .await
        .expect("R1 delayed candidate returns")
        .expect("R1 delayed task")
        .expect("R1 delayed release is not an error");
    assert!(
        matches!(
            pause.result.lock().unwrap().clone().unwrap(),
            awaken_session_state_contract::SessionMutationResult::Replayed { .. }
        ),
        "C2 the delayed candidate really lost a root CAS by receipt replay"
    );
    assert!(
        !loser.mutation.transitioned(),
        "R1/E1 a replayed CAS is not a second applied archive"
    );
    assert_eq!(
        loser.mutation.session, winner_root,
        "R1/E2 the loser returns the winner's complete aggregate"
    );
    assert_eq!(
        loser.publication_receipt, winner.publication_receipt,
        "R1/E2 one durable publication receipt"
    );
    assert_eq!(
        durable.pending_lifecycle().await.unwrap(),
        facts_after_winner,
        "R1/E3 the terminal fact is enqueued exactly once"
    );
    assert_eq!(
        *runtime.terminal_effect_order.lock().unwrap(),
        effects_after_winner,
        "R1/E3 no second physical publication or cleanup effect"
    );
}

#[tokio::test]
async fn every_disposition_command_reports_a_lost_root_cas_as_a_replay() {
    // Diffusion across the sibling commands that publish the same
    // `SessionDispositionMutation` from the same aggregate, not only the one
    // that broke. Causes: C1 two identical disposition commands race one root
    // revision; C2 the delayed candidate reaches the root CAS. Effects: E1 the
    // winner reports the transition; E2 the loser reports a replay; E3 the
    // lifecycle outbox gains exactly one fact for the pair.
    // R1 force terminate, R2 terminal release, R3 delete intent:
    //     C1 & C2 => E1 & E2 & E3.
    for (case, initial, mutation_key) in [
        ("force", "running", "session:archive:"),
        ("release", "activation_failed", "session:archive:"),
        ("delete", "idle", "session:delete-intent:"),
    ] {
        let session_id = format!("disposition-race-{case}");
        let durable = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
        create(repo.as_ref(), persisted(&session_id, false, initial)).await;
        let app = Arc::new(application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        ));
        let origin = durable.get(&SessionId::from(&session_id)).await.unwrap();
        let fact = ManagedLifecycleFact {
            id: format!("{session_id}:terminated"),
            object_id: session_id.clone(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: 7,
            runtime_interval: None,
        };

        let pause = repo.pause_next_mutation(mutation_key);
        let delayed = Arc::clone(&app);
        let delayed_id = session_id.clone();
        let delayed_fact = fact.clone();
        let loser = tokio::spawn(async move {
            match case {
                "force" => {
                    delayed
                        .force_terminate_session(&delayed_id, "2026-09-19T00:00:00Z", delayed_fact)
                        .await
                }
                "release" => {
                    delayed
                        .release_terminal_session(&delayed_id, "2026-09-19T00:00:00Z", delayed_fact)
                        .await
                }
                _ => {
                    delayed
                        .commit_delete_intent(SessionDeleteCommand::at_for_test(&delayed_id, 11))
                        .await
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
            .await
            .unwrap_or_else(|_| panic!("C1 {case} delayed candidate reaches the root CAS"));
        assert_eq!(
            pause
                .candidate
                .lock()
                .unwrap()
                .clone()
                .unwrap()
                .expected_revision,
            origin.revision,
            "C1 {case} both requests start from the one origin revision"
        );

        let winner = match case {
            "force" => {
                app.force_terminate_session(&session_id, "2026-09-19T00:00:00Z", fact.clone())
                    .await
            }
            "release" => {
                app.release_terminal_session(&session_id, "2026-09-19T00:00:00Z", fact.clone())
                    .await
            }
            _ => {
                app.commit_delete_intent(SessionDeleteCommand::at_for_test(&session_id, 11))
                    .await
            }
        }
        .unwrap_or_else(|error| panic!("E1 {case} winner: {error:?}"));
        assert!(winner.transitioned(), "R1-R3/E1 {case} winner applied");
        let winner_facts = durable.pending_lifecycle().await.unwrap();

        pause.release.notify_one();
        let loser = tokio::time::timeout(std::time::Duration::from_secs(5), loser)
            .await
            .unwrap_or_else(|_| panic!("{case} delayed candidate returns"))
            .unwrap()
            .unwrap_or_else(|error| panic!("E2 {case} loser: {error:?}"));
        assert!(
            matches!(
                pause.result.lock().unwrap().clone().unwrap(),
                awaken_session_state_contract::SessionMutationResult::Replayed { .. }
            ),
            "C2 {case} the delayed candidate really lost the root CAS by receipt replay"
        );
        assert!(
            !loser.transitioned(),
            "R1-R3/E2 {case} a replayed CAS is not a second applied disposition"
        );
        assert_eq!(
            durable.pending_lifecycle().await.unwrap(),
            winner_facts,
            "R1-R3/E3 {case} the terminal fact is enqueued exactly once"
        );
    }
}
