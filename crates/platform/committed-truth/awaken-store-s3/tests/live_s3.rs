//! Live-provider tests deliberately require an explicit isolated S3 endpoint.
#[path = "live_s3/fixture.rs"]
mod live;
#[path = "live_s3/load.rs"]
mod load;
#[path = "live_s3/processes.rs"]
mod processes;
#[path = "live_s3/proxy.rs"]
mod proxy;
mod support;

use awaken_agent_contract::{
    agent::{run::Id as RunId, thread::Id as ThreadId},
    thread::read::recovery::RunRecoverySource,
};
use awaken_store_s3::{Error, Limits};
use bytes::Bytes;
use object_store::{ObjectStoreExt, PutMode, PutOptions, UpdateVersion, path::Path};
use std::sync::{Arc, atomic::Ordering};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn independent_clients_race_real_create_and_etag_conditions() {
    let fixture = live::Fixture::new(Limits::default()).await;
    let probe = Path::from(format!("{}/conditional-race", fixture.prefix));
    let mut predecessor: Option<UpdateVersion> = None;
    let mut stale = None;
    for round in 0..2 {
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let tasks = (0..8).map(|writer| {
            let client = live::connect(&live::endpoint());
            let path = probe.clone();
            let barrier = barrier.clone();
            let version = predecessor.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let mode = version.map(PutMode::Update).unwrap_or(PutMode::Create);
                let result = client
                    .put_opts(
                        &path,
                        Bytes::from(format!("{round}:{writer}")).into(),
                        PutOptions {
                            mode,
                            ..Default::default()
                        },
                    )
                    .await;
                (writer, result)
            })
        });
        let mut winners = Vec::new();
        for result in futures::future::join_all(tasks).await {
            let (writer, result) = result.unwrap();
            match result {
                Ok(receipt) => winners.push((writer, receipt)),
                Err(error) => assert!(
                    matches!(
                        error,
                        object_store::Error::Precondition { .. }
                            | object_store::Error::AlreadyExists { .. }
                    ),
                    "a losing conditional write must have a definitive provider precondition result: {error}"
                ),
            }
        }
        assert_eq!(winners.len(), 1, "one visible winner per provider version");
        let (writer, receipt) = winners.pop().unwrap();
        assert_eq!(
            fixture
                .store
                .get(&probe)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap(),
            Bytes::from(format!("{round}:{writer}"))
        );
        if round == 0 {
            let version = UpdateVersion {
                e_tag: receipt.e_tag,
                version: receipt.version,
            };
            assert!(version.e_tag.is_some() || version.version.is_some());
            stale = Some(version.clone());
            predecessor = Some(version);
        }
    }
    let before = fixture
        .store
        .get(&probe)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let error = live::connect(&live::endpoint())
        .put_opts(
            &probe,
            Bytes::from_static(b"stale").into(),
            PutOptions {
                mode: PutMode::Update(stale.unwrap()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, object_store::Error::Precondition { .. }));
    assert_eq!(
        fixture
            .store
            .get(&probe)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        before
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn actual_tcp_loss_before_and_after_put_preserves_one_operation() {
    for (upload, mode) in [
        (false, proxy::BEFORE),
        (false, proxy::AFTER),
        (true, proxy::BEFORE),
        (true, proxy::AFTER),
    ] {
        let fixture = live::Fixture::new(Limits::default()).await;
        let proxy = proxy::Proxy::new(live::endpoint()).await;
        let (_, _, through_proxy) = live::open(
            live::connect(&proxy.endpoint),
            &fixture.prefix,
            1_000,
            Limits::default(),
        );
        let claim = support::activate(&through_proxy, "run", "session")
            .await
            .run_claim();
        let operation = support::operation("run", "session", 0, 0, "exact bytes across TCP loss");
        if upload {
            proxy.arm_upload(mode);
        } else {
            proxy.arm(mode);
        }
        let first = through_proxy.commit(&claim, operation.clone()).await;
        assert!(
            first.is_ok()
                || matches!(&first, Err(Error::OutcomeUnknown(_)))
                || (upload && matches!(&first, Err(Error::Storage(_)))),
            "{first:?}"
        );
        let (_, _, cold) = fixture.cold(1_000, Limits::default());
        let before = cold.head().await.unwrap().commit_sequence;
        assert!(before <= 1);
        if matches!(&first, Err(Error::Storage(_))) {
            assert_eq!(
                before, 0,
                "an unconfirmed immutable upload cannot publish a head"
            );
        }
        if mode == proxy::AFTER {
            assert_eq!(
                proxy.fault.after.load(Ordering::SeqCst),
                1,
                "provider applied before TCP close"
            );
            if !upload {
                assert_eq!(before, 1);
            }
        } else {
            assert_eq!(proxy.fault.before.load(Ordering::SeqCst), 1);
        }
        let receipt = cold.commit(&claim, operation.clone()).await.unwrap();
        assert_eq!(receipt.duplicate, before == 1);
        let mut expected = receipt.clone();
        expected.duplicate = true;
        assert_eq!(cold.commit(&claim, operation).await.unwrap(), expected);
        let recovered = cold
            .recovery_snapshot(&ThreadId("session".into()), &RunId("run".into()))
            .await
            .unwrap();
        assert_eq!(recovered.thread_version, 1);
        assert_eq!(recovered.next_commit_ordinal, 1);
        assert_eq!(recovered.messages.len(), 1);
        assert_eq!(
            recovered.messages[0].text_content(),
            "exact bytes across TCP loss"
        );
        assert_eq!(cold.head().await.unwrap().commit_sequence, 1);
        drop(proxy);
        fixture.cleanup().await;
    }
}

/// Independent clients would otherwise choose and publish separate claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn concurrent_claim_requests_replay_the_same_epoch_after_cold_restart() {
    use awaken_dispatch::ObjectDispatchStore;
    use awaken_dispatch_contract::{
        ClaimRequest, ClaimRequestOutcome, ClaimSearch, DispatchControlClaims as _,
        DispatchEnqueue as _,
    };
    use awaken_dispatch_testkit::worker_http::ready_workers;
    use awaken_worker_contract::{LeastLoadedPolicy, WorkerObservationSource as _};

    let fixture = live::Fixture::new(Limits::default()).await;
    let request_id = uuid::Uuid::now_v7();
    let (seconds, nanos) = request_id.get_timestamp().unwrap().to_unix();
    let now = seconds * 1_000 + u64::from(nanos / 1_000_000);
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(now));
    let open = || {
        let engine = awaken_store_s3::S3Storage::new(
            live::connect(&live::endpoint()),
            &fixture.prefix,
            clock.clone(),
            Limits::default(),
        )
        .unwrap();
        ObjectDispatchStore::new(
            Arc::new(engine.component_store("dispatch").unwrap()),
            "tenant".into(),
            clock.clone(),
        )
    };
    let first = open();
    let second = open();
    for (run, thread, session) in [
        ("run", "thread", "session"),
        ("other-run", "other-thread", "other-session"),
    ] {
        let mut dispatch = support::dispatch(run, thread, session);
        dispatch.placement = awaken_worker_contract::PlacementRequirements::remote_preferred();
        dispatch = dispatch.with_worker_codec_requirements();
        first.enqueue(dispatch).await.unwrap();
    }
    let workers: Vec<_> = ready_workers(&["worker"])
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.snapshot)
        .collect();
    let requester = &workers[0];
    // No Run id pins the request to an initial candidate. Both clients still
    // have to converge on the same Session and publish at most one claim.
    let request = ClaimRequest {
        request_id: request_id.to_string(),
        run_id: None,
    };
    let policy = Arc::new(LeastLoadedPolicy);
    let first_shard = awaken_dispatch_contract::dispatch_shard("tenant", "session");
    let second_shard = awaken_dispatch_contract::dispatch_shard("tenant", "other-session");
    assert_ne!(first_shard, second_shard);
    let shards = first.claimable_shards().await.unwrap().unwrap();
    assert!(shards.contains(&first_shard));
    assert!(shards.contains(&second_shard));
    let (left, right) = tokio::join!(
        first.search_claim_request(
            &request,
            requester,
            workers.clone(),
            policy.clone(),
            1000,
            ClaimSearch::Shard(first_shard)
        ),
        second.search_claim_request(
            &request,
            requester,
            workers.clone(),
            policy.clone(),
            1000,
            ClaimSearch::Shard(second_shard)
        ),
    );
    let claim = |outcome| match outcome {
        ClaimRequestOutcome::Claimed(claimed) => claimed.lease,
        outcome => panic!("expected a replayable claim, got {outcome:?}"),
    };
    let left = claim(left.unwrap());
    let right = claim(right.unwrap());
    assert_eq!(left.run_id(), right.run_id());
    assert_eq!(left.epoch(), right.epoch());
    let cold = open();
    let replay = claim(
        cold.claim_for_request(&request, requester, workers.clone(), policy.clone(), 1000)
            .await
            .unwrap(),
    );
    assert_eq!(replay.run_id(), left.run_id());
    assert_eq!(replay.epoch(), left.epoch());
    let mut changed = request.clone();
    changed.run_id = Some(RunId("other-run".into()));
    assert!(matches!(
        cold.claim_for_request(&changed, requester, workers.clone(), policy.clone(), 1000)
            .await,
        Err(awaken_dispatch_contract::DispatchError::Conflict(_))
    ));
    clock.advance(1_000);
    assert!(matches!(
        cold.claim_for_request(&request, requester, workers.clone(), policy, 1000)
            .await
            .unwrap(),
        ClaimRequestOutcome::Empty
    ));
    // Durable retirement must survive a restarted Coordinator with a slow clock.
    use futures::TryStreamExt as _;
    let path = Path::from(format!(
        "{}/v1/routing/claim-requests/dispatch",
        fixture.prefix
    ));
    assert_eq!(
        fixture
            .store
            .list(Some(&path))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        1
    );
    clock.advance(16 * 60 * 1_000);
    let sweeper = open();
    sweeper.claimable_shards().await.unwrap();
    assert!(
        fixture
            .store
            .list(Some(&path))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    clock.set(now);
    assert!(matches!(
        open()
            .claim_for_request(
                &request,
                requester,
                workers.clone(),
                Arc::new(LeastLoadedPolicy),
                1000
            )
            .await
            .unwrap(),
        ClaimRequestOutcome::Empty
    ));
    fixture.cleanup().await;
}
