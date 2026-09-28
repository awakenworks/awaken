// @formal evidence session_resources.external_io_requires_durable_prepared_state
// @formal evidence session_resources.failed_replacement_preserves_active_manifest
// @formal evidence session_resources.restart_retries_same_pending_revision
// @formal evidence session_resources.terminal_session_never_reactivates
use super::*;
use awaken_session_realization_contract::{
    ActivateSessionRealization, BeginSessionRealization, FrozenResourceTransitionUse,
    SessionRealizationControl, SessionRealizationTarget, SessionResourceMaterializationControl,
};
use awaken_session_resource_contract::{SessionRealizationLease, SessionResourceManifest};
use awaken_session_state_contract::SessionResourceState;
use awaken_session_types::SessionId;

/// Cause/effect design: C1 retained claim pins A; C2 replacement B is pending;
/// C3 Session Running/Idle (Awaiting boundary); C4 same/successor Worker; C5
/// claim instead pins desired B; C6 wrong workspace/revision/content.
/// R1 C1+!C2 => exact A; R2 C1+C2+C3+C4 => exact A, B untouched through
/// Begin/Activate; R3 C5+C2 => stage B; R4 C6 => reject before any root mutation.
/// This extends the SQLite/public-application reproduction, not a second
/// selector or a relaxed Worker predicate. It does NOT prove Worker HTTP,
/// physical IO, or model execution; those composition gates remain separate.
#[tokio::test]
async fn retained_claim_selects_active_resources_without_consuming_pending_generation() {
    for status in ["idle", "running"] {
        for replace in [false, true] {
            for successor in [false, true] {
                let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
                    awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
                );
                let app = application_with_configuration(
                    repo.clone(),
                    Arc::new(RecordingEnvironmentSource::default()),
                    SessionApplicationConfiguration {
                        execution_placement: SessionExecutionPlacement::RegisteredWorker,
                        ..Default::default()
                    },
                );
                let id = "retained-resource-run";
                let mut session = persisted(id, false, status);
                let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
                    &mut session.baseline
                else {
                    unreachable!()
                };
                baseline.runtime_placement = SessionRuntimePlacement::Worker;
                let active = skill_resources("pinned-skill");
                let mut desired =
                    awaken_session_resource_contract::ResolvedSessionResources::try_new(
                        file_resources("context-file").inputs().to_vec(),
                        active.skills().to_vec(),
                    )
                    .unwrap();
                session.resources = SessionResourceState::from_active(active.clone());
                session.realization = Some(SessionRealizationLease {
                    owner: "worker-a".into(),
                    runtime_incarnation: "worker-a/boot-1".into(),
                    epoch: 1,
                    expires_at_unix_ms: u64::MAX - 1,
                });
                create(repo.as_ref(), session).await;
                let carried = SessionResourceManifest::at_revision("workspace", 1, active.clone());
                if replace {
                    let replaced = app
                        .replace_session_resource_manifest(
                            &SessionId::from(id),
                            ReplaceSessionResourceManifest {
                                resources: desired.clone(),
                                expected_resource_revision: Some(1),
                                idempotency_key: Some("replace-context".into()),
                                request_fingerprint: "context-inputs".into(),
                            },
                        )
                        .await
                        .unwrap();
                    // Publication owns lifecycle timestamps too. The carried
                    // snapshot is the persisted result, not the pre-command DTO.
                    desired = replaced.session.resources.pending.unwrap();
                }
                let target = SessionRealizationTarget {
                    owner: if successor { "worker-b" } else { "worker-a" }.into(),
                    runtime_incarnation: if successor {
                        "worker-b/boot-1"
                    } else {
                        "worker-a/boot-1"
                    }
                    .into(),
                    lease_expires_at_unix_ms: u64::MAX,
                    reassign_existing_lease: true,
                };
                let begin = BeginSessionRealization {
                    session_id: id.into(),
                    target: target.clone(),
                    expected_resources: Some(carried.clone()),
                };
                // All identity axes independently reject before Begin mutates
                // the realization lease or increments pending activation attempts.
                for wrong in [
                    SessionResourceManifest::at_revision("foreign", 1, active.clone()),
                    SessionResourceManifest::at_revision("workspace", 99, active.clone()),
                    SessionResourceManifest::at_revision("workspace", 1, desired.clone()),
                ] {
                    let before = repo.get(&SessionId::from(id)).await.unwrap();
                    assert!(
                        app.begin_session_realization(BeginSessionRealization {
                            expected_resources: Some(wrong),
                            ..begin.clone()
                        })
                        .await
                        .is_err(),
                        "R4"
                    );
                    assert_eq!(
                        repo.get(&SessionId::from(id)).await.unwrap(),
                        before,
                        "R4 no mutation"
                    );
                }
                let directive = app.begin_session_realization(begin.clone()).await.unwrap();
                assert_eq!(
                    directive
                        .projection
                        .resource_transition(FrozenResourceTransitionUse::ApplyEffects)
                        .unwrap()
                        .desired(),
                    &carried,
                    "R1/R2"
                );
                assert_eq!(
                    // Ordinary observation replays the assigned generation.
                    // A second explicit reassignment is a new claim-authorized
                    // assignment, not an observation of the previous lease.
                    app.begin_session_realization(BeginSessionRealization {
                        target: SessionRealizationTarget {
                            reassign_existing_lease: false,
                            ..begin.target.clone()
                        },
                        ..begin
                    })
                    .await
                    .unwrap()
                    .projection,
                    directive.projection,
                    "exact Begin replay"
                );
                let intent =
                    awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
                        id,
                        1,
                        active.skills()[0].clone(),
                        directive.authority.lease.clone(),
                    )
                    .unwrap();
                assert_eq!(
                    app.authorize_resource_materialization_intent(&intent)
                        .await
                        .unwrap(),
                    "workspace",
                    "R2 cold Worker may read exact A while B remains pending"
                );
                let activated = app
                    .activate_session_realization(ActivateSessionRealization {
                        session_id: id.into(),
                        lease: directive.authority.lease,
                        prepared_resource_revision: Some(1),
                        mcp_receipts: Vec::new(),
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    activated
                        .projection
                        .resource_transition(FrozenResourceTransitionUse::ApplyEffects)
                        .unwrap()
                        .desired(),
                    &carried,
                    "R2 activation preserves A"
                );
                let current = repo.get(&SessionId::from(id)).await.unwrap();
                assert_eq!(current.resources.active, active);
                assert_eq!(
                    current.resources.pending,
                    replace.then_some(desired.clone())
                );
                if replace {
                    let next = app
                        .begin_session_realization(BeginSessionRealization {
                            session_id: id.into(),
                            target,
                            expected_resources: Some(SessionResourceManifest::at_revision(
                                "workspace",
                                2,
                                desired.clone(),
                            )),
                        })
                        .await
                        .unwrap();
                    let selected = next
                        .projection
                        .resource_transition(FrozenResourceTransitionUse::ApplyEffects)
                        .unwrap();
                    assert_eq!(selected.desired().revision, 2, "R3");
                    assert_eq!(selected.desired().resources, desired, "R3");
                    let settled = app
                        .activate_session_realization(ActivateSessionRealization {
                            session_id: id.into(),
                            lease: next.authority.lease.clone(),
                            prepared_resource_revision: Some(2),
                            mcp_receipts: Vec::new(),
                        })
                        .await
                        .unwrap();
                    assert_eq!(
                        settled.projection.resource_revision, 2,
                        "R3 commits B only after its Stage"
                    );
                    assert!(
                        repo.get(&SessionId::from(id))
                            .await
                            .unwrap()
                            .resources
                            .pending
                            .is_none()
                    );
                    let before = repo.get(&SessionId::from(id)).await.unwrap();
                    assert!(
                        app.authorize_resource_materialization_intent(&intent)
                            .await
                            .is_err(),
                        "retired A cannot be read"
                    );
                    assert!(
                        app.activate_session_realization(ActivateSessionRealization {
                            session_id: id.into(),
                            lease: next.authority.lease,
                            prepared_resource_revision: Some(1),
                            mcp_receipts: Vec::new(),
                        })
                        .await
                        .is_err(),
                        "retired Stage A cannot roll back committed B"
                    );
                    assert_eq!(
                        repo.get(&SessionId::from(id)).await.unwrap(),
                        before,
                        "late A has no mutation"
                    );
                }
            }
        }
    }
}

/// Causes: C1 pending/rolled-back/committed replacement, C2 asserted revision
/// 1/2/unknown 3, C3 target belongs to A/B, C4 Input/Skill, C5 forged target.
/// Decision table (all C4): pending permits (1,A) and (2,B); rollback only
/// (1,A), commit only (2,B); every cross-pair/unknown/forged target rejects.
/// Effects: success returns root Workspace; failure is Invalid; neither writes
/// the root. Existing lease/terminal tests supply the orthogonal deny overrides.
#[tokio::test]
async fn materialization_authorizes_retained_pairs_not_revision_watermarks() {
    use awaken_session_realization_contract::SessionResourceMaterializationIntent;
    for lifecycle in ["pending", "rollback", "committed"] {
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = "materialization-pairs";
        let resources = |name| {
            awaken_session_resource_contract::ResolvedSessionResources::try_new(
                file_resources(name).inputs().to_vec(),
                skill_resources(name).skills().to_vec(),
            )
            .unwrap()
        };
        let active = resources("active-a");
        let desired = resources("pending-b");
        let lease = SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let mut session = persisted(id, false, "idle");
        session.realization = Some(lease.clone());
        session.resources = SessionResourceState::from_active(active.clone());
        session
            .resources
            .prepare(&awaken_session_types::SessionId::from(id), desired.clone())
            .unwrap();
        match lifecycle {
            "rollback" => session.resources.rollback("failed replacement").unwrap(),
            "committed" => session.resources.commit().unwrap(),
            _ => {}
        }
        create(repo.as_ref(), session).await;
        let before = repo.get(&SessionId::from(id)).await.unwrap();
        for revision in [1, 2, 3] {
            for (source, bindings) in [(1, &active), (2, &desired)] {
                for forged in [false, true] {
                    let mut input = bindings.inputs()[0].clone();
                    let mut skill = bindings.skills()[0].clone();
                    if forged {
                        input.mount_path = "/foreign".into();
                        skill.version += 1;
                    }
                    for intent in [
                        SessionResourceMaterializationIntent::try_new(
                            id,
                            revision,
                            input,
                            lease.clone(),
                        )
                        .unwrap(),
                        SessionResourceMaterializationIntent::try_new_skill(
                            id,
                            revision,
                            skill,
                            lease.clone(),
                        )
                        .unwrap(),
                    ] {
                        let allowed = !forged
                            && revision == source
                            && match lifecycle {
                                "rollback" => source == 1,
                                "committed" => source == 2,
                                _ => true,
                            };
                        let result = app.authorize_resource_materialization_intent(&intent).await;
                        if allowed {
                            assert_eq!(
                                result.unwrap(),
                                "workspace",
                                "{lifecycle}/{revision}/{source}"
                            );
                        } else {
                            assert!(matches!(result, Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))), "{lifecycle}/{revision}/{source}/{forged}");
                        }
                        assert_eq!(
                            repo.get(&SessionId::from(id)).await.unwrap(),
                            before,
                            "authorization never mutates"
                        );
                    }
                }
            }
        }
    }
}

/// Failure decision table: exact active A => record the failed recovery, retain
/// B with no B activation error; exact pending B => mark B's activation error;
/// unknown revision => Invalid, no root write. Retryable Running failures keep
/// the Run resumable and expire only its realization lease for fenced recovery.
#[tokio::test]
async fn failed_retained_stage_does_not_attribute_its_error_to_pending_resources() {
    for revision in [1, 2, 99] {
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = "failed-retained-stage";
        let lease = SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let mut session = persisted(id, false, "running");
        session.realization = Some(lease.clone());
        session.resources = SessionResourceState::from_active(file_resources("a"));
        session
            .resources
            .prepare(
                &awaken_session_types::SessionId::from(id),
                file_resources("b"),
            )
            .unwrap();
        session.resources.start_attempt().unwrap();
        create(repo.as_ref(), session).await;
        let before = repo.get(&SessionId::from(id)).await.unwrap();
        let result = app
            .fail_session_realization(
                awaken_session_realization_contract::FailSessionRealization {
                    session_id: id.into(),
                    lease,
                    prepared_resource_revision: Some(revision),
                    retryable: true,
                    source_run_id: None,
                    reason: "read unavailable".into(),
                },
            )
            .await;
        let after = repo.get(&SessionId::from(id)).await.unwrap();
        if revision == 99 {
            assert!(matches!(
                result,
                Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                        _
                    )
                )
            ));
            assert_eq!(after, before);
        } else {
            result.unwrap();
            assert_eq!(
                after.resources.active_generation(),
                before.resources.active_generation()
            );
            assert_eq!(after.resources.pending, before.resources.pending);
            assert_eq!(after.execution, before.execution);
            assert_eq!(after.realization.unwrap().expires_at_unix_ms, 0);
            assert_eq!(
                after.resources.activations[0].last_error.as_deref(),
                (revision == 2).then_some("read unavailable")
            );
        }
    }
}

#[tokio::test]
async fn restart_retries_the_exact_persisted_pending_resource_revision() {
    // Cause/effect graph: C1 generation P is pending after one failed attempt;
    // C2 the SQLite authority and application process are closed; C3 a fresh
    // application scans durable reconciliation work; C4 the Runtime retry
    // succeeds. Effects: E1 reopen returns P and attempt count 1; E2 retry
    // increments only P's existing activation; E3 P becomes active without a
    // new Resource generation; E4 pending/error state clears.
    //
    // | Rule | pending | restart | retry | Durable outcome | Effect |
    // |---|---|---|---|---|---|
    // | R1 | P, attempt 1 | yes | not run | P pending | E1 |
    // | R2 | P, attempt 1 | yes | success | P active, attempt 2 | E2-E4 |
    // The no-pending rejection is owned by the adjacent contract tests; this
    // test owns the real file close/reopen and application recovery boundary.
    let directory = tempfile::tempdir().expect("temporary Session authority");
    let path = directory.path().join("resource-restart.db");
    let path = path.to_string_lossy().to_string();
    let session_id = "resource-restart-same-revision";
    let pending = file_resources("restart-file");
    let pending_revision;

    {
        let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("open Session authority"),
        );
        let mut session = persisted(session_id, false, "idle");
        pending_revision = session
            .resources
            .prepare(
                &awaken_session_types::SessionId::from(session_id),
                pending.clone(),
            )
            .unwrap();
        session.resources.start_attempt().unwrap();
        session
            .resources
            .note_retryable_failure("process stopped before settlement")
            .unwrap();
        create(repository.as_ref(), session).await;
        let before_restart = repository.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(before_restart.resources.revision, pending_revision, "R1/E1");
        assert_eq!(
            before_restart.resources.pending,
            Some(pending.clone()),
            "R1/E1"
        );
        assert_eq!(before_restart.resources.activations[0].attempts, 1, "R1/E1");
    }

    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&path)
            .expect("reopen Session authority"),
    );
    let reopened = repository.get(&SessionId::from(session_id)).await.unwrap();
    assert_eq!(reopened.resources.revision, pending_revision, "R1/E1");
    assert_eq!(reopened.resources.pending, Some(pending.clone()), "R1/E1");
    assert_eq!(reopened.resources.activations[0].attempts, 1, "R1/E1");

    let application = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let report = application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "R2/E2-E4: {report:?}");
    assert_eq!(report.settled.len(), 1, "R2/E2-E4");

    let settled = repository.get(&SessionId::from(session_id)).await.unwrap();
    assert_eq!(settled.resources.revision, pending_revision, "R2/E3");
    assert_eq!(
        settled.resources.active_generation(),
        (pending_revision, &pending),
        "R2/E3"
    );
    assert!(settled.resources.pending.is_none(), "R2/E4");
    assert_eq!(settled.resources.activations.len(), 1, "R2/E2");
    assert_eq!(
        settled.resources.activations[0].revision, pending_revision,
        "R2/E2"
    );
    assert_eq!(settled.resources.activations[0].attempts, 2, "R2/E2");
    assert_eq!(
        settled.resources.activations[0].state,
        awaken_session_state_contract::ActivationState::Active,
        "R2/E3"
    );
    assert!(
        settled.resources.activations[0].last_error.is_none(),
        "R2/E4"
    );
    assert!(!settled.needs_resource_reconciliation(), "R2/E4");
}

#[test]
fn terminal_resource_recovery_releases_without_ordinary_runtime_apply() {
    run_composed_async_test(
        terminal_resource_recovery_releases_without_ordinary_runtime_apply_case,
    );
}

async fn terminal_resource_recovery_releases_without_ordinary_runtime_apply_case() {
    // Cause/effect graph: C1 the Session root is terminal; C2 active generation
    // A remains; C3 replacement B is pending and has already been attempted;
    // C4 recovery scans the root. Effects: E1 ordinary Runtime apply count stays
    // zero; E2 terminal cleanup clears A; E3 B becomes Failed rather than
    // Active; E4 no Resource manifest or retention edge survives. Released A
    // diagnostics may be compacted because this aggregate is not an audit log.
    //
    // | Rule | terminal | active | pending | Recovery outcome | Effect |
    // |---|---|---|---|---|---|
    // | R1 | no | A | B | ordinary reconciliation | covered by restart test |
    // | R2 | yes | A | B | terminal release only | E1-E4 |
    // The lifecycle classifier and terminal-release record transitions are
    // covered independently by their contract decision tables; this test owns
    // the application-to-Runtime negative effect boundary.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session authority"),
    );
    let session_id = "terminal-resource-never-reactivates";
    let mut session = persisted(session_id, false, "terminated");
    session.resources = SessionResourceState::from_active(file_resources("active"));
    session
        .resources
        .prepare(
            &awaken_session_types::SessionId::from(session_id),
            file_resources("never-active"),
        )
        .unwrap();
    session.resources.start_attempt().unwrap();
    create(repository.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let report = application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "R2/E2-E4: {report:?}");
    assert_eq!(report.settled.len(), 1, "R2/E2-E4");
    assert_eq!(
        runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
        0,
        "R2/E1"
    );

    let terminal = repository.get(&SessionId::from(session_id)).await.unwrap();
    assert!(terminal.resources.active.inputs().is_empty(), "R2/E2-E4");
    assert!(terminal.resources.pending.is_none(), "R2/E3-E4");
    assert!(!terminal.resources.has_references(), "R2/E4");
    assert!(
        terminal.resources.activations.iter().any(|activation| {
            activation.resource_id
                == awaken_resource_contract::InputResourceId::File(
                    awaken_resource_contract::FileId::from("never-active"),
                )
                && activation.state == awaken_session_state_contract::ActivationState::Failed
        }),
        "R2/E3"
    );
}
