// @formal evidence session_realization.concurrent_drivers_preserve_one_owner_fence
//! Read qualification retains the original roots and decides from the
//! source retention the Worker reported for the exact subjects it was given.
use super::*;

use awaken_session_realization_contract::{SourceRetentionSubject, WorkerSourceRetentionSubjects};
use awaken_worker_contract::WorkerSourceRetention;
use std::collections::BTreeSet;

/// The Worker's report of `subjects`: one fact per subject, `retained` as given.
fn report(subjects: &[SourceRetentionSubject], retained: bool) -> BTreeSet<WorkerSourceRetention> {
    subjects
        .iter()
        .map(|subject| WorkerSourceRetention {
            session_id: subject.session_id.to_string(),
            subject: subject.digest.clone(),
            retained,
        })
        .collect()
}

/// Read the subjects the Worker is asked to observe, then qualify retirement
/// from `reported`, built from those subjects.
async fn qualify_retirement(
    app: &SessionApplication,
    identity: &awaken_worker_contract::WorkerIdentity,
    reported: impl FnOnce(&[SourceRetentionSubject]) -> BTreeSet<WorkerSourceRetention>,
) -> (
    Result<crate::WorkerRealizationObservation, RunError>,
    Vec<SourceRetentionSubject>,
) {
    let occupancy = app.worker_occupancy();
    let subjects = occupancy.source_retention_subjects(identity).await.unwrap();
    let reported = reported(&subjects);
    let observed = occupancy
        .observe_worker_realizations(crate::WorkerRealizationRead::Retirement {
            identity,
            reported: &reported,
        })
        .await;
    (observed, subjects)
}

#[tokio::test]
async fn ordinary_worker_occupancy_does_not_depend_on_source_provider_availability() {
    // R7 ordinary exact-incarnation consumers need occupancy/exit facts and no
    // retention report. Explicit retirement remains a separate projection of
    // this ONE read, not another scan/state authority.
    let identity = awaken_worker_contract::WorkerIdentity::new("occupancy-only", "boot", 1);
    let repo = sqlite_session_repository("provider-independent occupancy").await;
    let id = SessionId::from("occupancy-retained");
    create(repo.as_ref(), retained_root(id.as_str(), &identity)).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let app = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(repo, awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Default::default(),
        None,
    );
    let read = app
        .worker_occupancy()
        .observe_worker_realizations(crate::WorkerRealizationRead::Occupancy(&identity.worker_id))
        .await
        .unwrap();
    assert!(read.occupied);
    assert!(
        !read.sources_retained,
        "no implicit retirement qualification"
    );
    assert!(
        app.worker_occupancy()
            .observe_worker_realizations(crate::WorkerRealizationRead::Occupancy(""))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn worker_retirement_observes_the_existing_disposal_action_without_acknowledging_it() {
    // R8 source effects already admitted/prepared before grant expiry: canonical
    // typed preparation recording selects Dispose. The Session owner lowers
    // THAT action to source_prepared=true; retirement preserves pending disposal
    // and never fabricates completion. The other rows below require false.
    // Fixture receipts are produced only for this synthetic, source-free test;
    // provider/API and deployed receipts are qualified by their own cases.
    let identity = awaken_worker_contract::WorkerIdentity::new("prepared-actor", "boot", 1);
    let id = SessionId::from("retirement-prepared");
    let mut root = retained_root(id.as_str(), &identity);
    source_admission::prepare_terminal_root_for_disposal(&mut root);
    let repo = sqlite_session_repository("prepared source retirement").await;
    create(repo.as_ref(), root).await;
    let before = repo.get(&id).await.unwrap();
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.source_observation.lock().unwrap() =
        awaken_session_execution_contract::SessionSourceDispatchObservation::Settled;
    let app = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Default::default(),
        None,
    );
    let (observed, subjects) =
        qualify_retirement(&app, &identity, |subjects| report(subjects, true)).await;
    let observed = observed.unwrap();
    assert!(observed.occupied && observed.sources_retained);
    assert!(subjects[0].source_prepared);
    assert_eq!(repo.get(&id).await.unwrap(), before);
    assert!(!before.terminal_cleanup.is_completed());
}

fn retained_root(id: &str, identity: &awaken_worker_contract::WorkerIdentity) -> PersistedSession {
    let mut root = registry_race_terminal_session(
        id,
        Some(crate::session_resource::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 7,
            expires_at_unix_ms: 1,
        }),
    );
    root.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "retention-fixture",
            id,
        ))
        .unwrap(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    root
}

#[tokio::test]
async fn active_resident_source_retirement_preserves_the_original_session() {
    // LR1 causes: same Worker/incarnation, complete active Idle+Resident root,
    // frozen baseline, positive independent source retention. A prior Run scope
    // may remain Pending; this is Actor source retention, not its closure.
    // Effect: occupancy remains true and sources_retained becomes true, while
    // root/lease/cleanup/Run facts remain byte-for-byte unchanged. No prepare,
    // installed projection, resource effect or cleanup ACK is produced.
    let identity = awaken_worker_contract::WorkerIdentity::new("live-actor", "boot", 1);
    let id = SessionId::from("retained-active-session");
    let mut root = retained_root(id.as_str(), &identity);
    root.execution = awaken_session_state_contract::SessionExecutionState::Idle;
    root.terminal_cleanup = Default::default();
    let repo = sqlite_session_repository("active resident retirement").await;
    create(repo.as_ref(), root).await;
    let before = repo.get(&id).await.unwrap();
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.source_observation.lock().unwrap() =
        awaken_session_execution_contract::SessionSourceDispatchObservation::Pending;
    let app = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
    );
    let (observed, subjects) =
        qualify_retirement(&app, &identity, |subjects| report(subjects, true)).await;
    let observed = observed.unwrap();
    assert!(observed.occupied);
    assert!(
        observed.sources_retained,
        "LR1 positive reported retained source does not require terminating the Session"
    );
    assert_eq!(repo.get(&id).await.unwrap(), before);
    assert!(!before.is_terminal());
    assert!(!before.terminal_cleanup.is_completed());
    assert_eq!(subjects.len(), 1);
    assert!(!subjects[0].source_prepared);
    assert!(runtime.terminal_assignments.lock().unwrap().is_empty());
    assert!(runtime.intents.lock().unwrap().is_empty());
    assert!(runtime.acknowledged_completed.lock().unwrap().is_empty());
    assert_eq!(
        runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn worker_retirement_of_idle_no_create_sessions_requires_positive_closed_root_facts() {
    // C1 modern explicit-empty Create history vs legacy unknown/admitted Create;
    // C2 exact incarnation and explicitly closed assignment (zero, not expired);
    // C3 frozen Idle + complete Events; C4 no retained Resource references or
    // pending transition; C5 the terminal-source port is not a live-root gate.
    // R1 all root facts -> occupied but retained,
    // no provider subject. R2 each false -> occupied/refused, no partial report.
    // R3 a mixed Resident source still needs its exact positive report. Every
    // row preserves the entire original root and produces no effect/cleanup ACK.
    for rule in [
        "no-create",
        "mcp-pending",
        "legacy",
        "admitted",
        "live-lease",
        "expired-lease",
        "foreign",
        "preparing",
        "active-activity",
        "running-interval",
        "incomplete-events",
        "active-resource",
        "pending-resource",
        "pending-empty",
        "irrelevant-source-pending",
        "source-unavailable",
        "mixed",
        "mixed-missing",
    ] {
        let identity = awaken_worker_contract::WorkerIdentity::new("lazy-worker", "boot", 1);
        let id = SessionId::from(format!("no-create-{rule}"));
        let fixture = retained_root(id.as_str(), &identity);
        let mut baseline = fixture.frozen_baseline().unwrap().clone();
        baseline.environment.self_hosted = false;
        baseline.environment.sandbox_provisioning =
            awaken_session_configuration_contract::SandboxProvisioning::OnToolUse;
        let mut root = PersistedSession::frozen_with_budget(
            id.as_str(),
            baseline,
            Default::default(),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        root.execution = awaken_session_state_contract::SessionExecutionState::Idle;
        root.realization = fixture.realization;
        root.realization.as_mut().unwrap().expires_at_unix_ms = 0;
        match rule {
            "mcp-pending" => {
                // Idle credential adoption can still await a successor. Its
                // desired MCP generation is not an Environment Create effect.
                root.mcp = awaken_session_configuration_contract::mcp::SessionMcpAttachmentSet::from_initial(
                    vec![awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                        name: "flow".into(),
                        target: awaken_agent_contract::McpTarget::parse_http("https://flow.example.test/mcp").unwrap(),
                        prompts_as_skills: false,
                        credential: None,
                        origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
                    }], None,
                ).unwrap();
            }
            "legacy" => {
                // Match the existing persisted historical fixture codec; no
                // public mutation of private materialization history exists.
                let mut wire = serde_json::to_value(&root).unwrap();
                wire.as_object_mut()
                    .unwrap()
                    .remove("environment_materialization");
                root = serde_json::from_value(wire).unwrap();
            }
            "admitted" => {
                root.realization.as_mut().unwrap().expires_at_unix_ms = u64::MAX;
                let intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
                    id.as_str(),
                    awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                    root.realization.clone(),
                )
                .for_environment(
                    root.frozen_baseline()
                        .unwrap()
                        .environment
                        .config_fingerprint
                        .0
                        .clone(),
                );
                let mut aggregate =
                    awaken_session_state_contract::SessionAggregate::try_from(root).unwrap();
                aggregate
                    .environment()
                    .admit_environment_effect("workspace", &intent, 1)
                    .unwrap();
                aggregate.assignment().expire();
                root = aggregate.into_snapshot();
            }
            "live-lease" => root.realization.as_mut().unwrap().expires_at_unix_ms = u64::MAX,
            "expired-lease" => root.realization.as_mut().unwrap().expires_at_unix_ms = 1,
            "foreign" => {
                root.realization.as_mut().unwrap().runtime_incarnation = "other:1:boot".into()
            }
            "preparing" => {
                root.execution = awaken_session_state_contract::SessionExecutionState::Preparing
            }
            "active-activity" => {
                root.active_activity_epochs.insert(1);
            }
            "running-interval" => {
                root.begin_activity_epoch_at(1, None);
                root.execution = awaken_session_state_contract::SessionExecutionState::Idle;
            }
            "incomplete-events" => {
                root.event_batches =
                    crate::tests::authority::session_with_unsettled_event(id.as_str()).event_batches
            }
            "active-resource" => {
                root.resources = awaken_session_state_contract::SessionResourceState::from_active(
                    file_resources("active").clone(),
                )
            }
            "pending-resource" | "pending-empty" => {
                root.resources
                    .prepare(
                        &id,
                        if rule == "pending-empty" {
                            Default::default()
                        } else {
                            file_resources("pending")
                        },
                    )
                    .unwrap();
            }
            _ => {}
        }
        let repo = sqlite_session_repository("no-Create retirement").await;
        create(repo.as_ref(), root).await;
        let before = repo.get(&id).await.unwrap();
        assert!(
            !crate::archive_intent::terminal_environment_creation_not_attempted(
                repo.as_ref(),
                "workspace",
                &before,
            )
            .await
            .unwrap(),
            "a live no-Create fact must not bypass terminal cleanup admission: {rule}",
        );
        let mixed = matches!(rule, "mixed" | "mixed-missing");
        if mixed {
            create(
                repo.as_ref(),
                retained_root("no-create-other-source", &identity),
            )
            .await;
        }
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        // The source port's real nonterminal answer is NotTerminal, not
        // Settled. Sentinels also prove the live branch never consumes this
        // unrelated dependency. Only the mixed TERMINAL root may consume two
        // Settled reads, once per complete scan.
        let sentinel = match rule {
            "irrelevant-source-pending" => {
                awaken_session_execution_contract::SessionSourceDispatchObservation::Pending
            }
            "source-unavailable" => Default::default(),
            _ => awaken_session_execution_contract::SessionSourceDispatchObservation::NotProven {
                reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::NotTerminal,
            },
        };
        {
            let mut steps = runtime.source_observation_steps.lock().unwrap();
            if mixed {
                steps.extend([
                    awaken_session_execution_contract::SessionSourceDispatchObservation::Settled,
                    awaken_session_execution_contract::SessionSourceDispatchObservation::Settled,
                ]);
            }
            steps.push_back(sentinel.clone());
        }
        let app = configured_application(
            runtime.clone(),
            crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            Default::default(),
            None,
        );
        let (observed, subjects) = qualify_retirement(&app, &identity, |subjects| {
            if rule == "mixed-missing" {
                BTreeSet::new()
            } else {
                report(subjects, true)
            }
        })
        .await;
        let observed = observed.unwrap();
        assert!(observed.occupied, "original ownership retained: {rule}");
        assert_eq!(
            observed.sources_retained,
            matches!(
                rule,
                "no-create"
                    | "mcp-pending"
                    | "irrelevant-source-pending"
                    | "source-unavailable"
                    | "mixed"
            ),
            "R1-R3 {rule}"
        );
        assert_eq!(
            subjects.len(),
            usize::from(mixed),
            "no partial source set: {rule}"
        );
        assert_eq!(
            repo.get(&id).await.unwrap(),
            before,
            "unchanged root: {rule}"
        );
        assert!(runtime.intents.lock().unwrap().is_empty());
        assert_eq!(
            *runtime.source_observation_steps.lock().unwrap(),
            VecDeque::from([sentinel]),
            "only a mixed terminal root may read its frozen source scope: {rule}",
        );
        assert!(runtime.acknowledged_completed.lock().unwrap().is_empty());
        assert_eq!(
            runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
            0
        );
    }
}

#[tokio::test]
async fn worker_retirement_requires_each_terminal_source_without_mutating_roots() {
    // Causes: exact Actor incarnation; live/expired grant; terminal/frozen root;
    // settled source Dispatch; Worker report retained/not retained/missing/
    // stale. Ordered rules R1 foreign/preparing/activating/nonresident/
    // incomplete events or terminal waiting/unsettled facts -> false and no
    // subject is named. R2 all root facts + a retained report for the exact
    // subject -> true; R3 a not-retained report -> false; R4 a missing report
    // (the Worker's observation failed or never ran) -> false, never an error
    // and never a Coordinator-side observation; R5 a report for another
    // digest (stale) -> false. Every rule keeps the complete root/lease/
    // binding/preparations unchanged and installs no Runtime projection. LR2
    // Active Idle/Running/Rescheduling + Resident names the same subject with
    // unprepared-source semantics; neither its Run nor cleanup is completed.
    // LR3 a not-retained or missing report for it still refuses. The Worker's
    // provider witnesses own retention; private Memory refusal remains in that
    // Worker-side provider-selection/read owner.
    for row in [
        "retained",
        "provider-unknown",
        "report-missing",
        "report-stale",
        "live",
        "foreign",
        "nonterminal",
        "running",
        "rescheduling",
        "preparing",
        "activating",
        "nonresident",
        "incomplete-events",
        "live-provider-unknown",
        "live-report-missing",
        "waiting",
        "pending",
        "unsealed",
    ] {
        let identity = awaken_worker_contract::WorkerIdentity::new("retiring-worker", "boot", 1);
        let id = SessionId::from(format!("retirement-{row}"));
        let repo = sqlite_session_repository("retirement read qualification").await;
        let mut root = retained_root(id.as_str(), &identity);
        match row {
            "live" => root.realization.as_mut().unwrap().expires_at_unix_ms = u64::MAX,
            "foreign" => {
                root.realization.as_mut().unwrap().runtime_incarnation =
                    "retiring-worker:2:other-boot".into()
            }
            "nonterminal"
            | "running"
            | "rescheduling"
            | "preparing"
            | "activating"
            | "nonresident"
            | "incomplete-events"
            | "live-provider-unknown"
            | "live-report-missing" => {
                use awaken_session_state_contract::SessionExecutionState;
                root.execution = match row {
                    "running" => SessionExecutionState::Running,
                    "rescheduling" => SessionExecutionState::Rescheduling,
                    "preparing" => SessionExecutionState::Preparing,
                    "activating" => SessionExecutionState::Activating,
                    _ => SessionExecutionState::Idle,
                };
                root.terminal_cleanup = Default::default();
                if row == "running" {
                    // The common persisted fixture intentionally models legacy
                    // missing history. Use the aggregate's existing fresh-root
                    // constructor for a positively recorded Run admission;
                    // never manufacture history on that legacy root.
                    let environment = root.environment.clone();
                    let realization = root.realization.clone();
                    root = PersistedSession::frozen_with_budget(
                        id.as_str(),
                        root.frozen_baseline().unwrap().clone(),
                        root.resources,
                        root.mcp,
                        root.title,
                        root.metadata,
                        root.tools,
                        root.budget,
                    );
                    root.environment = environment;
                    root.realization = realization;
                    root.execution = SessionExecutionState::Running;
                    // LR2 keeps a real named active Run admission, not only a
                    // legacy Running label with no retained activity history.
                    let thread = awaken_agent_contract::ThreadId(id.as_str().into());
                    let run = awaken_agent_contract::RunId("retained-running-run".into());
                    let operation =
                        awaken_session_execution_contract::session_run_activity_operation_id(
                            &id, &run,
                        );
                    let epoch = 2;
                    assert!(root.begin_activity_epoch_at(epoch, Some((&operation, &thread, &run))));
                    assert!(root.active_activity_epochs.contains(&epoch));
                    assert_eq!(root.run_activities().unwrap().0.len(), 1);
                }
                if row == "nonresident" {
                    root.environment =
                        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized;
                }
                if row == "incomplete-events" {
                    root.event_batches =
                        crate::tests::authority::session_with_unsettled_event(id.as_str())
                            .event_batches;
                    assert!(root.has_incomplete_event_batches());
                }
            }
            "waiting" => {
                root.terminal_cleanup = Default::default();
                assert!(root.terminal_cleanup.request(&id));
            }
            _ => {}
        }
        create(repo.as_ref(), root).await;
        let before = repo.get(&id).await.unwrap();
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        *runtime.source_observation.lock().unwrap() = match row {
            "pending" => awaken_session_execution_contract::SessionSourceDispatchObservation::Pending,
            "unsealed" => {
                awaken_session_execution_contract::SessionSourceDispatchObservation::NotProven {
                    reason:
                        awaken_session_execution_contract::SessionExecutionRecoveryGap::ScopeNotFrozen,
                }
            }
            _ => awaken_session_execution_contract::SessionSourceDispatchObservation::Settled,
        };
        let app = configured_application(
            runtime.clone(),
            crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
            None,
        );
        let (result, subjects) = qualify_retirement(&app, &identity, |subjects| {
            if row.ends_with("report-missing") {
                BTreeSet::new()
            } else if row == "report-stale" {
                report(subjects, true)
                    .into_iter()
                    .map(|fact| WorkerSourceRetention {
                        subject: format!("{}-before-the-subject-changed", fact.subject),
                        ..fact
                    })
                    .collect()
            } else {
                report(subjects, !row.ends_with("provider-unknown"))
            }
        })
        .await;
        let observed = result.unwrap();
        assert!(
            observed.occupied,
            "retirement never clears cleanup occupancy"
        );
        assert_eq!(
            observed.sources_retained,
            matches!(
                row,
                "retained" | "live" | "nonterminal" | "running" | "rescheduling"
            ),
            "R1-R5 {row}"
        );
        assert_eq!(
            repo.get(&id).await.unwrap(),
            before,
            "unchanged root: {row}"
        );
        assert!(
            runtime.terminal_assignments.lock().unwrap().is_empty(),
            "no installed slot"
        );
        assert!(
            runtime.intents.lock().unwrap().is_empty(),
            "no source preparation"
        );
        assert!(
            runtime.acknowledged_completed.lock().unwrap().is_empty(),
            "no cleanup ACK"
        );
        assert_eq!(
            runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            subjects.len(),
            usize::from(matches!(
                row,
                "retained"
                    | "live"
                    | "provider-unknown"
                    | "report-missing"
                    | "report-stale"
                    | "nonterminal"
                    | "running"
                    | "rescheduling"
                    | "live-provider-unknown"
                    | "live-report-missing"
            )),
            "subject gate: {row}"
        );
        if let Some(SourceRetentionSubject {
            session_id: source_id,
            projection,
            source_prepared,
            ..
        }) = subjects.first()
        {
            assert!(
                !source_prepared,
                "Prepare cannot claim durable preparation: {row}"
            );
            assert_eq!(source_id.as_str(), id.as_str());
            assert_eq!(
                projection.environment, before.environment,
                "original binding"
            );
            assert_eq!(
                &projection.baseline,
                before.frozen_baseline().unwrap(),
                "complete baseline"
            );
            if before.is_terminal() {
                assert!(
                    projection.previous_resource_manifest.is_some(),
                    "complete terminal resource endpoints"
                );
            }
        }
    }
}

#[tokio::test]
async fn worker_retirement_never_publishes_a_successful_source_prefix() {
    // R6 one retained source cannot qualify a later Preparing or incomplete
    // root: the blocked read names no subject at all. R7 all sources retained,
    // including a mixed terminal/active set, qualify only once every subject
    // of the complete set is reported. Roots remain unchanged and no
    // successful prefix is published when a later root refuses.
    for later in [
        "terminal",
        "retained-active",
        "preparing",
        "incomplete-events",
    ] {
        let identity =
            awaken_worker_contract::WorkerIdentity::new("multi-source-worker", "boot", 1);
        let repo = sqlite_session_repository("whole retirement source set").await;
        let mut roots = Vec::new();
        for id in ["retirement-a", "retirement-z"] {
            let mut root = retained_root(id, &identity);
            if id.ends_with('z') && later != "terminal" {
                root.execution = if later == "preparing" {
                    awaken_session_state_contract::SessionExecutionState::Preparing
                } else {
                    awaken_session_state_contract::SessionExecutionState::Idle
                };
                root.terminal_cleanup = Default::default();
                if later == "incomplete-events" {
                    root.event_batches =
                        crate::tests::authority::session_with_unsettled_event(id).event_batches;
                }
            }
            create(repo.as_ref(), root).await;
            roots.push(repo.get(&SessionId::from(id)).await.unwrap());
        }
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        *runtime.source_observation.lock().unwrap() =
            awaken_session_execution_contract::SessionSourceDispatchObservation::Settled;
        let app = configured_application(
            runtime.clone(),
            crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
            None,
        );
        let (observed, subjects) =
            qualify_retirement(&app, &identity, |subjects| report(subjects, true)).await;
        assert_eq!(
            observed.unwrap().sources_retained,
            matches!(later, "terminal" | "retained-active"),
        );
        for root in roots {
            assert_eq!(repo.get(&root.session_id).await.unwrap(), root);
        }
        assert_eq!(
            subjects.len(),
            if matches!(later, "terminal" | "retained-active") {
                2
            } else {
                0
            },
            "no partial successful source set: {later}",
        );
    }
}

#[tokio::test]
async fn worker_retirement_explains_every_refusing_session_without_changing_the_decision() {
    // Causes: C1 Sessions held by one exact Worker: A Idle+Resident with a
    // retainable source, B Idle but not Resident, C leased to another
    // incarnation of the same Worker; C2 the Worker's retention report.
    // Effects: E1 the decision (unchanged law: any refusal denies); E2 the
    // explanation names every refusing Session with its reason, including
    // Sessions after the first refusal; E3 reads mutate nothing.
    // | Rule | Sessions | report      | decision  | explanation                     |
    // | X1   | A,B,C    | any         | denied    | B=EnvironmentNotResident,       |
    // |      |          |             |           | C=LeaseHeldByAnotherIncarnation |
    // | X2   | A        | A retained  | permitted | empty                           |
    // | X3   | A        | none        | denied    | A=SourceRetentionNotReported    |
    use crate::WorkerRetirementBlockReason::{
        EnvironmentNotResident, LeaseHeldByAnotherIncarnation, SourceRetentionNotReported,
    };
    let identity = awaken_worker_contract::WorkerIdentity::new("explained-actor", "boot", 2);
    let idle = |id: &str| {
        let mut root = retained_root(id, &identity);
        root.execution = awaken_session_state_contract::SessionExecutionState::Idle;
        root.terminal_cleanup = Default::default();
        root
    };
    let explain = |blockers: Vec<crate::WorkerRetirementBlocker>| {
        blockers
            .into_iter()
            .map(|blocker| (blocker.session_id, blocker.reason))
            .collect::<Vec<_>>()
    };
    let application = |repo| {
        configured_application(
            Arc::new(RecordingCleanupRuntime::default()),
            crate::SessionPersistence::new(repo, awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
            None,
        )
    };

    let repo = sqlite_session_repository("explained retirement").await;
    create(repo.as_ref(), idle("a-resident")).await;
    let mut unmaterialized = idle("b-unmaterialized");
    unmaterialized.environment =
        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized;
    create(repo.as_ref(), unmaterialized).await;
    let mut foreign = idle("c-foreign");
    foreign.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: awaken_worker_contract::WorkerIdentity::new(
            "explained-actor",
            "boot",
            1,
        )
        .lease_owner(),
        epoch: 7,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), foreign).await;
    let ids = ["a-resident", "b-unmaterialized", "c-foreign"];
    let mut before = Vec::new();
    for id in ids {
        before.push(repo.get(&SessionId::from(id)).await.unwrap());
    }
    let app = application(repo.clone());
    let (observed, subjects) =
        qualify_retirement(&app, &identity, |subjects| report(subjects, true)).await;
    let observed = observed.unwrap();
    assert!(observed.occupied, "X1");
    assert!(!observed.sources_retained, "X1/E1 any refusal denies");
    assert!(subjects.is_empty(), "X1 a refused read names no subject");
    assert_eq!(
        explain(
            app.worker_occupancy()
                .explain_worker_retirement(&identity, &BTreeSet::new())
                .await
                .unwrap()
        ),
        vec![
            ("b-unmaterialized".to_owned(), EnvironmentNotResident),
            ("c-foreign".to_owned(), LeaseHeldByAnotherIncarnation),
        ],
        "X1/E2 every refusal, sorted"
    );
    for (id, before) in ids.into_iter().zip(&before) {
        assert_eq!(
            &repo.get(&SessionId::from(id)).await.unwrap(),
            before,
            "E3 {id}"
        );
    }

    let repo = sqlite_session_repository("explained retirement single").await;
    create(repo.as_ref(), idle("a-resident")).await;
    let app = application(repo);
    let (observed, subjects) =
        qualify_retirement(&app, &identity, |subjects| report(subjects, true)).await;
    assert!(observed.unwrap().sources_retained, "X2/E1");
    assert!(
        app.worker_occupancy()
            .explain_worker_retirement(&identity, &report(&subjects, true))
            .await
            .unwrap()
            .is_empty(),
        "X2/E2"
    );
    assert_eq!(
        explain(
            app.worker_occupancy()
                .explain_worker_retirement(&identity, &BTreeSet::new())
                .await
                .unwrap()
        ),
        vec![("a-resident".to_owned(), SourceRetentionNotReported)],
        "X3/E2"
    );
}
