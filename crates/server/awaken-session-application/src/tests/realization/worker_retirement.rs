// @formal evidence session_realization.concurrent_drivers_preserve_one_owner_fence
//! Read qualification retains the original roots and delegates physical facts.
use super::*;

#[tokio::test]
async fn ordinary_worker_occupancy_does_not_depend_on_source_provider_availability() {
    // R7 ordinary exact-incarnation consumers need occupancy/exit facts even
    // when the source provider is unavailable. Explicit retirement remains a
    // separate projection of this ONE read, not another scan/state authority.
    let identity = awaken_worker_contract::WorkerIdentity::new("occupancy-only", "boot", 1);
    let repo = sqlite_session_repository("provider-independent occupancy").await;
    let id = SessionId::from("occupancy-retained");
    create(repo.as_ref(), retained_root(id.as_str(), &identity)).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.retention_unavailable.store(true, Ordering::SeqCst);
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
    assert!(runtime.retention_reads.lock().unwrap().is_empty());
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
    let lease = root.realization.clone().unwrap();
    let commands = match root.terminal_cleanup_work_action().unwrap().unwrap() {
        awaken_session_state_contract::SessionTerminalCleanupAction::Prepare { commands } => {
            commands
        }
        other => panic!("original preparation: {other:?}"),
    };
    for command in commands {
        let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
            command,
            lease.clone(),
        );
        let prepared = awaken_session_state_contract::SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap();
        let repository = awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
            &id,
            "workspace",
            &root.resources,
        )
        .unwrap();
        root.record_terminal_cleanup_preparation("workspace", &lease, prepared, Some(repository))
            .unwrap();
    }
    assert!(matches!(
        root.terminal_cleanup_work_action().unwrap(),
        Some(awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { .. })
    ));
    let repo = sqlite_session_repository("prepared source retirement").await;
    create(repo.as_ref(), root).await;
    let before = repo.get(&id).await.unwrap();
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.source_retained.store(true, Ordering::SeqCst);
    *runtime.source_observation.lock().unwrap() =
        awaken_session_execution_contract::SessionSourceDispatchObservation::Settled;
    let app = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Default::default(),
        None,
    );
    let observed = app
        .worker_occupancy()
        .observe_worker_realizations(crate::WorkerRealizationRead::Retirement(&identity))
        .await
        .unwrap();
    assert!(observed.occupied && observed.sources_retained);
    assert!(runtime.retention_reads.lock().unwrap()[0].2);
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
    runtime.source_retained.store(true, Ordering::SeqCst);
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
    let observed = app
        .worker_occupancy()
        .observe_worker_realizations(crate::WorkerRealizationRead::Retirement(&identity))
        .await
        .unwrap();
    assert!(observed.occupied);
    assert!(
        observed.sources_retained,
        "LR1 positive retained source does not require terminating the Session"
    );
    assert_eq!(repo.get(&id).await.unwrap(), before);
    assert!(!before.is_terminal());
    assert!(!before.terminal_cleanup.is_completed());
    assert_eq!(runtime.retention_reads.lock().unwrap().len(), 1);
    assert!(!runtime.retention_reads.lock().unwrap()[0].2);
    assert!(runtime.terminal_assignments.lock().unwrap().is_empty());
    assert!(runtime.intents.lock().unwrap().is_empty());
    assert!(runtime.acknowledged_completed.lock().unwrap().is_empty());
    assert_eq!(
        runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn worker_retirement_requires_each_terminal_source_without_mutating_roots() {
    // Causes: exact Actor incarnation; live/expired grant; terminal/frozen root;
    // settled source Dispatch; provider retained/unknown/unavailable. Ordered
    // rules R1 foreign/preparing/activating/nonresident/incomplete events or
    // terminal waiting/unsettled facts -> false
    // before provider I/O. R2 all root facts + positive provider -> true;
    // R3 unknown provider -> false; R4 failed provider -> Err. Every rule keeps
    // the complete root/lease/binding/preparations unchanged and installs no
    // Runtime projection. LR2 Active Idle/Running/Rescheduling + Resident uses
    // the same provider with unprepared-source semantics; neither its Run nor
    // cleanup is completed. LR3 unknown/error provider still refuses/errors.
    // The physical provider's API witnesses own retention; private Memory
    // refusal remains in that existing provider-selection/read owner.
    for row in [
        "retained",
        "provider-unknown",
        "provider-error",
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
        "live-provider-error",
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
            | "live-provider-error" => {
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
        runtime
            .source_retained
            .store(!row.ends_with("provider-unknown"), Ordering::SeqCst);
        runtime
            .retention_unavailable
            .store(row.ends_with("provider-error"), Ordering::SeqCst);
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
        let result = app
            .worker_occupancy()
            .observe_worker_realizations(crate::WorkerRealizationRead::Retirement(&identity))
            .await;
        if row.ends_with("provider-error") {
            assert!(result.is_err(), "R4");
        } else {
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
                "R1/R2/R3 {row}"
            );
        }
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
        let reads = runtime.retention_reads.lock().unwrap();
        assert_eq!(
            reads.len(),
            usize::from(matches!(
                row,
                "retained"
                    | "live"
                    | "provider-unknown"
                    | "provider-error"
                    | "nonterminal"
                    | "running"
                    | "rescheduling"
                    | "live-provider-unknown"
                    | "live-provider-error"
            )),
            "provider read gate: {row}"
        );
        if let Some((source_id, projection, source_prepared)) = reads.first() {
            assert!(
                !source_prepared,
                "Prepare cannot claim durable preparation: {row}"
            );
            assert_eq!(source_id, id.as_str());
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
    // R5 one retained source cannot qualify a later Preparing or incomplete
    // root. R6 all sources retained, including a mixed terminal/active set,
    // qualify only after the complete set is observed. Roots remain unchanged
    // and no successful prefix is published when a later root refuses.
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
        runtime.source_retained.store(true, Ordering::SeqCst);
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
        assert_eq!(
            app.worker_occupancy()
                .observe_worker_realizations(crate::WorkerRealizationRead::Retirement(&identity))
                .await
                .unwrap()
                .sources_retained,
            matches!(later, "terminal" | "retained-active"),
        );
        for root in roots {
            assert_eq!(repo.get(&root.session_id).await.unwrap(), root);
        }
        assert_eq!(
            runtime.retention_reads.lock().unwrap().len(),
            if matches!(later, "terminal" | "retained-active") {
                2
            } else {
                1
            },
            "no partial successful source set: {later}",
        );
    }
}
