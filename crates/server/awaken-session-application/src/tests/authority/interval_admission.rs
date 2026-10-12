use super::*;

#[test]
fn fresh_creation_requires_explicit_empty_activity_history_without_promoting_legacy() {
    // F1 untouched actual fresh constructor -> accepted; F2 only new history
    // omitted, with materialization still fresh -> rejected independently;
    // F3 anonymous/named admission already present -> not a fresh insert.
    // No historical field is repaired and no receipt replay uses this factory.
    let mut fresh = first_materialization::fresh_subject("history-create");
    fresh.execution = SessionExecutionState::Preparing;
    fresh.created_at_unix_ms = None;
    fresh.updated_at_unix_ms = None;
    crate::creation::CompleteSessionRoot::from_fresh(fresh.clone()).expect("F1");
    let mut legacy_wire = serde_json::to_value(&fresh).unwrap();
    legacy_wire
        .as_object_mut()
        .unwrap()
        .remove("run_activities");
    let legacy = serde_json::from_value(legacy_wire).unwrap();
    assert!(
        crate::creation::CompleteSessionRoot::from_fresh(legacy).is_err(),
        "F2"
    );
    for named in [false, true] {
        let mut admitted = fresh.clone();
        let thread = ThreadId("history-create".into());
        let run = RunId("already-admitted".into());
        assert!(admitted.begin_activity_epoch_at(1, named.then_some(("op", &thread, &run))));
        assert!(
            crate::creation::CompleteSessionRoot::from_fresh(admitted).is_err(),
            "F3"
        );
    }
}

#[tokio::test]
async fn named_activity_cas_replay_retains_original_raw_identity_before_first_create() {
    // Causes: C1 new named admission before any Create; C2 pre-CAS failure;
    // C3 competing exact callers plus a committed-response loss; C4 same op
    // with wrong Thread/Run; C5 same Run, new operation and epoch; C6 terminal
    // state after commit. Effects: R1 C2 leaves the complete Root unchanged;
    // R2 C1+C3 stores one original raw record and both callers replay it;
    // R3 C4 conflicts without a CAS; R4 C5 appends, never rewrites the first;
    // R5 C6 exact receipt wins before current admission policy. The raw record
    // is not a physical domain/stop grant; there is only one real SQLite CAS.
    let id = SessionId::from("named-activity-race");
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    create(
        durable.as_ref(),
        first_materialization::fresh_subject(id.as_str()),
    )
    .await;
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(200));
    let app = Arc::new(
        application(
            faulting.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        )
        .with_clock(clock.clone()),
    );
    let thread = ThreadId(id.to_string());
    let run = RunId("same-original-run".into());
    let original = durable.get(&id).await.unwrap();
    assert!(
        original.environment_materialization().unwrap().is_empty(),
        "C1 no Create yet"
    );
    faulting.fail_once("managed:session-activity:");
    assert!(
        app.begin_activity_for_operation_with_policy(
            &id,
            "op-first",
            false,
            None,
            None,
            Some((&thread, &run))
        )
        .await
        .is_err(),
        "R1 outage"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        original,
        "R1 raw fact and activity are atomic"
    );

    let pause = faulting.pause_next_mutation("managed:session-activity:");
    let delayed = {
        let app = app.clone();
        let id = id.clone();
        let thread = thread.clone();
        let run = run.clone();
        tokio::spawn(async move {
            app.begin_activity_for_operation_with_policy(
                &id,
                "op-first",
                false,
                None,
                None,
                Some((&thread, &run)),
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        durable.get(&id).await.unwrap(),
        original,
        "R2 no admission before CAS"
    );
    let candidate = pause.candidate.lock().unwrap().clone().unwrap();
    let awaken_session_state_contract::SessionMutationPayload::Replace(detached) =
        &candidate.payload
    else {
        panic!("activity uses existing Root replacement")
    };
    assert_eq!(
        detached.run_activities().unwrap().0.len(),
        1,
        "R2 record in same candidate"
    );
    faulting.commit_then_conflict_once("managed:session-activity:");
    let (winner, epoch) = app
        .begin_activity_for_operation_with_policy(
            &id,
            "op-first",
            false,
            None,
            None,
            Some((&thread, &run)),
        )
        .await
        .unwrap();
    pause.release.notify_one();
    let (loser, losing_epoch) = tokio::time::timeout(std::time::Duration::from_secs(5), delayed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(losing_epoch, epoch, "R2 original receipt epoch");
    assert_eq!(loser, winner, "R2 durable winner, not detached candidate");
    let (records, unknown) = winner.run_activities().unwrap();
    assert!(!unknown);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation_id(), "op-first");
    assert_eq!(records[0].activity_epoch().get(), epoch);
    assert_eq!(records[0].thread_id(), &thread);
    assert_eq!(records[0].run_id(), &run);
    let wrong_thread = ThreadId("different-thread".into());
    let wrong_run = RunId("different-run".into());
    for source in [(&wrong_thread, &run), (&thread, &wrong_run)] {
        assert_eq!(
            app.begin_activity_for_operation_with_policy(
                &id,
                "op-first",
                false,
                None,
                None,
                Some(source)
            )
            .await
            .unwrap_err(),
            SessionActivityError::Conflict,
            "R3 original receipt cannot bless different raw coordinates"
        );
        assert_eq!(durable.get(&id).await.unwrap(), winner, "R3 no CAS");
    }
    let (continued, next_epoch) = app
        .transfer_committed_activity_for_operation(&id, "op-continue", Some(epoch), (&thread, &run))
        .await
        .unwrap();
    assert!(next_epoch > epoch, "R4 same Run gets new operation epoch");
    assert_eq!(
        continued.active_activity_epochs,
        BTreeSet::from([next_epoch])
    );
    let retained = continued.run_activities().unwrap().0.to_vec();
    assert_eq!(retained.len(), 2);
    assert_eq!(retained[0], winner.run_activities().unwrap().0[0]);
    assert_eq!(retained[1].run_id(), &run);
    clock.set(1000);
    let mut terminal = SessionAggregate::try_from(continued).unwrap();
    terminal
        .lifecycle()
        .transition_execution_at(SessionExecutionState::Terminated, 1000)
        .unwrap();
    let terminal = app
        .commit_session_snapshot_with_outcome(
            "workspace",
            terminal,
            "test-terminal-activity",
            Vec::new(),
        )
        .await
        .unwrap()
        .into_session();
    assert!(terminal.is_terminal());
    let (replayed, original_epoch) = app
        .begin_activity_for_operation_with_policy(
            &id,
            "op-first",
            false,
            None,
            None,
            Some((&thread, &run)),
        )
        .await
        .unwrap();
    assert_eq!(original_epoch, epoch, "R5 receipt before terminal policy");
    assert_eq!(replayed, terminal, "R5 no reopened activity");
    assert_eq!(replayed.run_activities().unwrap().0, retained);
}

#[tokio::test]
async fn anonymous_and_legacy_activity_receipts_never_backfill_named_history() {
    // Decision table: A1 fresh+anonymous successful open -> sticky unknown,
    // empty named records; A2 legacy missing+named open -> still missing;
    // A3 exact receipt read/retry supplies raw coordinates only now -> no new
    // historical record and no CAS. Failed/terminal opens stutter in the state
    // table. This tests legacy/fresh distinction without inventing old effects.
    for legacy in [false, true] {
        let id = SessionId::from(if legacy {
            "legacy-raw-activity"
        } else {
            "anonymous-raw-activity"
        });
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let subject = if legacy {
            persisted(id.as_str(), false, "idle")
        } else {
            first_materialization::fresh_subject(id.as_str())
        };
        create(repo.as_ref(), subject).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let thread = ThreadId(id.to_string());
        let run = RunId("run-now".into());
        let (admitted, epoch) = if legacy {
            app.begin_activity_for_operation_with_policy(
                &id,
                "op-old",
                false,
                None,
                None,
                Some((&thread, &run)),
            )
            .await
            .unwrap()
        } else {
            app.begin_activity_for_operation(&id, "op-old")
                .await
                .unwrap()
        };
        if legacy {
            assert!(admitted.run_activities().is_none(), "A2");
        } else {
            assert_eq!(admitted.run_activities(), Some((&[][..], true)), "A1");
        }
        let recovered = app
            .recover_activity_for_operation(&id, "op-old", Some((&thread, &run)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            recovered,
            (admitted.clone(), epoch),
            "A3 receipt read never backfills"
        );
        let replayed = app
            .begin_activity_for_operation_with_policy(
                &id,
                "op-old",
                false,
                None,
                None,
                Some((&thread, &run)),
            )
            .await
            .unwrap();
        assert_eq!(replayed, recovered, "A3 exact retry never backfills");
        assert_eq!(repo.get(&id).await.unwrap(), admitted, "A3 no CAS");
    }
}

#[tokio::test]
async fn late_boundary_settles_activity_without_claiming_a_pre_admission_commit() {
    // Causes: A1 the Session root admits an activity after Runtime cursor 7;
    // A2 its exact epoch later settles with a boundary at 6, 7, or 8; A3 the
    // same settlement replays. Effects: E1 pre/equal boundaries settle the
    // activity but leave the closed interval unobserved; E2 post-admission 8
    // is retained as its exact close; E3 replay changes no revision or pair.
    // Rules R1=A1+A2(6/7)=>E1; R2=A1+A2(8)=>E2; R3=R1/R2+A3=>E3.
    // The Runtime commit fence is the source authority; a late acceptance of
    // already committed Awaiting cannot move an earlier Managed Usage cursor.
    for (source, expected_observations) in [(6, 0), (7, 0), (8, 1)] {
        let id = format!("late-boundary-{source}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let session = persisted(&id, false, "idle");
        let mut session = SessionAggregate::try_from(session).expect("valid admission fixture");
        let epoch = SessionApplication::open_activity_on_snapshot(
            &mut session,
            None,
            false,
            false,
            1_000,
            7,
            None,
        )
        .unwrap();
        assert_eq!(
            session
                .running_interval
                .as_ref()
                .unwrap()
                .admitted_after_commit_cursor,
            Some(7),
            "A1 durable admission fence"
        );
        create(repo.as_ref(), session.into_snapshot()).await;
        let app = application(repo, Arc::new(RecordingEnvironmentSource::default()));
        let observation = awaken_session_state_contract::SessionRuntimeIntervalObservation {
            activity_epoch: epoch,
            thread_id: ThreadId(id.clone()),
            run_id: RunId(format!("run-{source}")),
            lifecycle_cursor: awaken_agent_contract::encode_run_lifecycle_cursor(source, 0)
                .unwrap(),
            source_commit_cursor: source,
        };
        let settled = app
            .settle_activity_observed(
                &SessionId::from(id.as_str()),
                epoch,
                Some(observation.clone()),
            )
            .await
            .expect("R1/R2 exact admitted epoch settles");
        assert!(
            settled.active_activity_epochs.is_empty(),
            "R1/R2 activity removed"
        );
        assert!(settled.running_interval.is_none(), "R1/R2 interval closed");
        assert_eq!(settled.closed_runtime_intervals.len(), 1);
        assert_eq!(
            settled.closed_runtime_intervals[0].observations.len(),
            expected_observations,
            "R1/R2 causal interval ownership at source {source}"
        );
        let replay = app
            .settle_activity_observed(&SessionId::from(id.as_str()), epoch, Some(observation))
            .await
            .expect("R3 exact replay stutters");
        assert_eq!(
            replay.revision, settled.revision,
            "R3 unchanged CAS revision"
        );
        assert_eq!(
            replay.closed_runtime_intervals,
            settled.closed_runtime_intervals
        );
    }
}

#[tokio::test]
async fn user_event_boundary_before_admission_refuses_without_settlement() {
    // Causes: U1 an exact User Event activity is admitted after Runtime source
    // 7; U2 Dispatch reports a distinct boundary at 6 or 7. Effects: E1 the
    // root CAS refuses the boundary; E2 the admitted epoch and open interval
    // remain unchanged, so no processed receipt can be fabricated. Rules
    // R1=U1+U2(6), R2=U1+U2(7)=>E1+E2. The 8 successor is covered above by
    // the shared interval predicate and the normal Dispatch-settlement tests.
    for source in [6, 7] {
        let id = format!("user-pre-admission-{source}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let session = persisted(&id, false, "idle");
        let mut session = SessionAggregate::try_from(session).expect("valid admission fixture");
        let epoch = SessionApplication::open_activity_on_snapshot(
            &mut session,
            None,
            false,
            false,
            1_000,
            7,
            None,
        )
        .unwrap();
        create(repo.as_ref(), session.into_snapshot()).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let before = repo.get(&SessionId::from(id.as_str())).await.unwrap();
        let run = RunId(format!("run-{source}"));
        let observation = awaken_session_state_contract::SessionRuntimeIntervalObservation {
            activity_epoch: epoch,
            thread_id: ThreadId(id.clone()),
            run_id: run.clone(),
            lifecycle_cursor: awaken_agent_contract::encode_run_lifecycle_cursor(source, 0)
                .unwrap(),
            source_commit_cursor: source,
        };
        let error = app
            .settle_user_event_dispatch_observed(
                &SessionId::from(id.as_str()),
                epoch,
                observation,
                &run,
                awaken_session_execution_contract::SessionAgentRunBoundary::Awaiting,
            )
            .await
            .expect_err("R1/R2 cannot claim a pre-admission User Event commit");
        assert!(matches!(error, SessionActivityError::Unavailable(_)), "E1");
        let after = repo.get(&SessionId::from(id.as_str())).await.unwrap();
        assert_eq!(after.revision, before.revision, "E2 no CAS");
        assert_eq!(after.active_activity_epochs, before.active_activity_epochs);
        assert_eq!(after.running_interval, before.running_interval);
    }
}

#[tokio::test]
async fn transferred_activity_fences_a_late_distinct_boundary_before_conflict_validation() {
    // Cause/effect graph: C1 epoch A closes with observation X; C2 a reply
    // opens successor epoch B; C3 A's still-leased Worker reports later
    // observation Y. Effects: E1 C3 is a stale no-op, not epoch-reuse
    // corruption; E2 B remains the sole active epoch and its interval stays
    // open. Decision table: A1=C1=>closed(X); A2=C1+C2=>active(B);
    // A3=C1+C2+C3=>E1+E2. The active-epoch set is the fencing authority.
    let session_id = "stale-transferred-boundary";
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("activity fence repository"),
    );
    create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let first = app
        .begin_activity(&SessionId::from(session_id))
        .await
        .expect("A1 activity");
    let first_epoch = first.activity_epoch;
    let observation =
        |source_commit_cursor| awaken_session_state_contract::SessionRuntimeIntervalObservation {
            activity_epoch: first_epoch,
            thread_id: ThreadId(session_id.into()),
            run_id: RunId("shared-run".into()),
            lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(source_commit_cursor),
            source_commit_cursor,
        };
    app.settle_activity_observed(
        &SessionId::from(session_id),
        first_epoch,
        Some(observation(10)),
    )
    .await
    .expect("A1 closes with X");
    let successor = app
        .begin_activity(&SessionId::from(session_id))
        .await
        .expect("A2 successor");
    let successor_epoch = successor.activity_epoch;
    assert_ne!(successor_epoch, first_epoch, "A2 owns a new fence");

    let after_stale = app
        .settle_activity_observed(
            &SessionId::from(session_id),
            first_epoch,
            Some(observation(20)),
        )
        .await
        .expect("A3/E1 stale distinct boundary is fenced");
    assert_eq!(
        after_stale.active_activity_epochs,
        BTreeSet::from([successor_epoch]),
        "A3/E2"
    );
    assert_eq!(
        after_stale.execution,
        SessionExecutionState::Running,
        "A3/E2"
    );
    assert!(after_stale.running_interval.is_some(), "A3/E2");
    assert_eq!(after_stale.closed_runtime_intervals.len(), 1, "A3/E1");
}

#[tokio::test]
async fn cancellation_before_external_realization_settles_without_a_runtime_interval() {
    // Cause/effect graph: C1 an externally realized Session is Preparing; C2 a
    // Session Run activity is admitted during that stronger phase; C3 the Run
    // commits a cancellation boundary before realization opens Running. Effects:
    // E1 settlement removes the exact activity epoch; E2 Preparing remains
    // authoritative; E3 no synthetic open/closed Runtime interval or lifecycle
    // fact is created. Constraint: committed Run truth remains auditable in the
    // Runtime feed; this reducer owns only the customer-visible Session interval.
    // Decision rule P1=C1+C2+C3=>E1+E2+E3. Running-without-interval remains an
    // invalid historical boundary and is covered by the aggregate reducer tests.
    let session_id = "cancelled-before-external-realization";
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("pre-realization cancellation repository"),
    );
    let session = persisted(session_id, false, "preparing");
    let mut session = SessionAggregate::try_from(session).expect("valid admission fixture");
    let epoch = SessionApplication::open_activity_on_snapshot(
        &mut session,
        None,
        true,
        false,
        awaken_time::system_clock().now().as_epoch_ms(),
        0,
        None,
    )
    .expect("P1/C2 activity admitted during external realization");
    assert!(
        session.running_interval.is_none(),
        "P1/C1 no Running interval"
    );
    create(repo.as_ref(), session.into_snapshot()).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let observation = awaken_session_state_contract::SessionRuntimeIntervalObservation {
        activity_epoch: epoch,
        thread_id: ThreadId(session_id.into()),
        run_id: RunId("cancelled-before-realization-run".into()),
        lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(7),
        source_commit_cursor: 7,
    };

    let settled = app
        .settle_activity_observed(&SessionId::from(session_id), epoch, Some(observation))
        .await
        .expect("P1 cancellation boundary settles the admitted activity");
    assert!(settled.active_activity_epochs.is_empty(), "P1/E1");
    assert_eq!(settled.execution, SessionExecutionState::Preparing, "P1/E2");
    assert!(settled.running_interval.is_none(), "P1/E3");
    assert!(settled.closed_runtime_intervals.is_empty(), "P1/E3");
    assert!(
        repo.pending_lifecycle()
            .await
            .expect("P1 lifecycle outbox")
            .into_iter()
            .all(|fact| fact.event_type != "session.runtime_interval_closed"),
        "P1/E3"
    );
}
