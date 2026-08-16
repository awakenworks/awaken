use super::*;

#[tokio::test]
async fn root_mutation_cause_effect_decision_table() {
    // Cause-effect graph: C1 expected revision is current; C2 idempotency key
    // and payload hash replay exactly; C3 expected revision is stale; C4 an
    // existing key is reused with another hash. Effects: E1 apply once and
    // advance revision; E2 replay current truth without another advance; E3
    // conflict; E4 idempotency mismatch. No interface adapter owns a second
    // CAS or retry algorithm.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | M1 | yes | no | no | no | E1 applied |
    // | M2 | no | yes | no | no | E2 replayed |
    // | M3 | no | no | yes | no | E3 conflict |
    // | M4 | no | no | no | yes | E4 mismatch |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("mutation", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let original = repo.get("mutation").await.expect("fixture");
    let mut candidate = original.clone();
    candidate.title = Some("applied".into());
    let payload = awaken_session_contract::SessionMutationPayload::Replace(candidate.clone());
    let record = awaken_session_contract::IdempotencyRecord {
        key: "mutation:one".into(),
        payload_hash: payload.stable_hash(),
    };

    let (applied, changed) = app
        .commit_session_snapshot_with_record(
            "workspace",
            candidate.clone(),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("M1");
    assert!(changed, "M1");
    assert!(applied.revision > original.revision, "M1");

    let (replayed, changed) = app
        .commit_session_snapshot_with_record(
            "workspace",
            candidate.clone(),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("M2");
    assert!(!changed, "M2");
    assert_eq!(replayed.revision, applied.revision, "M2");

    let stale = app
        .commit_session_snapshot("workspace", candidate.clone(), "stale", Vec::new())
        .await;
    assert_eq!(stale, Err(SessionMutationError::Conflict), "M3");

    let mismatch = app
        .commit_session_snapshot_with_record(
            "workspace",
            candidate,
            awaken_session_contract::IdempotencyRecord {
                key: record.key,
                payload_hash: "another-payload".into(),
            },
            Vec::new(),
        )
        .await;
    assert_eq!(
        mismatch,
        Err(SessionMutationError::IdempotencyMismatch),
        "M4"
    );
}

/// Session-root insertion graph. C1 identity is unused; C2 key/hash/payload
/// exactly replay; C3 an existing key carries another hash; C4 another key
/// targets an existing identity. Effects are E1 one insert/revision advance,
/// E2 replay of the same revision, E3 idempotency mismatch, and E4 identity
/// conflict. The application is the only repository-result classifier.
///
/// | Rule | Identity | Key/hash | Payload | Effect |
/// |---|---|---|---|---|
/// | C1 | unused | new | original | E1 |
/// | C2 | existing | exact | exact | E2 |
/// | C3 | existing | same key/different hash | changed | E3 |
/// | C4 | existing | another key | any | E4 |
#[tokio::test]
async fn create_session_root_classifies_insert_replay_and_conflicts() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let original = persisted("create-root", false, "preparing");
    let payload = awaken_session_contract::SessionMutationPayload::Replace(original.clone());
    let record = awaken_session_contract::IdempotencyRecord {
        key: "create-root:one".into(),
        payload_hash: payload.stable_hash(),
    };

    let inserted = app
        .create_session_root("workspace", original.clone(), record.clone(), Vec::new())
        .await
        .expect("C1");
    assert_eq!(
        inserted.revision,
        awaken_session_contract::SessionRevision(1),
        "C1"
    );
    let replayed = app
        .create_session_root("workspace", original.clone(), record.clone(), Vec::new())
        .await
        .expect("C2");
    assert_eq!(replayed.revision, inserted.revision, "C2");

    let mut changed = original.clone();
    changed.title = Some("changed".into());
    assert_eq!(
        app.create_session_root(
            "workspace",
            changed,
            awaken_session_contract::IdempotencyRecord {
                key: record.key,
                payload_hash: "changed-hash".into(),
            },
            Vec::new(),
        )
        .await,
        Err(SessionMutationError::IdempotencyMismatch),
        "C3"
    );
    assert_eq!(
        app.create_session_root(
            "workspace",
            original,
            awaken_session_contract::IdempotencyRecord {
                key: "create-root:another".into(),
                payload_hash: "another-hash".into(),
            },
            Vec::new(),
        )
        .await,
        Err(SessionMutationError::Conflict),
        "C4"
    );
}

/// Terminal-transition cause/effect graph. C1 the durable Session is live;
/// C2 two archive commands race; C3 the same archive is replayed; C4 delete
/// targets a live Session; C5 delete targets an archived or failed Session. Effects:
/// E1 exactly one archive CAS/fact and one idempotent observation; E2 replay
/// does not advance revision; E3 delete atomically records hidden terminal
/// disposition plus recoverable cleanup classification; E4 retention remains
/// orthogonal and both terminal execution states remain deletable.
///
/// | Rule | Durable state | Command | Race/replay | Effect |
/// |---|---|---|---|---|
/// | L1 | idle | archive | race | E1 |
/// | L2 | terminated | archive | replay | E2 |
/// | L3 | idle | delete | none | E3 |
/// | L4 | archived | delete | none | E4 |
/// | L5 | activation_failed | delete | none | E4 |
#[tokio::test]
async fn terminal_transition_decision_table_is_durable_and_idempotent() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("archive-race", false, "idle")).await;
    create(repo.as_ref(), persisted("delete-live", true, "idle")).await;
    create(repo.as_ref(), persisted("archive-work", true, "idle")).await;
    create(
        repo.as_ref(),
        persisted("delete-failed", false, "activation_failed"),
    )
    .await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application(repo.clone(), environments.clone());
    let fact = |id: &str, event_type: &str| awaken_session_contract::ManagedLifecycleFact {
        id: format!("{id}:{event_type}"),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: event_type.into(),
        timestamp: 1,
        runtime_interval: None,
    };

    let archive_fact = fact("archive-race", "session.terminated");
    let (first, second) = tokio::join!(
        app.begin_archive("archive-race", "2026-08-06T00:00:00Z", archive_fact.clone()),
        app.begin_archive("archive-race", "2026-08-06T00:00:00Z", archive_fact)
    );
    let first = first.expect("L1 first");
    let second = second.expect("L1 second");
    assert_ne!(first.transitioned, second.transitioned, "L1");
    let archived = repo.get("archive-race").await.expect("L1 durable");
    assert_eq!(archived.execution.as_str(), "terminated", "L1");
    let revision = archived.revision;
    let replay = app
        .begin_archive(
            "archive-race",
            "another-timestamp",
            fact("archive-race", "session.terminated"),
        )
        .await
        .expect("L2");
    assert!(!replay.transitioned, "L2");
    assert_eq!(replay.session.revision, revision, "L2");

    let app = Arc::new(app);
    let deleted = app
        .delete_session(SessionDeleteCommand::new("delete-live"))
        .await
        .expect("L3");
    let transition = deleted.clone();
    app.release_terminal_resources(&transition.owner_scope, "delete-live")
        .await
        .expect("L3 reconciliation");
    assert!(transition.transitioned, "L3");
    assert_eq!(transition.session.execution.as_str(), "terminated", "L3");
    assert!(transition.session.is_hidden(), "L3");
    assert!(transition.session.needs_resource_reconciliation(), "L3");
    let tombstone = repo.get("delete-live").await.expect_err("L3 tombstone");
    assert!(matches!(
        tombstone,
        awaken_session_contract::SessionRepositoryError::NotFound
    ));
    assert!(
        environments
            .retired
            .lock()
            .unwrap()
            .iter()
            .any(|session_id| session_id == "delete-live"),
        "L3 terminal truth retires its one Work projection"
    );
    app.terminate_session(
        "archive-work",
        "2026-08-06T00:00:00Z",
        fact("archive-work", "session.terminated"),
    )
    .await
    .expect("L3a archive cleanup");
    app.terminate_session(
        "archive-work",
        "2026-08-06T00:00:00Z",
        fact("archive-work", "session.terminated"),
    )
    .await
    .expect("L3a archive replay");
    assert_eq!(
        environments
            .retired
            .lock()
            .unwrap()
            .iter()
            .filter(|session_id| session_id.as_str() == "archive-work")
            .count(),
        1,
        "L3a completed archive cleanup does not retire Work twice"
    );
    let archived_delete = app
        .commit_delete_intent(SessionDeleteCommand::new("archive-race"))
        .await
        .expect("L4 archived Session is deletable");
    assert!(archived_delete.transitioned, "L4");
    assert!(archived_delete.session.is_hidden(), "L4");
    let failed_delete = app
        .commit_delete_intent(SessionDeleteCommand::new("delete-failed"))
        .await
        .expect("L5 failed Session is deletable");
    assert!(failed_delete.transitioned, "L5");
    assert_eq!(
        failed_delete.session.execution,
        SessionExecutionState::ActivationFailed,
        "L5 execution failure remains audit truth"
    );
}

#[tokio::test]
async fn delete_fact_is_committed_once_at_the_fence_and_not_reemitted_by_tombstone() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("delete-fact-once", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let transition = app
        .commit_delete_intent(SessionDeleteCommand::new("delete-fact-once"))
        .await
        .expect("Delete fence");
    let facts = repo.pending_lifecycle().await.expect("Delete outbox");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].id, "session:delete-fact-once:deleted");
    repo.complete_lifecycle(&facts[0].id)
        .await
        .expect("consumer acknowledged Delete fact");

    app.release_terminal_resources(&transition.owner_scope, "delete-fact-once")
        .await
        .expect("verified cleanup and tombstone");
    assert!(repo.pending_lifecycle().await.unwrap().is_empty());
    assert!(matches!(
        repo.get("delete-fact-once").await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    ));
}

#[tokio::test]
async fn session_work_authority_classifies_scope_before_queue_access() {
    // Cause/effect graph: C1 no Session root; C2 a Cloud Session; C3 a
    // self-hosted nonterminal Session whose stopped Work can be revived by a
    // claimed successor; C4 terminal self-hosted Session; C5 renewal without a
    // Run claim. Effects: E1/C1 and
    // E1/C2 are outside the Work ownership boundary; E2/C3 reuses the canonical
    // wake path then leases once; E3/C4 and E3/C5 stay unowned. FMECA: without E2, a
    // predecessor retire racing a successor wake loses the approved input;
    // applying E2 to C4 resurrects terminal effects; applying it to C5 revives
    // settled Work with no Run left to release it and blocks the Environment.
    //
    // | Rule | Session | Environment | Queue result | Effect |
    // |---|---|---|---|---|
    // | W1 | absent | n/a | not called | NotRequired |
    // | W2 | present | Cloud | not called | NotRequired |
    // | W3 | present | self-hosted/nonterminal | stopped | wake + Leased |
    // | W4 | present | self-hosted/terminal | stopped | Unowned; no wake |
    // | W5 | present | self-hosted/nonterminal renewal | stopped | Unowned; no wake |
    use awaken_session_contract::work_queue::{
        SessionWorkAcquisition, SessionWorkLeaseAuthority, SessionWorkOwnership,
    };

    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("cloud-work", false, "idle")).await;
    create(repo.as_ref(), persisted("self-work", true, "idle")).await;
    create(repo.as_ref(), persisted("renewal-work", true, "idle")).await;
    create(
        repo.as_ref(),
        persisted("terminal-work", true, "terminated"),
    )
    .await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application(repo, environments.clone());

    assert_eq!(
        app.acquire_session_work(
            "missing",
            "owner",
            1,
            SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .expect("W1"),
        SessionWorkOwnership::NotRequired,
        "W1"
    );
    assert_eq!(
        app.acquire_session_work(
            "cloud-work",
            "owner",
            1,
            SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .expect("W2"),
        SessionWorkOwnership::NotRequired,
        "W2"
    );
    let SessionWorkOwnership::Leased(lease) = app
        .acquire_session_work("self-work", "owner", 1, SessionWorkAcquisition::ClaimedRun)
        .await
        .expect("W3")
    else {
        panic!("W3 must revive and lease the stopped Session Work");
    };
    assert_eq!(lease.owner, "owner", "W3");
    assert!(
        environments.awakened.lock().unwrap().contains("self-work"),
        "W3"
    );
    assert_eq!(
        app.acquire_session_work(
            "terminal-work",
            "owner",
            1,
            SessionWorkAcquisition::ClaimedRun,
        )
        .await
        .expect("W4"),
        SessionWorkOwnership::Unowned,
        "W4"
    );
    assert!(
        !environments
            .awakened
            .lock()
            .unwrap()
            .contains("terminal-work"),
        "W4"
    );
    assert_eq!(
        app.acquire_session_work(
            "renewal-work",
            "renewal-owner",
            2,
            SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .expect("W5"),
        SessionWorkOwnership::Unowned,
        "W5"
    );
    assert!(
        !environments
            .awakened
            .lock()
            .unwrap()
            .contains("renewal-work"),
        "W5"
    );
}

/// Activity-fence FMECA cause/effect graph. Causes: C1 the Session exists;
/// C2 it is ready (idle/running/rescheduling or Worker-owned preparing); C3 the epoch can advance; C4 settlement presents
/// the current epoch; C5 a later admission or terminal transition has
/// fenced that settlement; C6 initial realization is still preparing.
/// Effects: E1 an idle admission commits `running` with one unique monotonic
/// epoch and opens one interval; E2 only the current running completion commits
/// `idle` and the matching interval fact; E3 stale or terminal completions are
/// no-ops; E4 missing, terminal, not-ready, and exhausted-epoch admissions do
/// not mutate durable truth; E5 terminal intent closes the same open interval;
/// E6 a Worker-owned initial event advances the epoch but preserves Preparing
/// until the claimed Worker acknowledges realization.
///
/// | Rule | Exists | Status | Epoch available | Current settle | Fence | Effect |
/// |---|---|---|---|---|---|---|
/// | A1 | yes | idle | yes | n/a | concurrent admit | E1, distinct epochs |
/// | A2 | yes | running | n/a | no | newer epoch | E3, remains running |
/// | A3 | yes | running | n/a | yes | none | E2, idle + one interval fact |
/// | A4 | yes | running | n/a | n/a | terminal command | E5, terminal + interval fact |
/// | A5 | yes | terminal | any | n/a | n/a | E4, reject admission |
/// | A6 | yes | idle | no | n/a | n/a | E4, reject exhaustion |
/// | A7 | no | n/a | n/a | n/a | n/a | E4, not found |
/// | A8 | yes | local preparing | yes | n/a | realization pending | E4, reject admission |
/// | A9 | yes | activation_failed | n/a | any | realization failed | E3, preserve failed |
/// | A10 | yes | Worker preparing | yes | n/a | event triggers claim | E6, admit |
#[tokio::test]
async fn activity_fence_decision_table_preserves_monotonic_and_terminal_truth() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("activity", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (first, second) = tokio::join!(
        app.begin_activity("activity"),
        app.begin_activity("activity")
    );
    let first = first.expect("A1 first admission");
    let second = second.expect("A1 concurrent admission");
    let mut epochs = [first.activity_epoch, second.activity_epoch];
    epochs.sort_unstable();
    assert_eq!(epochs, [1, 2], "A1");
    let active = repo.get("activity").await.expect("A1 durable Session");
    assert_eq!(active.execution.as_str(), "running", "A1");
    let first_interval = active
        .running_interval
        .clone()
        .expect("A1 one durable interval");
    assert_eq!(first_interval.activity_epoch, epochs[0], "A1 joins overlap");

    let stale = app
        .settle_activity("activity", epochs[0])
        .await
        .expect("A2 stale settlement");
    assert_eq!(stale.execution.as_str(), "running", "A2");
    assert_eq!(stale.activity_epoch, epochs[1], "A2");
    assert_eq!(stale.running_interval, Some(first_interval.clone()), "A2");

    let idle = app
        .settle_activity("activity", epochs[1])
        .await
        .expect("A3 current settlement");
    assert_eq!(idle.execution.as_str(), "idle", "A3");
    assert!(idle.running_interval.is_none(), "A3");
    let pending = repo.pending_lifecycle().await.expect("A3 outbox");
    assert_eq!(pending.len(), 1, "A3 one interval fact");
    let closed = pending[0]
        .runtime_interval
        .as_ref()
        .expect("A3 typed interval");
    assert_eq!(closed.interval_id, first_interval.interval_id, "A3");
    assert!(closed.ended_at_unix_ms >= closed.started_at_unix_ms, "A3");

    let running = app
        .begin_activity("activity")
        .await
        .expect("A4 activity before terminal transition");
    let terminated = app
        .begin_archive(
            "activity",
            "2026-08-11T00:00:00Z",
            awaken_session_contract::ManagedLifecycleFact {
                id: "activity-terminal".into(),
                object_id: "activity".into(),
                workspace_id: Some("workspace".into()),
                event_type: "session.status_terminated".into(),
                timestamp: 1,
                runtime_interval: None,
            },
        )
        .await
        .expect("A4 terminal transition")
        .session;
    assert!(terminated.running_interval.is_none(), "A4/E5");
    let fenced = app
        .settle_activity("activity", running.activity_epoch)
        .await
        .expect("A4 terminal settlement is idempotent");
    assert_eq!(fenced, terminated, "A4");
    assert_eq!(
        app.begin_activity("activity").await,
        Err(SessionActivityError::Terminal),
        "A5"
    );

    let mut exhausted = persisted("activity-exhausted", false, "idle");
    exhausted.activity_epoch = u64::MAX;
    create(repo.as_ref(), exhausted).await;
    let exhausted_before = repo
        .get("activity-exhausted")
        .await
        .expect("A6 durable Session before admission");
    assert_eq!(
        app.begin_activity("activity-exhausted").await,
        Err(SessionActivityError::EpochExhausted),
        "A6"
    );
    assert_eq!(
        repo.get("activity-exhausted")
            .await
            .expect("A6 durable Session"),
        exhausted_before,
        "A6"
    );
    assert_eq!(
        app.begin_activity("missing").await,
        Err(SessionActivityError::NotFound),
        "A7"
    );

    create(
        repo.as_ref(),
        persisted("activity-preparing", false, "preparing"),
    )
    .await;
    assert_eq!(
        app.begin_activity("activity-preparing").await,
        Err(SessionActivityError::NotReady),
        "A8/E4"
    );
    let still_preparing = repo.get("activity-preparing").await.expect("A8 durable");
    assert_eq!(still_preparing.activity_epoch, 0, "A8/E4");
    assert_eq!(still_preparing.execution.as_str(), "preparing", "A8/E4");

    let mut worker_preparing = persisted("activity-worker-preparing", false, "preparing");
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) =
        &mut worker_preparing.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    create(repo.as_ref(), worker_preparing).await;
    let admitted = app
        .begin_activity("activity-worker-preparing")
        .await
        .expect("A10 Worker claim must be triggered by the driving event");
    assert_eq!(admitted.activity_epoch, 1, "A10/E6");
    assert_eq!(
        admitted.execution,
        SessionExecutionState::Preparing,
        "A10/E6"
    );

    let mut failed = still_preparing;
    failed.execution = SessionExecutionState::ActivationFailed;
    let failed = app
        .commit_session_snapshot(
            "workspace",
            failed,
            "activity-test-realization-failed",
            Vec::new(),
        )
        .await
        .expect("A9 terminal realization");
    assert_eq!(
        app.settle_activity("activity-preparing", 0)
            .await
            .expect("A9 stale settlement"),
        failed,
        "A9/E3"
    );
}

/// Message-execution FMECA and cause/effect graph. Failure modes are FM1 a
/// successful Runtime step leaves the Session running, FM2 a Runtime error
/// skips settlement, FM3 admission failure invokes Runtime anyway. Causes: C1
/// Session is idle, C2 Runtime succeeds, C3 Runtime fails after admission, C4
/// Session is terminal before admission. Effects: E1 one epoch and idle
/// settlement with a step, E2 one epoch and idle settlement with the original
/// error, E3 no Runtime effect and terminal truth unchanged. Cause graph:
/// C1&&C2 -> E1; C1&&C3 -> E2; C4 -> E3.
///
/// | Rule | Idle | Runtime | Terminal | Effect |
/// |---|---|---|---|---|
/// | M1 | yes | success | no | E1 |
/// | M2 | yes | error | no | E2 |
/// | M3 | no | not called | yes | E3 |
#[tokio::test]
async fn session_message_execution_always_settles_its_activity() {
    let success_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("M1 repository"),
    );
    create(
        success_repo.as_ref(),
        persisted("message-success", false, "idle"),
    )
    .await;
    let success = application_with_runtime(
        Arc::new(SuccessfulRuntime),
        success_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .run_session_message(
        "agent",
        "message-success",
        vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "go",
        )],
        None,
        Arc::new(DiscardProgress),
    )
    .await
    .expect("M1 successful message");
    assert_eq!(success.session.execution, SessionExecutionState::Idle, "M1");
    assert_eq!(success.session.activity_epoch, 1, "M1");

    let failure_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("M2 repository"),
    );
    create(
        failure_repo.as_ref(),
        persisted("message-failure", false, "idle"),
    )
    .await;
    let failure = application(
        failure_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .run_session_message(
        "agent",
        "message-failure",
        vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "go",
        )],
        None,
        Arc::new(DiscardProgress),
    )
    .await;
    assert!(failure.is_err(), "M2");
    let settled = failure_repo.get("message-failure").await.expect("M2 state");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "M2");
    assert_eq!(settled.activity_epoch, 1, "M2");

    let mut terminal = persisted("message-terminal", false, "idle");
    terminal.execution = SessionExecutionState::Terminated;
    create(failure_repo.as_ref(), terminal).await;
    let terminal_before = failure_repo
        .get("message-terminal")
        .await
        .expect("M3 initial state");
    let rejected = application(
        failure_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .run_session_message(
        "agent",
        "message-terminal",
        Vec::new(),
        None,
        Arc::new(DiscardProgress),
    )
    .await;
    assert!(rejected.is_err(), "M3");
    assert_eq!(
        failure_repo
            .get("message-terminal")
            .await
            .expect("M3 state"),
        terminal_before,
        "M3"
    );
}

/// Update-admission authority graph. C1 durable status is idle; C2 durable
/// status is running/terminal; C3 an interface cache is absent or stale.
/// Only C1 permits mutation (E1); C2 always rejects without a root revision
/// change (E2), independently of C3. This prevents another process's wire
/// projection from becoming a parallel lifecycle authority.
///
/// | Rule | Durable status | Wire cache | Effect |
/// |---|---|---|---|
/// | U1 | idle | any/absent | apply through root CAS |
/// | U2 | running | any/absent | reject, no mutation |
/// | U3 | terminal | any/absent | reject, no mutation |
#[tokio::test]
async fn update_admission_uses_only_durable_session_status() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("update-idle", false, "idle")).await;
    create(repo.as_ref(), persisted("update-running", false, "running")).await;
    create(
        repo.as_ref(),
        persisted("update-terminal", false, "terminated"),
    )
    .await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = |title: &str| SessionUpdateCommand {
        title: Some(Some(title.into())),
        metadata: None,
        budget: None,
        tools: None,
        mcp_candidates: None,
        idempotency_key: None,
        request_fingerprint: awaken_session_contract::stable_fingerprint(&title),
        if_match: None,
    };

    let updated = app
        .update_session("update-idle", command("accepted"))
        .await
        .expect("U1");
    assert_eq!(updated.session.title.as_deref(), Some("accepted"), "U1");

    for (rule, id) in [("U2", "update-running"), ("U3", "update-terminal")] {
        let before = repo.get(id).await.expect(rule);
        assert!(
            matches!(
                app.update_session(id, command("rejected")).await,
                Err(SessionUpdateError::NotIdle)
            ),
            "{rule}"
        );
        assert_eq!(repo.get(id).await.expect(rule), before, "{rule}");
    }
}

#[tokio::test]
async fn budget_update_lifecycle_follows_the_one_way_decision_table() {
    // Cause/effect graph: C1 budget absent/active/removed; C2 requested limit
    // absent/present; C3 a present limit is greater than exact consumed cost.
    // Effects: E1 absent cannot acquire a budget; E2 active+C3 changes the cap;
    // E3 active+!C3 rejects; E4 active+removal preserves snapshot/cursor and
    // disables admission enforcement; E5 removed cannot become active again.
    // Decision table: B1 absent+present=>E1; B2 active+present+C3=>E2; B3
    // active+present+!C3=>E3; B4 active+absent=>E4; B5 removed+present=>E5.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut active = persisted("budget-active", false, "idle");
    let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::new(),
        runtime_rates: Default::default(),
        fingerprint: "prices-v1-fingerprint".into(),
    };
    active.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 10,
        consumed_numerator: 2
            * awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot,
        reached_event_emitted: false,
    };
    create(repo.as_ref(), active).await;
    create(repo.as_ref(), persisted("budget-absent", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = |budget| SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: Some(budget),
        tools: None,
        mcp_candidates: None,
        idempotency_key: None,
        request_fingerprint: awaken_session_contract::stable_fingerprint(&budget),
        if_match: None,
    };

    assert!(
        matches!(
            app.update_session("budget-absent", command(Some(3))).await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B1/E1"
    );
    let updated = app
        .update_session("budget-active", command(Some(3)))
        .await
        .expect("B2");
    assert_eq!(
        updated.session.budget.max_list_cost_minor(),
        Some(3),
        "B2/E2"
    );
    assert!(
        matches!(
            app.update_session("budget-active", command(Some(2))).await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B3/E3"
    );
    let removed = app
        .update_session("budget-active", command(None))
        .await
        .expect("B4");
    assert!(
        matches!(
            removed.session.budget,
            awaken_session_contract::SessionBudgetState::Removed { .. }
        ),
        "B4/E4"
    );
    assert!(removed.session.budget.can_admit_model_request(), "B4/E4");
    assert!(
        matches!(
            app.update_session("budget-active", command(Some(4))).await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B5/E5"
    );
}

/// Cause/effect graph: C1 the Runtime presents the exact durable realization
/// lease; C2 the binding is new or an idempotent replay; C3 the same epoch
/// has been renewed monotonically; C4 a replacement owner/epoch has fenced
/// the Runtime. C1 permits the ordinary root CAS; C3 preserves in-flight
/// work admitted before renewal; C4 rejects a binding change while an equal
/// durable binding remains a side-effect-free recovery replay.
///
/// | Rule | Asserted lease | Binding | Effect |
/// |---|---|---|---|
/// | B1 | exact | new | persist once |
/// | B2 | exact | equal | idempotent success |
/// | B3 | shorter same-epoch assertion under live renewal | new/equal | authorized |
/// | B4 | stale owner/epoch | equal | idempotent success, no mutation |
/// | B5 | stale owner/epoch | different | fenced, no mutation |
/// | B6 | aggregate/assertion both absent | new/equal | legacy CAS path |
/// | B7 | current lease expired | equal | idempotent success, no mutation |
/// | B8 | current lease expired | different | fenced, no mutation |
#[tokio::test]
async fn environment_binding_persistence_is_fenced_by_exact_realization() {
    fn receipt(
        session_id: &str,
        binding: &str,
        realization: Option<&awaken_session_contract::SessionRealizationLease>,
    ) -> awaken_session_contract::SessionEnvironmentReceipt {
        awaken_session_contract::SessionEnvironmentReceipt::new(
            session_id,
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            binding,
            realization.cloned(),
        )
    }
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = persisted("binding-fence", false, "idle");
    let current = awaken_session_contract::SessionRealizationLease {
        owner: "runtime-a".into(),
        runtime_incarnation: "runtime-a/boot-1".into(),
        epoch: 3,
        expires_at_unix_ms: u64::MAX,
    };
    session.realization = Some(current.clone());
    create(repo.as_ref(), session).await;
    let sink = RepositoryEnvironmentBindingSink::new(repo.clone());

    sink.persist(receipt("binding-fence", "sandbox-a", Some(&current)))
        .await
        .expect("B1");
    sink.persist(receipt("binding-fence", "sandbox-a", Some(&current)))
        .await
        .expect("B2");
    let mut renewed_session = persisted("binding-renewed", false, "idle");
    renewed_session.realization = Some(awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: u64::MAX,
        ..current.clone()
    });
    create(repo.as_ref(), renewed_session).await;
    let admitted_before_renewal = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: u64::MAX - 1,
        ..current.clone()
    };
    sink.persist(receipt(
        "binding-renewed",
        "sandbox-renewed",
        Some(&admitted_before_renewal),
    ))
    .await
    .expect("B3 monotonic renewal authorizes admitted work");
    create(
        repo.as_ref(),
        persisted("binding-unassigned", false, "idle"),
    )
    .await;
    sink.persist(receipt("binding-unassigned", "sandbox-legacy", None))
        .await
        .expect("B6");
    let stale = awaken_session_contract::SessionRealizationLease {
        owner: "runtime-b".into(),
        runtime_incarnation: "runtime-b/boot-1".into(),
        epoch: 4,
        expires_at_unix_ms: u64::MAX,
    };
    let revision_before_stale_replay = repo.get("binding-fence").await.expect("B4").revision;
    assert_eq!(
        sink.persist(receipt("binding-fence", "sandbox-a", Some(&stale)))
            .await
            .expect_err("B4 stale equal-binding replay is fenced")
            .code,
        "session_realization_stale",
        "B4"
    );
    assert_eq!(
        repo.get("binding-fence").await.expect("B4").revision,
        revision_before_stale_replay,
        "B4 must not write"
    );
    let error = sink
        .persist(receipt("binding-fence", "sandbox-b", Some(&stale)))
        .await
        .expect_err("B5 stale binding replacement is fenced");
    assert_eq!(error.code, "session_realization_stale", "B5");
    let expired = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: 0,
        ..current.clone()
    };
    let mut expired_session = repo.get("binding-fence").await.expect("B6 fixture");
    expired_session.realization = Some(expired.clone());
    let expected_revision = expired_session.revision;
    let payload = awaken_session_contract::SessionMutationPayload::Replace(expired_session);
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repo.commit_mutation(
            "workspace",
            awaken_session_contract::SessionMutation {
                expected_revision,
                idempotency: awaken_session_contract::IdempotencyRecord {
                    key: "binding-fence:expire".into(),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            },
        )
        .await
        .expect("B7 fixture mutation"),
        awaken_session_contract::SessionMutationResult::Applied { .. }
    ));
    assert_eq!(
        sink.persist(receipt("binding-fence", "sandbox-a", Some(&expired)))
            .await
            .expect_err("B7 expired equal-binding replay is fenced")
            .code,
        "session_realization_stale",
        "B7"
    );
    assert_eq!(
        sink.persist(receipt("binding-fence", "sandbox-b", Some(&expired)))
            .await
            .expect_err("B8")
            .code,
        "session_realization_stale",
        "B8"
    );
    assert_eq!(
        repo.get("binding-fence")
            .await
            .expect("binding-fence Session")
            .environment
            .binding()
            .map(str::to_owned)
            .as_deref(),
        Some("sandbox-a"),
        "B4/B5/B7/B8"
    );
}

/// In-flight renewal cause/effect graph: C1 one MCP generation is Realizing
/// under an admitted lease; C2 the same owner/incarnation/epoch is durably
/// extended before its Stage receipt returns. E1 accepts the old exact
/// receipt under the monotonic lease fence; E2 activates the durable
/// generation but returns one renewal Stage; E3 the renewal receipt leads
/// to Publish with the extended generation. Different owner/epoch is A2 in
/// the contract table; conflicting receipt bindings remain rejected by the
/// canonical receipt verification gate.
///
/// | Rule | C1 | C2 | First effect | Next effect |
/// |---|---|---|---|---|
/// | F1 | yes | yes | E1 + E2, no publish | E3 |
#[tokio::test]
async fn activation_restages_a_generation_renewed_while_its_stage_was_in_flight() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let admitted_expiry = now + 60_000;
    let renewed_expiry = now + 120_000;
    let asserted_lease = awaken_session_contract::SessionRealizationLease {
        owner: "runtime-a".into(),
        runtime_incarnation: "runtime-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: admitted_expiry,
    };
    let current_lease = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: renewed_expiry,
        ..asserted_lease.clone()
    };
    let mut session = persisted("in-flight-renewal", false, "activating");
    session.realization = Some(current_lease.clone());
    session.mcp = awaken_session_contract::SessionMcpAttachmentSet::from_initial(
        vec![awaken_session_contract::McpAttachmentDraft {
            name: "browser".into(),
            target: McpTarget::parse_http("https://browser.example.test/mcp").unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin: awaken_session_contract::McpAttachmentOrigin::Session,
        }],
        None,
    )
    .unwrap();
    let attachment_id = session.mcp.attachments[0].attachment_id.clone();
    session
        .mcp
        .claim_realization(
            &attachment_id,
            awaken_session_contract::McpGeneration(1),
            awaken_session_contract::McpRealizationClaim {
                realization_id: "realization-1".into(),
                runtime_incarnation: asserted_lease.runtime_incarnation.clone(),
                lease_epoch: asserted_lease.epoch,
                lease_expires_at_unix_ms: admitted_expiry,
                stage_idempotency_key: "stage-1".into(),
            },
        )
        .unwrap();
    let admitted_request = projection::stage_mcp_request(
        "workspace",
        &session.session_id,
        &session.mcp.attachments[0],
    )
    .unwrap();
    create(repo.as_ref(), session).await;
    let app = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let admitted_receipt = awaken_session_contract::McpRealizationReceipt {
        receipt_fingerprint: admitted_request.fingerprint(),
        generation: admitted_request.generation.clone(),
        realization_id: admitted_request.realization_id.clone(),
        selected_plaintext_holder: admitted_request.selected_plaintext_holder.clone(),
        actual_realization_kind: None,
    };

    let directive =
        awaken_session_contract::SessionRealizationControl::activate_session_realization(
            &app,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "in-flight-renewal".into(),
                lease: asserted_lease,
                prepared_resource_revision: None,
                mcp_receipts: vec![admitted_receipt],
            },
        )
        .await
        .expect("F1/E1");
    let awaken_session_contract::SessionRealizationAction::Stage {
        prepare_session,
        mut mcp_stages,
    } = directive.action
    else {
        panic!("F1/E2 must restage the extended exact fence before publish")
    };
    assert!(!prepare_session, "F1/E2 does not recreate the Environment");
    let renewed_request = mcp_stages.remove(0);
    assert_eq!(
        renewed_request.renewal_binding_fingerprint(),
        admitted_request.renewal_binding_fingerprint(),
        "F1/E2"
    );
    assert_eq!(
        renewed_request.generation.lease_expires_at_unix_ms, renewed_expiry,
        "F1/E2"
    );
    let renewed_receipt = awaken_session_contract::McpRealizationReceipt {
        receipt_fingerprint: renewed_request.fingerprint(),
        generation: renewed_request.generation.clone(),
        realization_id: renewed_request.realization_id,
        selected_plaintext_holder: renewed_request.selected_plaintext_holder,
        actual_realization_kind: None,
    };
    let directive =
        awaken_session_contract::SessionRealizationControl::activate_session_realization(
            &app,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "in-flight-renewal".into(),
                lease: current_lease,
                prepared_resource_revision: None,
                mcp_receipts: vec![renewed_receipt],
            },
        )
        .await
        .expect("F1/E3");
    let awaken_session_contract::SessionRealizationAction::Publish { publish, .. } =
        directive.action
    else {
        panic!("F1/E3 must publish after exact renewal restage")
    };
    assert_eq!(publish, vec![renewed_request.generation], "F1/E3");
}

#[test]
fn lifecycle_supervisor_claim_is_one_shot() {
    // Cause/effect decision table: C1=unclaimed fence, C2=already claimed.
    // R1 C1 -> E1 first caller becomes owner; R2 C2 -> E2 every later caller
    // is rejected. This proves moving the fence out of the protocol adapter
    // cannot start parallel Session lifecycle supervisors.
    let fence = AtomicBool::new(false);
    assert!(claim_once(&fence), "R1");
    assert!(!claim_once(&fence), "R2");
}

#[test]
fn native_only_lazy_provisioning_decision_table() {
    // Causes: C1 eager policy; C2 lazy policy; C3 implicit/native backend;
    // C4 ACP/A2A backend. Effects: E1 accept; E2 reject before Session
    // realization. Environment policy tests own disabled/exact-version rules.
    //
    // | Rule | policy | runtime | effect |
    // | R1 | eager | any | accept |
    // | R2 | lazy | implicit/native | accept |
    // | R3 | lazy | ACP/A2A | reject |
    use SandboxProvisioning::{Eager, OnToolUse};
    for (case, provisioning, runtime, accepted) in [
        ("R1 eager native", Eager, None, true),
        ("R1 eager ACP", Eager, Some("acp:claude"), true),
        ("R2 lazy implicit native", OnToolUse, None, true),
        ("R2 lazy explicit native", OnToolUse, Some("awaken"), true),
        ("R2 lazy genai native", OnToolUse, Some("genai"), true),
        (
            "R2 lazy custom native",
            OnToolUse,
            Some("provider-native"),
            true,
        ),
        ("R3 lazy ACP", OnToolUse, Some("acp:claude"), false),
        (
            "R3 lazy A2A",
            OnToolUse,
            Some("a2a:https://agent.example"),
            false,
        ),
    ] {
        assert_eq!(
            validate_sandbox_provisioning_runtime(provisioning, runtime).is_ok(),
            accepted,
            "{case}"
        );
    }
}

#[tokio::test]
async fn durable_session_truth_owns_one_work_projection_path() {
    // Cause/effect graph: C1 frozen Environment is self-hosted; C2 Session is
    // nonterminal; C3 the projection command is replayed. Effects: E1 only
    // C1+C2 dispatches;
    // E2 replay uses the same idempotent port and creates no second identity.
    //
    // | Rule | self-hosted | terminal | replay | effect |
    // | R1 | yes | no | no | project one |
    // | R2 | yes | no | yes | retain one |
    // | R3 | no | no | any | skip |
    // | R4 | yes | yes | any | skip |
    let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
        .expect("session repository");
    create(&repo, persisted("external", true, "idle")).await;
    create(&repo, persisted("local", false, "idle")).await;
    create(&repo, persisted("terminal", true, "terminated")).await;
    let environments = RecordingEnvironmentSource::default();

    let first = reconcile_work_dispatches(&repo, &environments).await;
    assert_eq!(first.settled, 1, "R1/R3/R4");
    assert!(first.failures.is_empty());
    assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R1");

    let replay = reconcile_work_dispatches(&repo, &environments).await;
    assert_eq!(replay.settled, 1, "R2");
    assert!(replay.failures.is_empty());
    assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R2");
}

#[tokio::test]
async fn work_dispatch_reconciliation_isolates_each_session_failure() {
    // Work-projection FMECA decision table. Causes: C1 a durable Session needs
    // external work; C2 its queue write succeeds; C3 a sibling queue write
    // fails; C4 an unrelated Session needs no dispatch. Effects: E1 every
    // eligible Session is attempted; E2 successes settle independently; E3
    // failures retain exact Session/Environment diagnostics for later retry;
    // E4 ineligible Sessions cause no side effect. Rules: W1 C1+C2=>E1+E2;
    // W2 C1+C3=>E1+E3 without aborting W1; W3 C4=>E4.
    let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
        .expect("session repository");
    create(&repo, persisted("external-failed", true, "idle")).await;
    create(&repo, persisted("external-settled", true, "idle")).await;
    create(&repo, persisted("local-skip", false, "idle")).await;
    let environments = RecordingEnvironmentSource::default();
    environments.fail_for("external-failed");

    let report = reconcile_work_dispatches(&repo, &environments).await;
    assert_eq!(report.settled, 1, "W1/W2");
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["external-settled".to_string()],
        "W1/W3"
    );
    assert_eq!(report.failures.len(), 1, "W2");
    assert_eq!(report.failures[0].session_id, "external-failed", "W2");
    assert_eq!(report.failures[0].environment_id, "env-worker", "W2");
    assert!(
        report.failures[0].message.contains("injected failure"),
        "W2 preserves the retry diagnostic"
    );
}
