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
    incumbent_owner: &str,
) -> (
    SessionApplication,
    Arc<RecordingCleanupRuntime>,
    Arc<RecordingWorkerObservations>,
) {
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
        owner: incumbent_owner.into(),
        runtime_incarnation: format!("{incumbent_owner}:1:old-boot"),
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
    let root = repository.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        crate::archive_intent::unattempted_profiled_archive_witness(
            repository.as_ref(),
            "workspace",
            &root,
        )
        .await
        .unwrap(),
        witness,
        "fixture reads the real archive command receipt, never authors missing history",
    );
    let target = target(incumbent_owner);
    let workers = Arc::new(RecordingWorkerObservations::default());
    let registered = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    assert_eq!(
        registered.snapshot.identity.lease_owner(),
        target.runtime_incarnation,
        "fixture uses the existing Registry's exact owner/generation/incarnation"
    );
    workers.replace(vec![registered]);
    app.set_worker_observation_source(workers.clone()).unwrap();
    (app, runtime, workers)
}

fn target(incumbent_owner: &str) -> awaken_session_realization_contract::SessionRealizationTarget {
    awaken_session_realization_contract::SessionRealizationTarget {
        owner: "new-worker".into(),
        // Preserve the original foreign-owner generation1 fixture. A restart
        // of that same logical owner is a distinct generation2 installation,
        // not a replacement boot retrofitted onto its historical generation1.
        runtime_incarnation: if incumbent_owner == "new-worker" {
            "new-worker:2:new-boot"
        } else {
            "new-worker:1:new-boot"
        }
        .into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    }
}

#[tokio::test]
async fn expired_foreign_cleanup_affinity_requires_exact_no_create_and_pristine_root() {
    // Cause graph: C0 logical owner is same or foreign, while old Runtime is
    // always different; C1 exact original unattempted archive receipt; C2 old
    // lease expired; C3 Unmaterialized; C4 pristine root-only request; C5 current
    // Registry. Both C0 relations use the same existing receipt/CAS rule.
    // | Row | C0 | C1/C2/C3/C4 | effect |
    // | F1 | same/foreign | all true | one CAS, epoch5, genuine no-Create |
    // | F2 | same/foreign | one independently false | pending, zero CAS/effects |
    // Ordinary/unknown roots cannot borrow the modern receipt; restart is not
    // proof of closure and the fixture does not invent an old request ledger.
    for incumbent_owner in ["old-worker", "new-worker"] {
        for (rule, witness, live, physical, child, allowed) in [
            ("F1", true, false, false, false, true),
            ("F2-unknown", false, false, false, false, false),
            ("F2-live", true, true, false, false, false),
            ("F2-physical", true, false, true, false, false),
            ("F2-child", true, false, false, true, false),
        ] {
            let durable = sqlite_session_repository("affinity witness repository").await;
            let repository = Arc::new(FaultingSessionRepository::new(durable));
            let id = format!("no-create-affinity-{incumbent_owner}-{rule}");
            let (app, runtime, _) = seed_expired_cleanup_affinity(
                repository.clone(),
                &id,
                witness,
                live,
                physical,
                child,
                incumbent_owner,
            )
            .await;
            let before = repository.get(&SessionId::from(&id)).await.unwrap();
            let effects_before = runtime.effective_ids.lock().unwrap().clone();
            let assignment = app
                .claim_next_terminal_cleanup(target(incumbent_owner), None)
                .await
                .unwrap();
            assert_eq!(assignment.is_some(), allowed, "{incumbent_owner}/{rule}");
            let after = repository.get(&SessionId::from(&id)).await.unwrap();
            if let Some(assignment) = assignment {
                assert!(
                    assignment.environment_creation_not_attempted,
                    "F1 exact witness"
                );
                assert_eq!(assignment.authority.lease.owner, "new-worker", "F1");
                assert_eq!(
                    assignment.authority.lease.runtime_incarnation,
                    target(incumbent_owner).runtime_incarnation,
                    "F1"
                );
                assert_eq!(assignment.authority.lease.epoch, 5, "F1");
                assert_eq!(after.revision.0, before.revision.0 + 1, "F1 one CAS");
            } else {
                assert_eq!(after, before, "{incumbent_owner}/{rule} no Root mutation");
            }
            assert_eq!(
                *runtime.effective_ids.lock().unwrap(),
                effects_before,
                "{incumbent_owner}/{rule} no Runtime effect during claim"
            );
        }
    }
}

#[tokio::test]
async fn no_create_affinity_claim_preserves_unavailable_authority_and_lost_cas_replay() {
    // Cause table crosses same/foreign logical owner with a new Runtime and
    // the original receipt/CAS/Registry fault boundaries, not a second loop.
    // | Row | fault | effect |
    // | F3 | receipt read unavailable | error, exact Root unchanged |
    // | F4 | original CAS committed, response lost | epoch5, one CAS, exact replay |
    // | F5 | Registry revoked before CAS | pending, exact Root unchanged |
    // | F6 | Registry revoked after CAS | pending, exact prior lease compensated |
    // Every row retains terminal targets and causes zero Runtime effects.
    for incumbent_owner in ["old-worker", "new-worker"] {
        for fault in ["receipt", "response-loss", "revoke-before", "revoke-after"] {
            let durable = sqlite_session_repository("affinity fault repository").await;
            let repository = Arc::new(FaultingSessionRepository::new(durable));
            let id = format!("no-create-affinity-{incumbent_owner}-{fault}");
            let (app, runtime, workers) = seed_expired_cleanup_affinity(
                repository.clone(),
                &id,
                true,
                false,
                false,
                false,
                incumbent_owner,
            )
            .await;
            let before = repository.get(&SessionId::from(&id)).await.unwrap();
            let effects_before = runtime.effective_ids.lock().unwrap().clone();
            let registered =
                || terminal_cleanup_worker(&target(incumbent_owner), terminal_cleanup_manifest());
            match fault {
                "receipt" => repository
                    .fail_idempotency_receipt_once
                    .store(true, Ordering::SeqCst),
                "response-loss" => {
                    repository.commit_then_conflict_once("claim-terminal-cleanup-recovery")
                }
                "revoke-before" => workers.script([
                    WorkerObservationStep::Workers(vec![registered()]),
                    WorkerObservationStep::Workers(Vec::new()),
                ]),
                "revoke-after" => workers.script([
                    WorkerObservationStep::Workers(vec![registered()]),
                    WorkerObservationStep::Workers(vec![registered()]),
                    WorkerObservationStep::Workers(Vec::new()),
                ]),
                _ => unreachable!(),
            }
            let result = app
                .claim_next_terminal_cleanup(target(incumbent_owner), None)
                .await;
            let after = repository.get(&SessionId::from(&id)).await.unwrap();
            match fault {
                "receipt" => {
                    assert!(matches!(result, Err(awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(_))), "F3");
                    assert_eq!(after, before, "F3 authority failure cannot alter affinity");
                }
                "response-loss" => {
                    let assignment = result.unwrap().expect("F4 replay assignment");
                    assert!(
                        assignment.environment_creation_not_attempted,
                        "F4 retained no-Create witness"
                    );
                    assert_eq!(
                        assignment.authority.lease.runtime_incarnation,
                        target(incumbent_owner).runtime_incarnation,
                        "F4 exact replay"
                    );
                    assert_eq!(assignment.authority.lease.epoch, 5, "F4");
                    assert_eq!(
                        after.revision.0,
                        before.revision.0 + 1,
                        "F4 no duplicate CAS"
                    );
                }
                "revoke-before" => {
                    assert!(result.unwrap().is_none(), "F5 revoked before CAS");
                    assert_eq!(after, before, "F5 zero root CAS");
                    assert_eq!(workers.list_calls.load(Ordering::SeqCst), 2, "F5");
                }
                "revoke-after" => {
                    assert!(result.unwrap().is_none(), "F6 revoked after CAS");
                    assert_eq!(
                        after.realization, before.realization,
                        "F6 exact predecessor restored"
                    );
                    assert_eq!(
                        after.terminal_cleanup, before.terminal_cleanup,
                        "F6 original targets retained"
                    );
                    assert_eq!(
                        after.revision.0,
                        before.revision.0 + 2,
                        "F6 one claim and exact compensation"
                    );
                    assert_eq!(workers.list_calls.load(Ordering::SeqCst), 3, "F6");
                }
                _ => unreachable!(),
            }
            assert_eq!(
                *runtime.effective_ids.lock().unwrap(),
                effects_before,
                "{incumbent_owner}/{fault} zero Runtime effects"
            );
        }
    }
}
