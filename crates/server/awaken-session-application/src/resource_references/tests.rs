//! Original Resource reference-projection case at the private bracket owner.
use crate::SessionMutationError;
use crate::tests::application_fixture::application;
use crate::tests::realization::sqlite_session_repository;
use crate::tests::{
    FaultingSessionRepository, RecordingEnvironmentSource, RecordingReferenceIndex,
    UnusedFileCatalog, create, persisted, skill_resources,
};
use awaken_session_repository_contract::{SessionRepository as _, SessionRepositoryReads as _};
use awaken_session_types::SessionId;
use std::collections::BTreeSet;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

/// The application's own Resource commit bracket.
fn bracket(app: &crate::SessionApplication) -> &super::SessionResourceReferences {
    app.resource_manifests.resource_references()
}

/// Reference-projection FMECA and cause/effect decision table. C1 the
/// Resource index accepts or rejects a replacement; C2 the Session root CAS
/// wins or conflicts after projection. Effects are E1 no durable intent is
/// exposed without its safety edge, E2 a losing candidate is repaired back
/// to repository truth, and E3 a winning pending replacement retains both
/// installed and desired targets until settlement.
///
/// | Rule | C1 index | C2 root CAS | Effect |
/// |---|---|---|---|
/// | R1 | rejects | not attempted | E1 unchanged Session |
/// | R2 | accepts | conflicts | E2 installed target only |
/// | R3 | accepts | wins | E3 installed + desired targets |
///
/// R4-R8 extend the same rule across automatic and explicit records: rejected
/// references prevent CAS; a real CAS conflict repairs from root truth; failed
/// repair preserves that first Conflict and remains recoverable; Applied and
/// Replayed keep exact root/receipt identity. Replay advances only a legal title
/// update with unchanged Resource targets, not the separately unproved target
/// drift partition. The existing SQLite, index and pause fixture own all IO.
///
/// | Rule | Record | Index | CAS/result | Repair | Required effect |
/// |---|---|---|---|---|---|
/// | R4 | auto/explicit | rejects | not entered | none | unchanged root |
/// | R5 | auto/explicit | accepts | Conflict | succeeds | authoritative refs |
/// | R6 | auto/explicit | accepts | Conflict | fails, then retried | first Conflict, later repair |
/// | R7 | auto/explicit | accepts | Applied | none | candidate refs + one root |
/// | R8 | auto/explicit | accepts | Replayed after title update | none | original receipt, newer root |
#[tokio::test]
async fn resource_reference_projection_fails_closed_and_repairs_cas_losers() {
    let repo = sqlite_session_repository("session repository").await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let mut application = application(repo.clone(), environments);
    let references = Arc::new(RecordingReferenceIndex::default());
    application.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));

    let mut original = persisted("reference-fence", false, "idle");
    original.resources =
        awaken_session_state_contract::SessionResourceState::from_active(skill_resources("old"));
    create(repo.as_ref(), original).await;

    let mut rejected = repo.get(&SessionId::from("reference-fence")).await.unwrap();
    let original_revision = rejected.revision;
    rejected
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("reference-fence"),
            skill_resources("new"),
        )
        .unwrap();
    references.fail_replace.store(true, Ordering::SeqCst);
    assert!(
        bracket(&application)
            .commit(
                "workspace",
                awaken_session_state_contract::SessionAggregate::try_from(rejected)
                    .expect("valid test snapshot"),
                "R1",
                Vec::new()
            )
            .await
            .is_err(),
        "R1"
    );
    assert_eq!(
        repo.get(&SessionId::from("reference-fence"))
            .await
            .unwrap()
            .revision,
        original_revision,
        "R1/E1"
    );

    references.fail_replace.store(false, Ordering::SeqCst);
    let mut stale = repo.get(&SessionId::from("reference-fence")).await.unwrap();
    let mut winner = stale.clone();
    winner.title = Some("concurrent winner".into());
    let _fixture = application
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            // R2 needs a legal authoring winner before the stale Resource CAS.
            awaken_store_conformance::session_store::record_update(&stale, winner, 0),
            "concurrent",
            Vec::new(),
        )
        .await
        .unwrap();
    stale
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("reference-fence"),
            skill_resources("new"),
        )
        .unwrap();
    assert!(matches!(
        bracket(&application)
            .commit(
                "workspace",
                awaken_session_state_contract::SessionAggregate::try_from(stale)
                    .expect("valid test snapshot"),
                "R2",
                Vec::new()
            )
            .await,
        Err(SessionMutationError::Conflict)
    ));
    let ids = || {
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(ids(), BTreeSet::from(["old".to_string()]), "R2/E2");

    let mut accepted = repo.get(&SessionId::from("reference-fence")).await.unwrap();
    accepted
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("reference-fence"),
            skill_resources("new"),
        )
        .unwrap();
    bracket(&application)
        .commit(
            "workspace",
            awaken_session_state_contract::SessionAggregate::try_from(accepted)
                .expect("valid test snapshot"),
            "R3",
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        ids(),
        BTreeSet::from(["new".to_string(), "old".to_string()]),
        "R3/E3"
    );
    for explicit in [false, true] {
        reference_bracket_rejection_rules(explicit).await;
        reference_bracket_success_rules(explicit).await;
    }
}

async fn reference_bracket_rejection_rules(explicit: bool) {
    for rule in ["R4", "R5", "R6"] {
        let durable = sqlite_session_repository("reference rejection repository").await;
        let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
        let mut app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let references = Arc::new(RecordingReferenceIndex::default());
        app.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));
        let app = Arc::new(app);
        let id = SessionId::from(format!("reference-{rule}-{explicit}"));
        let mut original = persisted(id.as_str(), false, "idle");
        original.resources = awaken_session_state_contract::SessionResourceState::from_active(
            skill_resources("old"),
        );
        create(durable.as_ref(), original).await;
        let before = durable.get(&id).await.unwrap();
        let mut candidate = app.mutations.session_aggregate(&id).await.unwrap();
        candidate
            .resources()
            .prepare(skill_resources("new"))
            .unwrap();
        let operation = format!("reference-bracket-{rule}");
        let record = crate::mutation::compile_session_snapshot_idempotency(&candidate, &operation);
        let pause = repo.pause_next_mutation(&operation);
        references
            .fail_replace
            .store(rule == "R4", Ordering::SeqCst);
        let subject = app.clone();
        let attempt = tokio::spawn(async move {
            if explicit {
                bracket(&subject)
                    .commit_with_record("workspace", candidate, record, Vec::new())
                    .await
                    .map(crate::SnapshotCommit::into_session)
            } else {
                bracket(&subject)
                    .commit("workspace", candidate, &operation, Vec::new())
                    .await
                    .map(crate::SessionAggregate::into_snapshot)
            }
        });
        if rule == "R4" {
            let result = tokio::time::timeout(Duration::from_secs(5), attempt)
                .await
                .expect("R4 reference rejection cannot enter held CAS")
                .expect("R4 command did not panic");
            assert!(
                matches!(result, Err(SessionMutationError::Unavailable(_))),
                "R4/{explicit}"
            );
            assert!(
                pause.candidate.lock().unwrap().is_none(),
                "R4 no CAS/{explicit}"
            );
            assert_eq!(
                durable.get(&id).await.unwrap(),
                before,
                "R4 unchanged/{explicit}"
            );
            continue;
        }
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
            .await
            .expect("R5/R6 reached original root CAS after reference publication");
        let captured = pause.candidate.lock().unwrap().clone().unwrap();
        assert_eq!(
            captured.expected_revision, before.revision,
            "R5/R6 origin/{explicit}"
        );
        let mut winner = before.clone();
        winner.title = Some("legal concurrent title".into());
        let winner = app
            .mutations
            .commit_session_snapshot_with_outcome(
                "workspace",
                awaken_store_conformance::session_store::record_update(&before, winner, 0),
                "reference-title-winner",
                Vec::new(),
            )
            .await
            .unwrap()
            .into_session();
        references
            .fail_replace
            .store(rule == "R6", Ordering::SeqCst);
        pause.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(5), attempt)
            .await
            .expect("R5/R6 conflict and repair terminate inline")
            .expect("R5/R6 command did not panic");
        assert!(
            matches!(result, Err(SessionMutationError::Conflict)),
            "{rule} first error/{explicit}"
        );
        assert!(
            matches!(
                *pause.result.lock().unwrap(),
                Some(awaken_session_state_contract::SessionMutationResult::Conflict { .. })
            ),
            "{rule} actual CAS conflict/{explicit}"
        );
        assert_eq!(
            durable.get(&id).await.unwrap(),
            winner,
            "{rule} winner retained/{explicit}"
        );
        let ids = || {
            references
                .records
                .lock()
                .unwrap()
                .iter()
                .map(|record| record.target.resource_id.clone())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            ids(),
            if rule == "R6" {
                BTreeSet::from(["new".to_string(), "old".to_string()])
            } else {
                BTreeSet::from(["old".to_string()])
            },
            "{rule} repair outcome/{explicit}"
        );
        references.fail_replace.store(false, Ordering::SeqCst);
        bracket(&app).repair("workspace", &id).await;
        assert_eq!(
            ids(),
            BTreeSet::from(["old".to_string()]),
            "{rule} later repair/{explicit}"
        );
        assert_eq!(
            durable.get(&id).await.unwrap(),
            winner,
            "{rule} repair never writes root/{explicit}"
        );
    }
}

async fn reference_bracket_success_rules(explicit: bool) {
    let durable = sqlite_session_repository("reference receipt repository").await;
    let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let mut app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let references = Arc::new(RecordingReferenceIndex::default());
    app.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));
    let id = SessionId::from(format!("reference-receipt-{explicit}"));
    let mut original = persisted(id.as_str(), false, "idle");
    original.resources =
        awaken_session_state_contract::SessionResourceState::from_active(skill_resources("old"));
    create(durable.as_ref(), original).await;
    let mut candidate = app.mutations.session_aggregate(&id).await.unwrap();
    candidate
        .resources()
        .prepare(skill_resources("new"))
        .unwrap();
    let operation = "reference-receipt-command";
    let record = crate::mutation::compile_session_snapshot_idempotency(&candidate, operation);
    let first = if explicit {
        let commit = bracket(&app)
            .commit_with_record("workspace", candidate.clone(), record.clone(), Vec::new())
            .await
            .unwrap();
        assert!(commit.transitioned(), "R7 explicit Applied");
        assert_eq!(
            commit.committed_revision(),
            commit.session().revision,
            "R7 receipt"
        );
        commit.into_session()
    } else {
        bracket(&app)
            .commit("workspace", candidate.clone(), operation, Vec::new())
            .await
            .unwrap()
            .into_snapshot()
    };
    let receipt = durable
        .idempotency_receipt(&id, &record.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.committed_revision, first.revision, "R7/{explicit}");
    assert_eq!(
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["new".to_string(), "old".to_string()]),
        "R7 retained targets/{explicit}"
    );
    let target_replay_candidate = candidate.clone();
    let target_replay_receipt = receipt.clone();
    // R8 only changes legal authoring metadata after the original command.
    // Resource targets remain identical, so this is no oracle for a replay
    // against a different active/pending Resource generation.
    let mut newer = first.clone();
    newer.title = Some("later legal title".into());
    let newer = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            awaken_store_conformance::session_store::record_update(&first, newer, 0),
            "reference-receipt-later-title",
            Vec::new(),
        )
        .await
        .unwrap()
        .into_session();
    assert!(
        newer.revision > receipt.committed_revision,
        "R8 newer reload/{explicit}"
    );
    let replay = if explicit {
        let commit = bracket(&app)
            .commit_with_record("workspace", candidate, record.clone(), Vec::new())
            .await
            .unwrap();
        assert!(!commit.transitioned(), "R8 actual Replayed");
        assert_eq!(
            commit.committed_revision(),
            receipt.committed_revision,
            "R8 original receipt"
        );
        assert_eq!(
            commit.session().revision,
            newer.revision,
            "R8 newer returned root"
        );
        commit.into_session()
    } else {
        bracket(&app)
            .commit("workspace", candidate, operation, Vec::new())
            .await
            .unwrap()
            .into_snapshot()
    };
    assert_eq!(replay, newer, "R8 exact newer root/{explicit}");
    assert_eq!(
        durable.idempotency_receipt(&id, &record.key).await.unwrap(),
        Some(receipt),
        "R8 original receipt retained/{explicit}"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        newer,
        "R8 replay no new root/{explicit}"
    );
    assert_eq!(
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["new".to_string(), "old".to_string()]),
        "R8 unchanged targets/{explicit}"
    );

    // R10-targets extends this SAME original R7/R8 fixture and retained legal
    // candidate; it is separate from the public early-receipt fault row also
    // named R10 in lost_root_cas_replay.rs. No fabricated snapshot/read exists.
    // C1 pending has not been attempted (no activation lease); C2 the canonical
    // aggregate law amends its targets at the SAME Resource generation; C3 a
    // later root CAS publishes those targets; C4 the original exact record
    // really returns Replayed. E1 return the newer root/original receipt; E2
    // references must still match the newer root, never the stale candidate.
    // | Record | Pending law | Root CAS | Delayed CAS | Reference effect |
    // | auto/explicit | revise_unattempted_pending | newer revision | Replayed | old + later |
    // This observes the private bracket's original exact-replay contract. The
    // existing CAS pause is AFTER reference publication: it records real
    // Replayed here, not a claim that the public HTTP race was scheduled.
    // A merely title-only successor (R8) cannot distinguish E2.
    let mut later = app.mutations.session_aggregate(&id).await.unwrap();
    let resource_generation = later.resources.revision;
    later
        .resources()
        .revise_unattempted_pending(skill_resources("later"))
        .expect("R10 legal amendment before any external attempt");
    assert_eq!(
        later.resources.revision, resource_generation,
        "R10 Resource generation remains stable; root CAS revision will advance"
    );
    let later = bracket(&app)
        .commit("workspace", later, "reference-later-targets", Vec::new())
        .await
        .unwrap()
        .into_snapshot();
    assert!(later.revision > newer.revision, "R10 later root CAS");
    let expected_targets = BTreeSet::from(["later".to_string(), "old".to_string()]);
    assert_eq!(
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>(),
        expected_targets,
        "R10 precondition: later legal Resource root is already projected/{explicit}"
    );
    // Accepted-replay repair C/E continuation of the same R10 fixture:
    // C1 real Replayed + later targets (constant); C2 index refuses only AFTER
    // candidate projection; C3 fresh repair read refuses, while the first
    // canonical reload succeeds. E1 keep accepted newer root/original receipt;
    // E2 successful repair keeps current targets; E3 failed repair stays pending
    // and the original later repair restores it without a new CAS/receipt.
    // | Rule | Post-CAS fault | Actual observation | Required effects |
    // | R10 | none | one repair-index call | E1+E2 |
    // | R11 | index | counted rejected repair-index call | E1+E3 |
    // | R12 | second get | fault consumed after canonical reload; no index call | E1+E3 |
    // failed_gets cannot isolate R12: it would kill the FIRST canonical reload,
    // whose error legitimately precedes any accepted SnapshotCommit.
    let app = Arc::new(app);
    for repair_fault in ["none", "index", "fresh-read"] {
        let pause = repo.pause_next_mutation(operation);
        let subject = app.clone();
        let target_replay_candidate = target_replay_candidate.clone();
        let original_revision = target_replay_receipt.committed_revision;
        let original_record = record.clone();
        let task = tokio::spawn(async move {
            if explicit {
                bracket(&subject)
                    .commit_with_record(
                        "workspace",
                        target_replay_candidate,
                        original_record,
                        Vec::new(),
                    )
                    .await
                    .map(|commit| {
                        assert!(!commit.transitioned(), "R10-R12 accepted explicit replay");
                        assert_eq!(
                            commit.committed_revision(),
                            original_revision,
                            "R10-R12 exact accepted receipt"
                        );
                        commit.into_session()
                    })
            } else {
                bracket(&subject)
                    .commit("workspace", target_replay_candidate, operation, Vec::new())
                    .await
                    .map(crate::SessionAggregate::into_snapshot)
            }
        });
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
            .await
            .expect("R10 original replay reaches real CAS after reference projection");
        let projected_calls = references.replace_calls.load(Ordering::SeqCst);
        if repair_fault == "index" {
            references.fail_replace.store(true, Ordering::SeqCst);
        } else if repair_fault == "fresh-read" {
            repo.fail_get_after_next_reload_once();
        }
        pause.release.notify_one();
        let replay = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("R10 replay terminates inline")
            .expect("R10 replay does not panic")
            .expect("R10 accepted replay is not a new error");
        assert!(
            matches!(
                *pause.result.lock().unwrap(),
                Some(awaken_session_state_contract::SessionMutationResult::Replayed { new_revision })
                    if new_revision == target_replay_receipt.committed_revision
            ),
            "R10 actual original receipt replay/{explicit}"
        );
        assert_eq!(replay, later, "R10 returned newer root/{explicit}");
        assert_eq!(
            durable.get(&id).await.unwrap(),
            later,
            "R10 replay did not overwrite root/{explicit}"
        );
        if repair_fault == "none" {
            assert_eq!(
                references
                    .records
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|record| record.target.resource_id.clone())
                    .collect::<BTreeSet<_>>(),
                expected_targets,
                "R10 replay must not roll back the newer Resource target projection/{explicit}"
            );
        }
        if repair_fault == "fresh-read" {
            assert_eq!(
                repo.fail_get_on_call_once.load(Ordering::SeqCst),
                0,
                "R12 the second read fault really fired, after the required replay reload"
            );
            assert_eq!(
                references.replace_calls.load(Ordering::SeqCst),
                projected_calls,
                "R12 unavailable fresh root prevents a repair-index write"
            );
        } else {
            assert_eq!(
                references.replace_calls.load(Ordering::SeqCst),
                projected_calls + 1,
                "R10/R11 accepted replay really attempted the one fresh projection repair"
            );
        }
        // R11/R12 cannot turn an accepted receipt into Unavailable. The successful
        // replay/root assertions above precede these repair observations. Clear only
        // the original test fault, then reuse the existing repair owner; no root CAS,
        // receipt lookup, replacement Runtime or repair ledger is introduced.
        if repair_fault != "none" {
            // R13 positive safety effect: the failed best-effort repair leaves
            // the CURRENT later Skill absent from the disposable index. The
            // existing Guard must still find this Session from authoritative
            // SQLite root truth, rather than treating missing index as zero
            // references. This is an actual blockers read, not a physical purge
            // or a new cleanup/permission authority. Both R11/R12 reuse it.
            assert!(
                !references
                    .records
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|record| record.target.resource_id == "later"),
                "R13 precondition: failed repair has not restored the later index edge"
            );
            let guard =
                crate::SessionResourcePurgeGuard::new(durable.clone(), Arc::new(UnusedFileCatalog));
            let blockers = awaken_resource_contract::ResourcePurgeGuard::blockers(
                &guard,
                &awaken_resource_contract::ResourceTarget::new(
                    "workspace",
                    awaken_resource_contract::ResourceKind::Skill,
                    "later",
                ),
                None,
                0,
            )
            .await
            .expect("R13 original root scan refuses incomplete observation");
            assert_eq!(
                blockers,
                vec![awaken_resource_contract::ResourceReference {
                    kind: awaken_resource_contract::ResourceReferenceKind::SessionBinding,
                    reference_id: id.to_string(),
                }],
                "R13 current durable Session still blocks reclamation after repair failure"
            );
            references.fail_replace.store(false, Ordering::SeqCst);
            bracket(&app).repair("workspace", &id).await;
            assert_eq!(
                references
                    .records
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|record| record.target.resource_id.clone())
                    .collect::<BTreeSet<_>>(),
                expected_targets,
                "R11/R12 original later repair restores the current-root projection"
            );
            assert_eq!(
                durable.get(&id).await.unwrap(),
                later,
                "R11/R12 repair cannot rewrite accepted root or receipt revision"
            );
            assert_eq!(
                durable.idempotency_receipt(&id, &record.key).await.unwrap(),
                Some(target_replay_receipt.clone()),
                "R11/R12 original command receipt remains exact"
            );
        }
    }
}
