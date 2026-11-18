// @formal evidence store_history.s3_publication_refines_committed_history
mod support;
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_dispatch::ObjectDispatchPartition;
use awaken_dispatch_contract::{
    ClaimAuthority, ClaimedCommitCommand, ClaimedRunCommit, DispatchClaims as _,
    DispatchCommitAuthority as _, DispatchEnqueue as _,
};
use awaken_worker_contract::RunClaim;
use std::sync::{Arc, atomic::Ordering};

async fn claimed() -> (
    Arc<support::FaultStore>,
    Arc<awaken_lease_testkit::ManualClock>,
    awaken_store_s3::S3Storage,
    awaken_store_s3::SessionStorage,
    Arc<ObjectDispatchPartition>,
    RunClaim,
) {
    let (objects, clock, engine, session) = support::setup().await;
    let dispatch = Arc::new(ObjectDispatchPartition::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "tenant".into(),
        "session".into(),
        clock.clone(),
    ));
    dispatch
        .enqueue(support::dispatch("run", "session", "session"))
        .await
        .unwrap();
    let claim = RunClaim::from(
        &dispatch
            .claim("worker", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    (objects, clock, engine, session, dispatch, claim)
}
fn command(claim: RunClaim) -> ClaimedCommitCommand {
    ClaimedCommitCommand {
        claim,
        operation: support::operation("run", "session", 0, 0, "canonical"),
    }
}
#[tokio::test]
async fn canonical_writer_shares_postgres_replay_and_error_laws() {
    let (_, _, engine, session, dispatch, claim) = claimed().await;
    awaken_store_conformance::claimed_commit::exact_replay_and_rejections(
        &engine.canonical_commits(dispatch),
        &session,
        command(claim),
    )
    .await;
}
#[tokio::test]
async fn canonical_writer_shares_postgres_concurrent_operation_laws() {
    let (_, _, engine, session, dispatch, claim) = claimed().await;
    awaken_store_conformance::claimed_commit::concurrent_operation_has_one_winner(
        &engine.canonical_commits(dispatch),
        &session,
        command(claim),
    )
    .await;
}
#[tokio::test]
async fn canonical_writer_shares_postgres_takeover_laws() {
    let (_, clock, engine, session, dispatch, claim) = claimed().await;
    let stale = command(claim);
    clock.advance(1000);
    let successor = RunClaim::from(
        &dispatch
            .claim("replacement", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let mut current = stale.clone();
    current.claim = successor;
    awaken_store_conformance::claimed_commit::stale_claim_cannot_publish(
        &engine.canonical_commits(dispatch),
        &session,
        stale,
        current,
    )
    .await;
}
#[tokio::test]
async fn takeover_between_staging_and_cas_fences_a_prepared_operation() {
    let (objects, clock, engine, session, dispatch, claim) = claimed().await;
    let source = engine.canonical_commits(dispatch.clone());
    objects.hold_next_head.store(true, Ordering::SeqCst);
    let old = command(claim);
    let writing = tokio::spawn(async move {
        <_ as ClaimedRunCommit<awaken_runtime_contract::runs::RunError>>::commit_operation(
            &source, old,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        objects.head_entered.notified(),
    )
    .await
    .expect("prepared root mutation must reach the held head CAS");
    clock.advance(1000);
    let successor = RunClaim::from(
        &dispatch
            .claim("replacement", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    objects.head_resume.notify_one();
    assert!(writing.await.unwrap().is_err());
    assert_eq!(session.head().await.unwrap().commit_sequence, 0);
    let receipt =
        <_ as ClaimedRunCommit<awaken_runtime_contract::runs::RunError>>::commit_operation(
            &engine.canonical_commits(dispatch),
            command(successor),
        )
        .await
        .unwrap();
    assert_eq!(receipt.commit_sequence, 1);
}
#[tokio::test]
async fn conditional_checkpoint_retirement_and_takeover_preserve_one_authority() {
    let (_, clock, engine, session, dispatch, claim) = claimed().await;
    let checkpoint = engine.canonical_checkpoints(claim.clone(), dispatch.clone());
    let value = StreamCheckpoint {
        run_id: "run".into(),
        thread_id: "session".into(),
        model: "model".into(),
        partial_text: "partial".into(),
        partial_tools: vec![],
        retry_count: 1,
    };
    checkpoint.put(value.clone()).await.unwrap();
    assert!(matches!(checkpoint.get("run").await, Observed::Present(_)));
    let writer = engine.canonical_commits(dispatch.clone());
    <_ as ClaimedRunCommit<awaken_runtime_contract::runs::RunError>>::commit_operation(
        &writer,
        command(claim.clone()),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            session.streams().checkpoint("run").await,
            awaken_agent_contract::observation::Observed::Absent
        ),
        "commit and checkpoint retirement share one head CAS"
    );
    clock.advance(1000);
    let successor = RunClaim::from(
        &dispatch
            .claim("replacement", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let current = engine.canonical_checkpoints(successor, dispatch);
    current.put(value).await.unwrap();
    assert!(matches!(
        checkpoint.get("run").await,
        Observed::Unknown(UnknownCause::Fenced(_))
    ));
    assert!(matches!(
        checkpoint.delete("run").await,
        Err(StreamCheckpointError::Fenced(_))
    ));
    assert!(matches!(current.get("run").await, Observed::Present(_)));
}
#[tokio::test]
async fn identical_rows_in_another_engine_cannot_accept_foreign_conditions() {
    let (objects, clock, engine, session, dispatch, claim) = claimed().await;
    let ClaimAuthority::Conditional(authority) = dispatch
        .acquire_commit_authority(&claim)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected conditional authority")
    };
    let foreign =
        awaken_store_s3::S3Storage::new(objects, "other-engine", clock.clone(), Default::default())
            .unwrap();
    let other_session = foreign.session(session.key().clone());
    other_session.create().await.unwrap();
    let other_queue = ObjectDispatchPartition::new(
        Arc::new(foreign.component_store("dispatch").unwrap()),
        "tenant".into(),
        "session".into(),
        clock,
    );
    other_queue
        .enqueue(support::dispatch("run", "session", "session"))
        .await
        .unwrap();
    let same_claim = RunClaim::from(
        &other_queue
            .claim("worker", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    assert_eq!(same_claim, claim);
    assert!(
        other_session
            .commit_conditionally(
                &claim,
                command(claim.clone()).operation,
                &authority.condition
            )
            .await
            .is_err()
    );
    assert_eq!(other_session.head().await.unwrap().commit_sequence, 0);
    assert_eq!(session.head().await.unwrap().commit_sequence, 0);
    assert!(engine.find_session("session").await.unwrap().is_some());
}
#[tokio::test]
async fn pre_claim_projection_is_read_only_and_refreshes_authoritative_commits() {
    use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator as _;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView as _;
    use awaken_dispatch_erasure::ClaimedCommitAdapter;
    let (_, _, engine, session, dispatch, claim) = claimed().await;
    let projection = Arc::new(session.open_projection().await.unwrap());
    let adapter = ClaimedCommitAdapter::read_only(projection.clone());
    assert!(
        adapter
            .commit_operation(command(claim.clone()).operation)
            .await
            .is_err()
    );
    assert_eq!(session.head().await.unwrap().commit_sequence, 0);
    <_ as ClaimedRunCommit<awaken_runtime_contract::runs::RunError>>::commit_operation(
        &engine.canonical_commits(dispatch),
        command(claim),
    )
    .await
    .unwrap();
    projection.refresh().await.unwrap();
    assert_eq!(
        adapter
            .committed_messages(&awaken_agent_contract::ThreadId("session".into()))
            .len(),
        1
    );
}

#[tokio::test]
async fn canonical_dispatch_barrier_blocks_history_changes_until_settled() {
    use awaken_agent_contract::{
        agent::thread::Id as ThreadId, thread::read::transcript::TranscriptView,
    };
    use awaken_dispatch_contract::{DispatchOutcome, DispatchSettlement as _};
    let (_, _, engine, session, dispatch, claim) = claimed().await;
    let target = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    assert!(matches!(
        session.rollback(&ThreadId("session".into()), &target).await,
        Err(awaken_store_s3::Error::Conflict(_))
    ));
    assert!(matches!(
        session.compact(&ThreadId("session".into()), vec![]).await,
        Err(awaken_store_s3::Error::Conflict(_))
    ));
    <_ as ClaimedRunCommit<awaken_runtime_contract::runs::RunError>>::commit_operation(
        &engine.canonical_commits(dispatch.clone()),
        command(claim.clone()),
    )
    .await
    .unwrap();
    assert!(
        session
            .rollback(&ThreadId("session".into()), &target)
            .await
            .is_err()
    );
    dispatch
        .settle(&claim.run_id, claim.epoch, DispatchOutcome::Done, &[])
        .await
        .unwrap();
    session
        .rollback(&ThreadId("session".into()), &target)
        .await
        .unwrap();
    session
        .compact(&ThreadId("session".into()), vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn claim_racing_a_staged_rollback_invalidates_its_head_cas() {
    use awaken_agent_contract::{
        agent::thread::Id as ThreadId, thread::read::transcript::TranscriptView,
    };
    use awaken_dispatch_contract::{DispatchOutcome, DispatchSettlement as _};
    let (objects, _, _, session, dispatch, claim) = claimed().await;
    let target = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    dispatch
        .settle(&claim.run_id, claim.epoch, DispatchOutcome::Done, &[])
        .await
        .unwrap();
    dispatch
        .enqueue(support::dispatch("next", "session", "session"))
        .await
        .unwrap();
    objects.hold_next_head.store(true, Ordering::SeqCst);
    let updating = session.clone();
    let task = tokio::spawn(async move {
        updating
            .rollback(&ThreadId("session".into()), &target)
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        objects.head_entered.notified(),
    )
    .await
    .expect("prepared root mutation must reach the held head CAS");
    dispatch
        .claim("next-worker", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    objects.head_resume.notify_one();
    assert!(matches!(
        task.await.unwrap(),
        Err(awaken_store_s3::Error::Conflict(_))
    ));
}

#[tokio::test]
async fn canonical_attachments_are_atomic_replayable_and_collected_by_reachability() {
    use bytes::Bytes;
    let (_, _, engine, session, dispatch, claim) = claimed().await;
    let blob = session
        .streams()
        .upload_blob(Bytes::from_static(b"archived tool output"), "text/plain")
        .await
        .unwrap();
    let writer = engine.canonical_commits(dispatch);
    let operation = command(claim);
    let receipt = writer
        .commit_with_blobs(operation.clone(), std::slice::from_ref(&blob))
        .await
        .unwrap();
    session.collect_garbage().await.unwrap();
    assert_eq!(
        session.streams().read_blob(&blob).await.unwrap(),
        Bytes::from_static(b"archived tool output"),
    );
    assert!(
        writer
            .commit_with_blobs(operation.clone(), std::slice::from_ref(&blob))
            .await
            .unwrap()
            .duplicate
    );
    let other = session
        .streams()
        .upload_blob(Bytes::from_static(b"unpublished replacement"), "text/plain")
        .await
        .unwrap();
    assert!(writer.commit_with_blobs(operation, &[other]).await.is_err());
    assert_eq!(
        session.head().await.unwrap().commit_sequence,
        receipt.commit_sequence
    );
}

#[tokio::test]
async fn cas_budget_exhaustion_is_retryable_without_changing_the_operation_identity() {
    use awaken_runtime_contract::runs::{RunError, RunErrorKind};
    let (objects, _, engine, session, dispatch, claim) = claimed().await;
    let writer = engine.canonical_commits(dispatch);
    let operation = command(claim);
    // C1 every root CAS is rejected before applying; C2 contention clears.
    // E1 unavailable (not a logical version conflict), with no committed prefix;
    // E2 the identical operation publishes once and subsequently replays.
    objects.cas_conflicts.store(u64::MAX, Ordering::SeqCst);
    let error = <_ as ClaimedRunCommit<RunError>>::commit_operation(&writer, operation.clone())
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::Unavailable);
    assert_eq!(error.code, "unavailable");
    assert_eq!(session.head().await.unwrap().commit_sequence, 0);
    objects.cas_conflicts.store(0, Ordering::SeqCst);
    let applied = <_ as ClaimedRunCommit<RunError>>::commit_operation(&writer, operation.clone())
        .await
        .unwrap();
    assert!(!applied.duplicate);
    let replayed = <_ as ClaimedRunCommit<RunError>>::commit_operation(&writer, operation)
        .await
        .unwrap();
    assert!(replayed.duplicate);
    assert_eq!(session.head().await.unwrap().commit_sequence, 1);
}

#[tokio::test]
async fn canonical_writer_shares_postgres_lost_acknowledgement_laws() {
    use awaken_dispatch_contract::COMMIT_OUTCOME_UNKNOWN;
    use awaken_runtime_contract::runs::RunError;

    let (objects, clock, engine, session, dispatch, claim) = claimed().await;
    let writer = engine.canonical_commits(dispatch.clone());
    let trace = awaken_store_conformance::claimed_commit::lost_acknowledgement_and_takeover(
        &writer,
        &session,
        command(claim),
        async |command| {
            objects.fault.store(1, Ordering::SeqCst);
            let error = <_ as ClaimedRunCommit<RunError>>::commit_operation(&writer, command)
                .await
                .unwrap_err();
            assert_eq!(error.code, COMMIT_OUTCOME_UNKNOWN);
        },
        async |_| {
            clock.advance(1000);
            RunClaim::from(
                &dispatch
                    .claim("replacement", 1000, &Default::default())
                    .await
                    .unwrap()
                    .unwrap()
                    .lease,
            )
        },
    )
    .await;
    awaken_reliability_testkit::write_observation_trace(
        "s3-claimed-store",
        "claimed-store",
        serde_json::json!([{"rule": "lost-acknowledgement-takeover", "trace": trace}]),
    );
}

#[tokio::test]
async fn canonical_writer_shares_postgres_control_checkpoint_preservation_law() {
    let (_, _, engine, _, dispatch, claim) = claimed().await;
    let checkpoint = engine.canonical_checkpoints(claim.clone(), dispatch.clone());
    awaken_store_conformance::claimed_commit::control_commit_preserves_interrupted_stream(
        &engine.canonical_commits(dispatch),
        &checkpoint,
        command(claim),
    )
    .await;
}
