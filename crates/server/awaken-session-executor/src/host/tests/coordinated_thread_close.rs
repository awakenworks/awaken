use super::*;

#[tokio::test]
async fn failed_child_boundary_cancels_raced_follow_up_without_fencing_its_own_settlement() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the current coordinated child dispatch still owns
    // its lease but its Run has committed Failed; C2 a later follow-up for the
    // same logical Thread was admitted just before that failure became visible;
    // C3 the failed boundary invokes the existing Thread interrupt path. Effects:
    // E1 committed Run truth closes future admission; E2 C3 leaves the already
    // terminal current claim untouched; E3 C3 durably marks the queued follow-up
    // cancelled; E4 current settlement succeeds, then canonical cancellation
    // settlement consumes the raced row. No Thread terminal flag is stored.
    //
    // | Rule | Current Run | Later row | Interrupt | Effects |
    // |---|---|---|---|---|
    // | R1 | Failed + Leased | Pending | no | E1 only |
    // | R2 | Failed + Leased | Pending | yes | E1+E2+E3 |
    // | R3 | R2 | cancel claim | settled | E4 |
    use awaken_agent_contract::agent::run::Failure;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch::{DispatchOutcome, RunDispatch};

    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .expect("failed follow-up race dispatch"),
    );
    let host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_coordinator_dispatch_store(dispatch.clone());
    let parent = ThreadId("failed-race-parent".into());
    let child = ThreadId("failed-race-child".into());
    let failed_run = RunId("failed-race-current".into());
    let follow_up_run = RunId("failed-race-follow-up".into());
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &child.0,
                &failed_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(21),
        )
        .await
        .expect("R1 current admission");
    clock.set(0);
    let current_claim = dispatch
        .claim("failed-race-worker", 100, &Default::default())
        .await
        .expect("R1 claim")
        .expect("R1 current work");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &child.0,
                &follow_up_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(22),
        )
        .await
        .expect("R1 raced later admission");
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("parent partition");
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(failed_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R1 Running truth");
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::ended(failed_run.clone(), EndCause::Error(Failure::StateConflict)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R1 Failed truth");
    assert!(
        host.coordinated_thread_is_closed(
            &parent.0,
            &child,
            awaken_session_execution_contract::CoordinatedThreadKind::Agent
        )
        .await
        .expect("R1 admission fence"),
        "R1/E1"
    );

    host.interrupt_session_thread(&parent.0, &child)
        .await
        .expect("R2 boundary interruption");
    let rows = dispatch.list_dispatches().await.expect("R2 inspect rows");
    let current = rows
        .iter()
        .find(|row| row.run_id == failed_run)
        .expect("R2 current row");
    let raced = rows
        .iter()
        .find(|row| row.run_id == follow_up_run)
        .expect("R2 raced row");
    assert!(!current.cancellation_requested, "R2/E2");
    assert!(raced.cancellation_requested, "R2/E3");

    let settled = dispatch
        .settle(
            &failed_run,
            current_claim.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("R3 current settlement");
    assert!(settled.applied(), "R3/E4 current claim was not fenced");
    clock.set(0);
    let cancellation = dispatch
        .claim("failed-race-worker", 100, &Default::default())
        .await
        .expect("R3 cancellation claim")
        .expect("R3 cancellation work");
    assert_eq!(cancellation.request.run_id(), &follow_up_run, "R3/E4");
    assert!(cancellation.cancellation_requested, "R3/E4");
    dispatch
        .settle(
            &follow_up_run,
            cancellation.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("R3 cancellation settlement");
}

#[tokio::test]
async fn follow_up_close_fence_applies_the_contract_rule_for_each_thread_kind() {
    // Cause/effect graph: C1 coordinated Thread kind is an ordinary Agent or a
    // one-shot Advisor; C2 the latest committed Run in the parent partition is
    // Completed or Failed. Effects: E1 closed (follow-up rejected); E2 open.
    // The host reads the same contract close rule as the Managed projection,
    // so an Advisor consultation is not reusable here while it is Terminated
    // there.
    //
    // | Rule | Kind | Latest Run | Effect |
    // |---|---|---|---|
    // | K1 | Agent | Completed | E2 |
    // | K2 | Advisor | Completed | E1 |
    // | K3 | Agent | Failed | E1 |
    // | K4 | Advisor | Failed | E1 |
    use awaken_agent_contract::agent::run::Failure;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_session_execution_contract::CoordinatedThreadKind;

    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let parent = ThreadId("close-rule-parent".into());
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("parent partition");
    let rows = [
        (
            "K1",
            CoordinatedThreadKind::Agent,
            EndCause::NaturalEnd,
            false,
        ),
        (
            "K2",
            CoordinatedThreadKind::Advisor,
            EndCause::NaturalEnd,
            true,
        ),
        (
            "K3",
            CoordinatedThreadKind::Agent,
            EndCause::Error(Failure::StateConflict),
            true,
        ),
        (
            "K4",
            CoordinatedThreadKind::Advisor,
            EndCause::Error(Failure::StateConflict),
            true,
        ),
    ];
    for (row, kind, end, closed) in rows {
        let child = ThreadId(format!("close-rule-child-{row}"));
        let run = RunId(format!("close-rule-run-{row}"));
        for disposition in [
            RunDisposition::running(run.clone()),
            RunDisposition::ended(run.clone(), end.clone()),
        ] {
            commit
                .commit(ThreadCommit::assemble(
                    child.clone(),
                    disposition,
                    true,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ))
                .await
                .expect("committed child Run truth");
        }
        assert_eq!(
            host.coordinated_thread_is_closed(&parent.0, &child, kind)
                .await
                .expect("close fence"),
            closed,
            "{row}"
        );
    }
}
