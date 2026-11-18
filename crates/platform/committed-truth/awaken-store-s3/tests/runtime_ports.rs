mod support;
use awaken_agent_contract::{
    agent::{run::Id as RunId, thread::Id as ThreadId},
    event::{AgentEvent, Delta},
    stream::{
        event::{Event, Observation},
        sink::Error as StreamError,
    },
    thread::{
        commit::coordinator::OperationCoordinator, read::committed_thread_view::CommittedThreadView,
    },
};
use awaken_dispatch_contract::ClaimedStreamPublisher;
use awaken_dispatch_erasure::ClaimedCommitAdapter;
use std::sync::Arc;

#[derive(Default)]
struct Live {
    observations: awaken_sync::LeafLock<Vec<Observation>>,
}
#[async_trait::async_trait]
impl ClaimedStreamPublisher for Live {
    async fn publish(
        &self,
        _: &awaken_worker_contract::RunClaim,
        event: Event,
    ) -> Result<(), StreamError> {
        self.observations.lock().push(event.into());
        Ok(())
    }
    async fn publish_observation(
        &self,
        _: &awaken_worker_contract::RunClaim,
        event: Observation,
    ) -> Result<(), StreamError> {
        self.observations.lock().push(event);
        Ok(())
    }
}
#[tokio::test]
async fn runtime_coordinator_adapter_uses_claimed_s3_writer_and_refreshes_its_view() {
    let (_, _, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let bound = Arc::new(session.bind(claim.clone()).await.unwrap());
    let adapter = ClaimedCommitAdapter::new(bound, claim);
    adapter
        .commit_operation(support::operation("run", "session", 0, 0, "sdk boundary"))
        .await
        .unwrap();
    assert_eq!(
        adapter.committed_messages(&ThreadId("session".into()))[0].text_content(),
        "sdk boundary"
    );
}
#[tokio::test]
async fn native_and_acp_neutral_observations_are_durable_before_live_delivery() {
    let (store, clock, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let live = Arc::new(Live::default());
    let publisher = session.streams().progress_publisher(live.clone());
    let observation = Observation::assistant_delta(
        RunId("run".into()),
        ThreadId("session".into()),
        3,
        2,
        AgentEvent::Delta(Delta::TextDelta {
            delta: "streamed".into(),
        }),
    );
    publisher
        .publish_observation(&claim, observation.clone())
        .await
        .unwrap();
    assert_eq!(
        live.observations.lock().as_slice(),
        std::slice::from_ref(&observation)
    );
    let cold = support::engine(store, clock).session(session.key().clone());
    let watermark = cold
        .streams()
        .stream_watermarks("run")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let mut start = watermark;
    start.next_frame = 0;
    assert_eq!(
        cold.streams().read_observations(&start, 10).await.unwrap(),
        vec![observation]
    );
}

#[tokio::test]
async fn independent_publishers_preserve_identical_deltas_when_their_ordinals_race() {
    use std::sync::atomic::Ordering;
    let (objects, _, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let observation = Observation::assistant_delta(
        RunId("run".into()),
        ThreadId("session".into()),
        0,
        0,
        AgentEvent::Delta(Delta::TextDelta { delta: "ha".into() }),
    );
    let first = session
        .streams()
        .progress_publisher(Arc::new(Live::default()));
    let second = session
        .streams()
        .progress_publisher(Arc::new(Live::default()));
    objects.hold_next_head.store(true, Ordering::SeqCst);
    let old_claim = claim.clone();
    let old_observation = observation.clone();
    let staged =
        tokio::spawn(async move { first.persist_batch(&old_claim, vec![old_observation]).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        objects.head_entered.notified(),
    )
    .await
    .unwrap();
    second
        .persist_batch(&claim, vec![observation.clone()])
        .await
        .unwrap();
    objects.head_resume.notify_one();
    staged.await.unwrap().unwrap();
    let page = session
        .streams()
        .read_run_page(
            &awaken_store_s3::StreamReadCursor {
                key: session.key().clone(),
                run: "run".into(),
                after: 0,
            },
            10,
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.watermark, 2);
    assert_eq!(
        page.observations().unwrap(),
        vec![observation.clone(), observation]
    );
}
#[tokio::test]
async fn a_batch_cannot_silently_relabel_another_response_stream() {
    let (_, _, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let publisher = session
        .streams()
        .progress_publisher(Arc::new(Live::default()));
    let make = |response| {
        Observation::assistant_delta(
            RunId("run".into()),
            ThreadId("session".into()),
            0,
            response,
            AgentEvent::Delta(Delta::TextDelta {
                delta: "text".into(),
            }),
        )
    };
    assert!(
        publisher
            .persist_batch(&claim, vec![make(1), make(2)])
            .await
            .is_err()
    );
    assert!(
        session
            .streams()
            .stream_watermarks("run")
            .await
            .unwrap()
            .is_empty()
    );
}

struct PausedAuthority {
    queue: Arc<awaken_dispatch::ObjectDispatchStore>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::DispatchCommitAuthority for PausedAuthority {
    async fn worker_owns_run(
        &self,
        identity: &awaken_worker_contract::WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<awaken_worker_contract::RunClaim>, awaken_dispatch_contract::DispatchError>
    {
        self.queue.worker_owns_run(identity, run_id).await
    }

    async fn lock_commit_epoch(
        &self,
        claim: &awaken_worker_contract::RunClaim,
    ) -> Result<
        Option<awaken_dispatch_contract::CommitEpochGuard>,
        awaken_dispatch_contract::DispatchError,
    > {
        self.queue.lock_commit_epoch(claim).await
    }

    async fn acquire_commit_authority(
        &self,
        claim: &awaken_worker_contract::RunClaim,
    ) -> Result<
        Option<awaken_dispatch_contract::ClaimAuthority>,
        awaken_dispatch_contract::DispatchError,
    > {
        self.entered.notify_one();
        self.release.notified().await;
        self.queue.acquire_commit_authority(claim).await
    }
}

#[tokio::test]
async fn progress_retry_reconciles_a_receipt_published_before_takeover() {
    use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _, ProgressBatch};
    use awaken_worker_contract::RunClaim;

    let (_store, clock, engine, _) = support::setup().await;
    let queue = Arc::new(awaken_dispatch::ObjectDispatchStore::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "tenant".into(),
        clock.clone(),
    ));
    queue
        .enqueue(support::dispatch("run", "session", "session"))
        .await
        .unwrap();
    let first = queue
        .claim("first", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&first.lease);
    let observation = Observation::assistant_delta(
        RunId("run".into()),
        ThreadId("session".into()),
        0,
        0,
        AgentEvent::Delta(Delta::TextDelta {
            delta: "published".into(),
        }),
    );
    let batch = ProgressBatch {
        commit_ordinal: 0,
        response: observation.assistant_response.clone().unwrap(),
        batch_index: 0,
        observations: vec![observation],
    };
    let gate = Arc::new(PausedAuthority {
        queue: queue.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let blocked = engine.canonical_commits(gate.clone());
    let mut repeat = Box::pin(blocked.append_progress_batch(&claim, batch.clone()));
    tokio::select! {
        () = gate.entered.notified() => {}
        result = &mut repeat => panic!("retry finished before authority was checked: {result:?}"),
    }

    let owner = engine.canonical_commits(queue.clone());
    let receipt = owner
        .append_progress_batch(&claim, batch.clone())
        .await
        .unwrap();
    let mut other_step = batch;
    other_step.response.step += 1;
    other_step.observations[0].assistant_response = Some(other_step.response.clone());
    assert_eq!(
        owner
            .append_progress_batch(&claim, other_step)
            .await
            .expect("another assistant Step has an independent batch zero"),
        receipt
    );
    clock.advance(1000);
    let takeover = queue
        .claim("successor", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(takeover.lease.epoch() > first.lease.epoch());

    gate.release.notify_one();
    assert_eq!(repeat.await.unwrap(), receipt);
}
