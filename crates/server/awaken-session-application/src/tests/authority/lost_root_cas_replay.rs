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
    session.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: format!("{session_id}-source"),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    session.resources = awaken_session_contract::SessionResourceState::from_active(
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
                {
                    let probe = std::sync::Arc::new(crate::ExecutionObservationProbe::default());
                    crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
                },
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
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
        archived_at: awaken_time::utc::epoch_millis_to_rfc3339(route_sample_ms),
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
            .termination_commands
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
    let awaken_session_contract::SessionMutationPayload::Replace(delayed_root) = &candidate.payload
    else {
        panic!("C1 archive replaces one root")
    };
    assert_eq!(
        delayed_root.archived_at_unix_ms(),
        Some(100),
        "C1 delayed sample"
    );

    let winner = winner_app
        .termination_commands
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
            awaken_session_contract::SessionMutationResult::Replayed { .. }
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
                .termination_commands
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
                        .termination_commands
                        .terminate_session(&delayed_id, "2026-09-19T00:00:01Z", delayed_fact)
                        .await
                }
                "force" => {
                    delayed
                        .termination_commands
                        .force_terminate_session(&delayed_id, "2026-09-19T00:00:01Z", delayed_fact)
                        .await
                }
                "release" => {
                    delayed
                        .termination_commands
                        .release_terminal_session(&delayed_id, "2026-09-19T00:00:01Z", delayed_fact)
                        .await
                }
                _ => {
                    delayed
                        .lifecycle
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
                app.termination_commands
                    .terminate_session(&session_id, "2026-09-19T00:00:02Z", fact.clone())
                    .await
            }
            "force" => {
                app.termination_commands
                    .force_terminate_session(&session_id, "2026-09-19T00:00:02Z", fact.clone())
                    .await
            }
            "release" => {
                app.termination_commands
                    .release_terminal_session(&session_id, "2026-09-19T00:00:02Z", fact.clone())
                    .await
            }
            _ => {
                app.lifecycle
                    .commit_delete_intent(SessionDeleteCommand::at_for_test(&session_id, 11))
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
                awaken_session_contract::SessionMutationResult::Replayed { .. }
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

#[tokio::test]
async fn resource_manifest_replay_uses_the_committed_receipt_without_a_late_query() {
    // R9 causes: the public command's early receipt probe is absent; its actual
    // compiled CAS pauses; the same mutation wins, then a legal title-only
    // update advances the root. Arm the EXISTING receipt-read outage only now.
    // Effects: a real Replayed CAS returns its original receipt revision and
    // newer root, without a second fallible receipt lookup after acceptance.
    // R10 then uses that still-armed fault at the ORIGINAL early replay probe:
    // it must remain unavailable, not silently skip admission/read authority.
    // Resource targets stay empty and unchanged throughout; this is not a
    // positive oracle for replay against a different Resource generation.
    // | Rule | First probe | CAS | Later receipt IO | Required result |
    // | R9 | absent | actual Replayed + newer title | outage armed | Ok, original revision |
    // | R10 | outage armed | not entered | none | original Unavailable |
    let durable = authority_repository("manifest receipt repository").await;
    let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let id = SessionId::from("manifest-receipt-replay");
    create(repo.as_ref(), persisted(id.as_str(), false, "idle")).await;
    let app = Arc::new(application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let before = durable.get(&id).await.unwrap();
    assert!(
        before.resources.active.inputs().is_empty(),
        "R9 unchanged empty targets"
    );
    let command = crate::ReplaceSessionResourceManifest {
        resources: before.resources.desired().clone(),
        expected_resource_revision: Some(before.resources.revision),
        idempotency_key: Some("receipt-source".into()),
        request_fingerprint: "same-original-manifest".into(),
    };
    let pause = repo.pause_next_mutation("managed:resource-manifest-command:");
    let delayed = app.clone();
    let delayed_id = id.clone();
    let task = tokio::spawn(async move {
        Box::pin(
            delayed
                .resource_manifests
                .replace_session_resource_manifest(&delayed_id, command),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
        .await
        .expect("R9 public command passed the early probe and reached canonical CAS");
    let candidate = pause.candidate.lock().unwrap().clone().unwrap();
    assert_eq!(
        candidate.expected_revision, before.revision,
        "R9 original candidate"
    );
    let original_revision = match durable
        .commit("workspace", candidate.clone())
        .await
        .unwrap()
    {
        awaken_session_contract::SessionMutationResult::Applied { new_revision } => new_revision,
        other => panic!("R9 exact compiled command must apply first: {other:?}"),
    };
    let applied = durable.get(&id).await.unwrap();
    assert_eq!(
        applied.resources, before.resources,
        "R9 no Resource target change"
    );
    let mut newer = applied.clone();
    newer.title = Some("later legal metadata".into());
    let newer = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            awaken_store_conformance::session_store::record_update(&applied, newer, 0),
            "manifest-receipt-title",
            Vec::new(),
        )
        .await
        .unwrap()
        .into_session();
    assert!(
        newer.revision > original_revision,
        "R9 receipt differs from newer reload"
    );
    repo.fail_idempotency_receipt_once
        .store(true, Ordering::SeqCst);
    pause.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("R9 inline command completes")
        .expect("R9 command does not panic");
    assert!(
        matches!(
            pause.result.lock().unwrap().clone().unwrap(),
            awaken_session_contract::SessionMutationResult::Replayed { new_revision }
                if new_revision == original_revision
        ),
        "R9 actual canonical CAS replay, not a selector or mock shortcut"
    );
    let outcome =
        result.expect("R9 accepted manifest must use SnapshotCommit receipt, not late fallible IO");
    assert!(
        !outcome.command_applied,
        "R9 replay is not another application"
    );
    assert_eq!(
        outcome.command_revision, original_revision,
        "R9 original receipt"
    );
    assert_eq!(outcome.session, newer, "R9 exact newer root");
    assert!(
        repo.fail_idempotency_receipt_once.load(Ordering::SeqCst),
        "R9 no late receipt query"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        newer,
        "R9 no extra mutation"
    );
    let probe = app
        .resource_manifests
        .replay_session_resource_manifest(&id, "receipt-source", "same-original-manifest")
        .await;
    assert!(
        matches!(
            probe,
            Err(crate::SessionResourceManifestError::Unavailable(_))
        ),
        "R10 original early probe still reports receipt outage: {probe:?}"
    );
    assert!(
        !repo.fail_idempotency_receipt_once.load(Ordering::SeqCst),
        "R10 actual fault consumed"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        newer,
        "R10 no extra mutation"
    );
}
