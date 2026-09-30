use super::*;

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
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let mut session = persisted(&id, false, "idle");
        let epoch = SessionApplication::open_activity_on_snapshot(
            &mut session,
            None,
            false,
            false,
            1_000,
            7,
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
        create(repo.as_ref(), session).await;
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
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let mut session = persisted(&id, false, "idle");
        let epoch = SessionApplication::open_activity_on_snapshot(
            &mut session,
            None,
            false,
            false,
            1_000,
            7,
        )
        .unwrap();
        create(repo.as_ref(), session).await;
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
