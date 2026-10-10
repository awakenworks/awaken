use super::*;
use crate::tests::authority::unattempted_profiled_release::unattempted_with_placement;
use awaken_session_state_contract::{ManagedLifecycleFact, SessionEnvironmentState};

fn fact(id: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: awaken_session_event_contract::session_terminated_fact_id(id),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: awaken_session_event_contract::SESSION_TERMINATED.into(),
        timestamp: 1,
        runtime_interval: None,
    }
}

async fn seed_expired_cleanup_affinity(
    repository: Arc<FaultingSessionRepository>,
    id: &str,
    witness: bool,
    live: bool,
    physical: bool,
    child: bool,
) -> (SessionApplication, Arc<RecordingCleanupRuntime>) {
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let app = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repository.clone(), system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let mut root = unattempted_with_placement(id, SessionRuntimePlacement::Worker);
    if !witness {
        root.execution = SessionExecutionState::Idle;
    }
    create(repository.as_ref(), root).await;
    runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
    if witness {
        let _pending = app
            .release_unattempted_profiled_session(id, "2026-10-10T00:00:00Z", fact(id))
            .await;
    } else {
        app.terminate_session(id, "2026-10-10T00:00:00Z", fact(id))
            .await
            .unwrap();
    }
    let mut root = repository.get(&SessionId::from(id)).await.unwrap();
    assert!(matches!(
        root.disposition,
        awaken_session_state_contract::SessionDisposition::Archived { .. }
    ));
    // Controlled inputs reproduce the old Worker's first cleanup-only claim
    // after the real archive CAS. Its empty Runtime tree observation freezes
    // the canonical operation; no preparation/disposal receipt is authored.
    root.freeze_terminal_cleanup_targets(
        if child {
            vec![format!("{id}-child")]
        } else {
            Vec::new()
        },
        0,
        0,
    )
    .unwrap();
    root.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
        owner: "old-worker".into(),
        runtime_incarnation: "old-worker:1:old-boot".into(),
        epoch: 4,
        expires_at_unix_ms: if live { u64::MAX } else { 1 },
    });
    if physical {
        root.environment = SessionEnvironmentState::Resident {
            binding: "historical-binding".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    }
    let revision = root.revision;
    let payload = awaken_session_state_contract::SessionMutationPayload::Replace(root);
    repository
        .commit(
            "workspace",
            awaken_session_state_contract::SessionMutation {
                expected_revision: revision,
                idempotency: awaken_session_state_contract::IdempotencyRecord {
                    key: format!("test:initial-cleanup-affinity:{id}"),
                    payload_hash: payload.stable_hash(),
                },
                payload,
                lifecycle_facts: Vec::new(),
            },
        )
        .await
        .unwrap();
    let target = target();
    let workers = Arc::new(RecordingWorkerObservations::default());
    let registered = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    assert_eq!(
        registered.snapshot.identity.lease_owner(),
        target.runtime_incarnation,
        "fixture uses the existing Registry's exact owner/generation/incarnation"
    );
    workers.replace(vec![registered]);
    app.set_worker_observation_source(workers).unwrap();
    (app, runtime)
}

fn target() -> awaken_session_realization_contract::SessionRealizationTarget {
    awaken_session_realization_contract::SessionRealizationTarget {
        owner: "new-worker".into(),
        runtime_incarnation: "new-worker:1:new-boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    }
}

#[tokio::test]
async fn expired_foreign_cleanup_affinity_requires_exact_no_create_and_pristine_root() {
    // Cause graph: C1 exact unattempted archive receipt; C2 old lease expired;
    // C3 Unmaterialized; C4 pristine root-only request; C5 current Registry.
    // F1 all -> the existing CAS allocates epoch 5 and projects no-Create.
    // F2 one false C1..C4 -> zero claim/root change/Runtime effect; old affinity
    // survives. Ordinary/unknown roots cannot borrow the modern receipt.
    for (rule, witness, live, physical, child, allowed) in [
        ("F1", true, false, false, false, true),
        ("F2-unknown", false, false, false, false, false),
        ("F2-live", true, true, false, false, false),
        ("F2-physical", true, false, true, false, false),
        ("F2-child", true, false, false, true, false),
    ] {
        let durable = sqlite_session_repository("affinity witness repository").await;
        let repository = Arc::new(FaultingSessionRepository::new(durable));
        let id = format!("no-create-affinity-{rule}");
        let (app, runtime) =
            seed_expired_cleanup_affinity(repository.clone(), &id, witness, live, physical, child)
                .await;
        let before = repository.get(&SessionId::from(&id)).await.unwrap();
        let effects_before = runtime.effective_ids.lock().unwrap().clone();
        let assignment = app
            .claim_next_terminal_cleanup(target(), None)
            .await
            .unwrap();
        assert_eq!(assignment.is_some(), allowed, "{rule}");
        let after = repository.get(&SessionId::from(&id)).await.unwrap();
        if let Some(assignment) = assignment {
            assert!(
                assignment.environment_creation_not_attempted,
                "F1 exact witness"
            );
            assert_eq!(assignment.authority.lease.owner, "new-worker", "F1");
            assert_eq!(assignment.authority.lease.epoch, 5, "F1");
            assert_eq!(after.revision.0, before.revision.0 + 1, "F1 one CAS");
        } else {
            assert_eq!(after, before, "{rule} no Root mutation");
        }
        assert_eq!(
            *runtime.effective_ids.lock().unwrap(),
            effects_before,
            "{rule} no Runtime effect during claim"
        );
    }
}

#[tokio::test]
async fn no_create_affinity_claim_preserves_unavailable_authority_and_lost_cas_replay() {
    // F3 receipt point-read unavailable -> error, exact unchanged Root, no
    // guessed absence. F4 commit wins/response lost -> the existing retry reads
    // its exact current lease; one epoch increment, one CAS, no second claim.
    for unavailable in [true, false] {
        let durable = sqlite_session_repository("affinity fault repository").await;
        let repository = Arc::new(FaultingSessionRepository::new(durable));
        let id = if unavailable {
            "no-create-affinity-unavailable"
        } else {
            "no-create-affinity-response-loss"
        };
        let (app, _) =
            seed_expired_cleanup_affinity(repository.clone(), id, true, false, false, false).await;
        let before = repository.get(&SessionId::from(id)).await.unwrap();
        if unavailable {
            repository
                .fail_idempotency_receipt_once
                .store(true, Ordering::SeqCst);
        } else {
            repository.commit_then_conflict_once("claim-terminal-cleanup-recovery");
        }
        let result = app.claim_next_terminal_cleanup(target(), None).await;
        let after = repository.get(&SessionId::from(id)).await.unwrap();
        if unavailable {
            assert!(matches!(result, Err(awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(_))), "F3");
            assert_eq!(after, before, "F3 authority failure cannot alter affinity");
        } else {
            let assignment = result.unwrap().expect("F4 replay assignment");
            assert_eq!(assignment.authority.lease.epoch, 5, "F4");
            assert_eq!(
                after.revision.0,
                before.revision.0 + 1,
                "F4 no duplicate CAS"
            );
        }
    }
}
