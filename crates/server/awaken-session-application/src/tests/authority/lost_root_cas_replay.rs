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
    Arc<SessionApplication>,
) {
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
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
    let application_at = |time| {
        Arc::new(
            SessionApplicationBuilder::fixture(
                runtime.clone(),
                Arc::new(NoopMcpRealizer),
                SessionPersistence::new(
                    repo.clone(),
                    Arc::new(awaken_lease_testkit::ManualClock::new(time)),
                ),
                Arc::new(RecordingEnvironmentSource::default()),
            )
            .build()
            .expect("valid fixture Store/Clock composition"),
        )
    };
    let delayed = application_at(100);
    let winner = application_at(200);
    (durable, repo, runtime, delayed, winner)
}

fn publication_command(
    session_id: &str,
    route_sample_ms: u64,
) -> SessionArchiveWithRepositoryPublicationCommand {
    SessionArchiveWithRepositoryPublicationCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        archived_at: awaken_session_application_contract::epoch_millis_to_rfc3339(route_sample_ms),
        lifecycle_fact: ManagedLifecycleFact {
            id: format!("{session_id}:terminated"),
            object_id: session_id.into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: i64::try_from(route_sample_ms / 1_000).unwrap(),
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
    // are compiled from one root revision with distinct Application clock
    // samples 100/200; C2 the delayed candidate actually
    // reaches the root CAS and the store answers Replayed (not Conflict, which
    // would be absorbed by the pre-CAS Archived check instead and so would not
    // observe this rule at all). Effects: E1 exactly one call reports the
    // transition; E2 the losing call returns the winner's complete aggregate,
    // original archived time and exact publication receipt despite a different
    // sampled clock; E3 the terminal lifecycle fact and physical publication
    // effect each happen once. The adjacent authority table retains rejection
    // of a genuinely different command.
    // R1 = C1 & C2 => E1 & E2 & E3.
    let session_id = "publication-cas-race";
    let (durable, repo, runtime, delayed_app, winner_app) =
        publication_race_fixture(session_id).await;
    let delayed_command = publication_command(session_id, 1_000);
    let winner_command = publication_command(session_id, 2_000);
    assert_ne!(
        delayed_command.archived_at, winner_command.archived_at,
        "C1 route display"
    );
    assert_ne!(
        delayed_command.lifecycle_fact.timestamp, winner_command.lifecycle_fact.timestamp,
        "C1 route fact time"
    );
    let origin = durable.get(&SessionId::from(session_id)).await.unwrap();

    let pause = repo.pause_next_mutation("session:archive-with-repository-publication:");
    let delayed = Arc::clone(&delayed_app);
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
    let awaken_session_state_contract::SessionMutationPayload::Replace(delayed_root) =
        &candidate.payload
    else {
        panic!("C1 archive replaces one root")
    };
    assert_eq!(
        delayed_root.archived_at_unix_ms(),
        Some(100),
        "C1 delayed sample"
    );

    let winner = winner_app
        .archive_with_repository_publication(winner_command)
        .await
        .expect("E1 winner releases");
    assert!(winner.mutation.transitioned(), "R1/E1 winner applied");
    let winner_root = durable.get(&SessionId::from(session_id)).await.unwrap();
    assert_eq!(
        winner_root.archived_at_unix_ms(),
        Some(200),
        "R1/E2 winner sample"
    );
    assert_eq!(
        durable
            .idempotency_receipt(&SessionId::from(session_id), &candidate.idempotency.key)
            .await
            .unwrap()
            .unwrap()
            .payload_hash,
        candidate.idempotency.payload_hash,
        "C1 same exact command key despite distinct samples"
    );
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
    let mut different_intent = publication_command(session_id, 3_000);
    different_intent.repository.expectation.commit =
        "1111111111111111111111111111111111111111".into();
    assert!(
        matches!(
            winner_app
                .archive_with_repository_publication(different_intent)
                .await,
            Err(SessionArchiveWithRepositoryPublicationError::Conflict)
        ),
        "R2 different Git intent refuses after the winning archive"
    );
    assert_eq!(
        durable.get(&SessionId::from(session_id)).await.unwrap(),
        winner_root,
        "R2 different intent cannot rewrite archive time"
    );
}

#[tokio::test]
async fn every_disposition_command_reports_a_lost_root_cas_as_a_replay() {
    // Diffusion across the sibling commands that publish the same
    // `SessionDispositionMutation` from the same aggregate, not only the one
    // that broke. Causes: C1 two requests with the same business intent race
    // one root revision; archive routes independently sample 1s/2s for their
    // display and fact timestamps. C2 the delayed candidate reaches root CAS.
    // Effects: E1 the winner reports the transition; E2 the loser reports a
    // replay and the winner's archive time; E3 the lifecycle outbox gains one
    // fact for the pair. Distinct admission/interval meaning is covered by the
    // archive identity table and must still conflict.
    // R1 public archive, R2 force terminate, R3 terminal release, R4 delete:
    //     C1 & C2 => E1 & E2 & E3.
    for (case, initial, mutation_key) in [
        ("public", "idle", "session:archive:"),
        ("force", "running", "session:archive:"),
        ("release", "activation_failed", "session:archive:"),
        ("delete", "idle", "session:delete-intent:"),
    ] {
        let session_id = format!("disposition-race-{case}");
        let durable = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
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
            timestamp: 2,
            runtime_interval: None,
        };

        let pause = repo.pause_next_mutation(mutation_key);
        let delayed = Arc::clone(&app);
        let delayed_id = session_id.clone();
        let mut delayed_fact = fact.clone();
        delayed_fact.timestamp = 1;
        let loser = tokio::spawn(async move {
            match case {
                "public" => {
                    delayed
                        .terminate_session(&delayed_id, "2026-09-19T00:00:01Z", delayed_fact)
                        .await
                }
                "force" => {
                    delayed
                        .force_terminate_session(&delayed_id, "2026-09-19T00:00:01Z", delayed_fact)
                        .await
                }
                "release" => {
                    delayed
                        .release_terminal_session(&delayed_id, "2026-09-19T00:00:01Z", delayed_fact)
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
        let candidate = pause.candidate.lock().unwrap().clone().unwrap();
        assert_eq!(
            candidate.expected_revision, origin.revision,
            "C1 {case} both requests start from the one origin revision"
        );

        let winner = match case {
            "public" => {
                app.terminate_session(&session_id, "2026-09-19T00:00:02Z", fact.clone())
                    .await
            }
            "force" => {
                app.force_terminate_session(&session_id, "2026-09-19T00:00:02Z", fact.clone())
                    .await
            }
            "release" => {
                app.release_terminal_session(&session_id, "2026-09-19T00:00:02Z", fact.clone())
                    .await
            }
            _ => {
                app.commit_delete_intent(SessionDeleteCommand::at_for_test(&session_id, 11))
                    .await
            }
        }
        .unwrap_or_else(|error| panic!("E1 {case} winner: {error:?}"));
        assert!(winner.transitioned(), "R1-R4/E1 {case} winner applied");
        let winner_facts = durable.pending_lifecycle().await.unwrap();
        if case != "delete" {
            assert_eq!(
                durable
                    .idempotency_receipt(&SessionId::from(&session_id), &candidate.idempotency.key,)
                    .await
                    .unwrap()
                    .unwrap()
                    .payload_hash,
                candidate.idempotency.payload_hash,
                "C1 {case} both route samples name the same command receipt"
            );
        }

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
            "R1-R4/E2 {case} a replayed CAS is not a second applied disposition"
        );
        if case != "delete" {
            assert_eq!(
                loser.session.disposition, winner.session.disposition,
                "R1-R3/E2 {case} one absorbing archive disposition"
            );
            assert_eq!(
                winner.session.archived_at(),
                Some("2026-09-19T00:00:02Z"),
                "R1-R3/E2 {case} winning route observation"
            );
        }
        assert_eq!(
            durable.pending_lifecycle().await.unwrap(),
            winner_facts,
            "R1-R4/E3 {case} the terminal fact is enqueued exactly once"
        );
    }
}
