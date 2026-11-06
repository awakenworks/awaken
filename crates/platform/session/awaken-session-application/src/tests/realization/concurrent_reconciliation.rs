//! Concurrent drivers of one accepted Resource generation share the root CAS.
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum PauseAt {
    StartReplay,
    StartConflict,
    Commit,
}

#[derive(Clone, Copy, Debug)]
enum Winner {
    Exact,
    Amended,
    Later,
    RolledBack,
}

/// Causes: C1 the first public detach pauses at its start/commit CAS; C2 another
/// driver commits the exact desired pair, an amended same-generation manifest,
/// a later generation, or a rollback; C3 the paused CAS really returns Replay
/// or Conflict. Effects: E1 an exact completed pair converges successfully;
/// E2 another pair/rollback stays Conflict; E3 returning the winner neither
/// writes another root nor repeats physical application; E4 the first command
/// keeps its original receipt. Every result comes from the original SQLite
/// repository and pause_next_mutation, not a fabricated CAS result.
///
/// | Rule | Pause/result | Winner | Effect |
/// |---|---|---|---|
/// | R1 | start/Replayed | exact active pair | E1 + E3 + E4 |
/// | R2 | start/Conflict after title-only CAS | exact active pair | E1 + E3 + E4 |
/// | R3 | commit/Conflict | exact active pair | E1 + E3 + E4 |
/// | R4 | start/Conflict | same revision, amended desired | E2 + E3 |
/// | R5 | commit/Conflict | later active generation | E2 + E3 |
/// | R6 | start/Conflict | legal rollback to prior active pair | E2 + E3 |
/// | R7 | start/Replayed | later root returned with the old start receipt | E2 + E3 |
/// R4 amends only before any attempt commits, using the existing public command.
/// R6 commits the legal aggregate rollback through the original CAS; it tests
/// logical generation classification, not a provider's physical rollback.
async fn reconciliation_race(pause_at: PauseAt, winner: Winner) {
    let durable = sqlite_session_repository("Resource reconciliation race").await;
    let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = Arc::new(application_with_persistence(
        runtime.clone(),
        SessionPersistence::new(
            repo.clone(),
            Arc::new(awaken_lease_testkit::ManualClock::new(100)),
        ),
    ));
    let id = SessionId::from(format!("resource-race-{pause_at:?}-{winner:?}"));
    let mut original = persisted(id.as_str(), false, "idle");
    original.resources =
        awaken_session_contract::SessionResourceState::from_active(file_resources("installed"));
    let binding = original.resources.active.inputs()[0].binding_id.clone();
    create(repo.as_ref(), original).await;
    let before = durable.get(&id).await.unwrap();
    let original_command_revision = before.revision.0 + 1;
    let pause = repo.pause_next_mutation(match pause_at {
        PauseAt::StartReplay | PauseAt::StartConflict => "resource-reconcile-attempt",
        PauseAt::Commit => "resource-reconcile-active",
    });
    let delayed = app.clone();
    let delayed_id = id.clone();
    let task = tokio::spawn(async move {
        Box::pin(
            delayed
                .resource_manifests
                .detach_session_input(&delayed_id, &binding),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
        .await
        .expect("the public detach reaches the selected real CAS");
    let prepared = durable.get(&id).await.unwrap();
    let resource_revision = prepared.resources.revision;
    let desired = prepared.resources.pending.clone().unwrap();
    assert!(desired.inputs().is_empty(), "the detach intent is durable");
    assert_eq!(resource_revision, before.resources.revision + 1);

    if matches!(pause_at, PauseAt::StartConflict) {
        // A title-only root write makes B's automatic start record distinct
        // from A's, while preserving the accepted Resource generation.
        let title_before = durable.get(&id).await.unwrap();
        let mut title = title_before.clone();
        title.title = Some("concurrent root metadata".into());
        let _ = app
            .mutations
            .commit_session_snapshot_with_outcome(
                "workspace",
                awaken_store_conformance::session_store::record_update(&title_before, title, 100),
                "resource-race-title",
                Vec::new(),
            )
            .await
            .unwrap()
            .into_session();
    }

    match winner {
        Winner::Exact | Winner::Later => {
            Box::pin(
                app.resource_reconciliation
                    .reconcile_persisted_resources("workspace", durable.get(&id).await.unwrap()),
            )
            .await
            .expect("the competing original reconciler settles the target");
            if matches!(winner, Winner::Later) {
                Box::pin(app.resource_manifests.replace_session_resource_manifest(
                    &id,
                    ReplaceSessionResourceManifest {
                        resources: file_resources("later"),
                        expected_resource_revision: Some(resource_revision),
                        idempotency_key: Some("later-generation".into()),
                        request_fingerprint: "later-generation".into(),
                    },
                ))
                .await
                .expect("a later Resource generation commits independently");
            }
        }
        Winner::Amended => {
            Box::pin(app.resource_manifests.replace_session_resource_manifest(
                &id,
                ReplaceSessionResourceManifest {
                    resources: file_resources("amended"),
                    expected_resource_revision: Some(resource_revision),
                    idempotency_key: Some("amended-generation".into()),
                    request_fingerprint: "amended-generation".into(),
                },
            ))
            .await
            .expect("an unattempted pending generation may be amended");
        }
        Winner::RolledBack => {
            let mut rollback = app.mutations.session_aggregate(&id).await.unwrap();
            rollback.resources().rollback("competing rollback").unwrap();
            let _ = app
                .mutations
                .commit_session_snapshot_with_outcome(
                    "workspace",
                    rollback,
                    "resource-race-rollback",
                    Vec::new(),
                )
                .await
                .unwrap()
                .into_session();
        }
    }
    let completed = durable.get(&id).await.unwrap();
    assert!(completed.resources.pending.is_none(), "the winner settled");
    match winner {
        Winner::Exact => assert_eq!(
            completed.resources.active_generation(),
            (resource_revision, &desired),
            "the exact revision and complete manifest are one coordinate"
        ),
        Winner::Amended => {
            assert_eq!(completed.resources.active_revision(), resource_revision);
            assert_ne!(completed.resources.active, desired);
        }
        Winner::Later => assert!(
            completed.resources.active_revision() > resource_revision,
            "the later generation cannot stand in for the accepted target"
        ),
        Winner::RolledBack => {
            assert_eq!(completed.resources.revision, resource_revision);
            assert_eq!(
                completed.resources.active_generation(),
                before.resources.active_generation(),
                "rollback preserves the prior active pair, not the watermark"
            );
        }
    }
    let applied_before_release = runtime.counts().resource_applies;
    let expected_applies = match (pause_at, winner) {
        (PauseAt::Commit, Winner::Later) => 3,
        (PauseAt::Commit, _) => 2,
        (_, Winner::Later) => 2,
        (_, Winner::RolledBack) => 0,
        _ => 1,
    };
    assert_eq!(applied_before_release, expected_applies);
    pause.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the paused driver returns inline")
        .expect("the paused driver does not panic");
    let actual_cas = pause.result.lock().unwrap().clone().unwrap();
    match pause_at {
        PauseAt::StartReplay => assert!(
            matches!(
                actual_cas,
                awaken_session_contract::SessionMutationResult::Replayed { .. }
            ),
            "R1 actual original-store Replay: {actual_cas:?}"
        ),
        PauseAt::StartConflict | PauseAt::Commit => assert!(
            matches!(
                actual_cas,
                awaken_session_contract::SessionMutationResult::Conflict { .. }
            ),
            "R2-R6 actual original-store Conflict: {actual_cas:?}"
        ),
    }
    if matches!(winner, Winner::Exact) {
        let outcome =
            result.expect("the exact already-active generation is successful convergence");
        assert!(outcome.command_applied, "the original detach applied once");
        assert_eq!(outcome.command_revision.0, original_command_revision);
        assert_eq!(outcome.session, completed, "return the actual winner root");
    } else {
        assert!(
            matches!(
                result,
                Err(SessionResourceManifestError::ProjectionAfterCommit {
                    source: SessionPreparationError::Conflict,
                    ..
                })
            ),
            "another pair or rollback remains Conflict: {result:?}"
        );
    }
    assert_eq!(
        durable.get(&id).await.unwrap(),
        completed,
        "no extra root CAS"
    );
    assert_eq!(runtime.counts().resource_applies, applied_before_release);
    assert!(runtime.resource_rollbacks.lock().unwrap().is_empty());
    assert_eq!(runtime.counts().model_calls, 0);
    assert_eq!(runtime.counts().tool_calls, 0);
}

#[tokio::test]
async fn concurrent_resource_start_replay_accepts_the_exact_committed_generation() {
    // R1: same start record replays after B settles; do not reapply its target.
    Box::pin(reconciliation_race(PauseAt::StartReplay, Winner::Exact)).await;
}

#[tokio::test]
async fn concurrent_resource_start_conflict_accepts_the_exact_committed_generation() {
    // R2: a root-only title CAS changes the start record, not Resource identity.
    Box::pin(reconciliation_race(PauseAt::StartConflict, Winner::Exact)).await;
}

#[tokio::test]
async fn concurrent_resource_settlement_conflict_accepts_the_exact_committed_generation() {
    // R3: both drivers applied generation N; B settles before A's active CAS.
    Box::pin(reconciliation_race(PauseAt::Commit, Winner::Exact)).await;
}

#[tokio::test]
async fn concurrent_resource_reconciliation_rejects_another_or_rolled_back_generation() {
    // R4-R7: vary one coordinate/outcome; Replay must also recheck the later root.
    Box::pin(reconciliation_race(PauseAt::StartConflict, Winner::Amended)).await;
    Box::pin(reconciliation_race(PauseAt::Commit, Winner::Later)).await;
    Box::pin(reconciliation_race(
        PauseAt::StartConflict,
        Winner::RolledBack,
    ))
    .await;
    Box::pin(reconciliation_race(PauseAt::StartReplay, Winner::Later)).await;
}
