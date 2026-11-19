mod support;

use awaken_lease_testkit::ManualClock;
use awaken_object_storage::{SessionObject, SessionObjectStore as _};
use awaken_store_s3::{Limits, S3ComponentStore, S3Storage, SessionKey};
use futures::TryStreamExt as _;
use object_store::{ObjectStore as _, ObjectStoreExt as _, PutPayload, path::Path};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::Notify;

const FIRST_EXPIRY: u64 = 1_500;
const CUTOFF: u64 = 2_000;
const LIVE_EXPIRY: u64 = 3_000;

fn session() -> SessionObject {
    SessionObject {
        session_id: "session".into(),
        owner_scope: "tenant".into(),
        version: String::new(),
        payload: None,
        conditions: Default::default(),
    }
}

async fn bind(dispatch: &S3ComponentStore, request: &str, expires: u64) {
    assert!(
        dispatch
            .claim_request_partition(request, expires, Some(&session()))
            .await
            .unwrap()
            .is_some()
    );
}

async fn route_expiries(store: &support::FaultStore) -> Vec<u64> {
    let prefix = Path::from("test/v1/routing/claim-requests/dispatch");
    let mut expiries: Vec<_> = store
        .list(Some(&prefix))
        .map_ok(|object| {
            object
                .location
                .filename()
                .unwrap()
                .split_once('-')
                .unwrap()
                .0
                .parse::<u64>()
                .unwrap()
        })
        .try_collect()
        .await
        .unwrap();
    expiries.sort_unstable();
    expiries
}

async fn reach_storage_gate<T>(operation: Pin<&mut impl Future<Output = T>>, entered: &Notify) {
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(30), entered.notified()) => {
            result.expect("the operation reaches the injected storage gate");
        }
        _ = operation => panic!("the operation finished before reaching the storage gate"),
    }
}

#[tokio::test]
async fn shard_discovery_does_not_advance_claim_retention() {
    let engine = S3Storage::new(
        support::object_transport(),
        "claim-discovery",
        Arc::new(ManualClock::new(1_000)),
        Limits::default(),
    )
    .unwrap();
    let dispatch = engine.component_store("dispatch").unwrap();

    assert_eq!(
        dispatch.claimable_shards(1_000).await.unwrap(),
        Some(vec![])
    );
    assert_eq!(dispatch.claim_request_cutoff().await.unwrap(), 0);
}

#[tokio::test]
async fn cutoff_timeout_before_put_leaves_collection_fenced_until_retry() {
    let (store, clock, engine, _) = support::setup().await;
    let dispatch = engine.component_store("dispatch").unwrap();
    bind(&dispatch, "old", FIRST_EXPIRY).await;
    clock.set(CUTOFF);
    store.hold_next_cutoff.store(true, Ordering::SeqCst);
    let mut retirement = Box::pin(dispatch.retire_claim_requests());
    reach_storage_gate(retirement.as_mut(), &store.claim_write_entered).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, retirement)
            .await
            .is_err()
    );

    assert_eq!(dispatch.claim_request_cutoff().await.unwrap(), 0);
    assert!(!dispatch.collect_retired_claim_requests().await.unwrap());
    assert_eq!(route_expiries(&store).await, [FIRST_EXPIRY]);

    assert_eq!(dispatch.retire_claim_requests().await.unwrap(), CUTOFF);
    assert!(!dispatch.collect_retired_claim_requests().await.unwrap());
    assert!(route_expiries(&store).await.is_empty());
}

#[tokio::test]
async fn a_lost_cutoff_acknowledgement_fences_claims_during_a_delete_timeout() {
    let (store, clock, engine, _) = support::setup().await;
    let dispatch = engine.component_store("dispatch").unwrap();
    bind(&dispatch, "old", CUTOFF).await;
    bind(&dispatch, "live", LIVE_EXPIRY).await;
    clock.set(CUTOFF);
    store.lose_cutoff_response.store(true, Ordering::SeqCst);
    assert_eq!(dispatch.retire_claim_requests().await.unwrap(), CUTOFF);

    store.hold_next_delete.store(true, Ordering::SeqCst);
    let mut cleanup = Box::pin(dispatch.collect_retired_claim_requests());
    reach_storage_gate(cleanup.as_mut(), &store.delete_entered).await;
    assert!(tokio::time::timeout(Duration::ZERO, cleanup).await.is_err());
    assert_eq!(route_expiries(&store).await, [CUTOFF, LIVE_EXPIRY]);

    let cold = support::engine(store.clone(), Arc::new(ManualClock::new(1_000)))
        .component_store("dispatch")
        .unwrap();
    assert_eq!(cold.retire_claim_requests().await.unwrap(), CUTOFF);
    assert!(
        cold.claim_request_partition("old", CUTOFF, Some(&session()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cold.claim_request_partition("live", LIVE_EXPIRY, None)
            .await
            .unwrap()
            .is_some()
    );
    assert!(!cold.collect_retired_claim_requests().await.unwrap());
    assert_eq!(route_expiries(&store).await, [LIVE_EXPIRY]);
}

#[tokio::test]
async fn independent_sweepers_finish_partial_deletion_using_a_monotone_cutoff() {
    let (store, clock, engine, _) = support::setup().await;
    let dispatch = engine.component_store("dispatch").unwrap();
    bind(&dispatch, "old", FIRST_EXPIRY).await;
    bind(&dispatch, "boundary", CUTOFF).await;
    bind(&dispatch, "live", LIVE_EXPIRY).await;
    clock.set(CUTOFF);
    let peer = support::engine(store.clone(), Arc::new(ManualClock::new(FIRST_EXPIRY)))
        .component_store("dispatch")
        .unwrap();
    let (left, right) = tokio::join!(
        dispatch.retire_claim_requests(),
        peer.retire_claim_requests()
    );
    assert_eq!(left.unwrap(), CUTOFF);
    assert!([FIRST_EXPIRY, CUTOFF].contains(&right.unwrap()));
    assert_eq!(peer.claim_request_cutoff().await.unwrap(), CUTOFF);

    store.fail_delete_after_first.store(true, Ordering::SeqCst);
    assert!(dispatch.collect_retired_claim_requests().await.is_err());
    assert_eq!(route_expiries(&store).await, [CUTOFF, LIVE_EXPIRY]);

    let (left, right) = tokio::join!(
        dispatch.collect_retired_claim_requests(),
        peer.collect_retired_claim_requests()
    );
    assert!(!left.unwrap());
    assert!(!right.unwrap());
    assert_eq!(route_expiries(&store).await, [LIVE_EXPIRY]);
}

#[tokio::test]
async fn route_collection_finishes_backlog_in_bounded_batches() {
    let (store, clock, engine, _) = support::setup().await;
    let dispatch = engine.component_store("dispatch").unwrap();
    bind(&dispatch, "live", LIVE_EXPIRY).await;

    // Seed route objects directly so this exercises the collection limit.
    let prefix = Path::from("test/v1/routing/claim-requests/dispatch");
    let route = PutPayload::from(
        serde_json::to_vec(&SessionKey::new("tenant", "session").unwrap()).unwrap(),
    );
    futures::stream::iter((0..1_001).map(Ok))
        .try_for_each_concurrent(16, |index| {
            let path = prefix.clone().join(format!("{CUTOFF:020}-{index:064x}"));
            let route = route.clone();
            let store = &store;
            async move { store.put(&path, route).await.map(|_| ()) }
        })
        .await
        .unwrap();

    clock.set(CUTOFF);
    assert_eq!(dispatch.retire_claim_requests().await.unwrap(), CUTOFF);
    assert!(dispatch.collect_retired_claim_requests().await.unwrap());
    assert_eq!(route_expiries(&store).await, [CUTOFF, LIVE_EXPIRY]);
    assert!(!dispatch.collect_retired_claim_requests().await.unwrap());
    assert_eq!(route_expiries(&store).await, [LIVE_EXPIRY]);
}

#[tokio::test]
async fn a_route_created_after_collection_cannot_authorize_the_old_request() {
    let (store, clock, engine, _) = support::setup().await;
    let sweeper = engine.component_store("dispatch").unwrap();
    let cold = support::engine(store.clone(), Arc::new(ManualClock::new(1_000)))
        .component_store("dispatch")
        .unwrap();
    let proposed = session();
    store.hold_next_claim_route.store(true, Ordering::SeqCst);
    let mut late = Box::pin(cold.claim_request_partition("old", CUTOFF, Some(&proposed)));
    reach_storage_gate(late.as_mut(), &store.claim_write_entered).await;
    clock.set(CUTOFF);
    assert_eq!(sweeper.retire_claim_requests().await.unwrap(), CUTOFF);
    assert!(!sweeper.collect_retired_claim_requests().await.unwrap());

    store.claim_write_resume.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(30), late)
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(route_expiries(&store).await, [CUTOFF]);
    assert!(!sweeper.collect_retired_claim_requests().await.unwrap());
    assert!(route_expiries(&store).await.is_empty());
}

#[tokio::test]
async fn maintenance_retires_routes_without_discovery_and_can_be_cancelled() {
    let (store, clock, engine, _) = support::setup().await;
    let dispatch = engine.component_store("dispatch").unwrap();
    bind(&dispatch, "old", CUTOFF).await;
    clock.set(CUTOFF);
    store.hold_next_delete.store(true, Ordering::SeqCst);
    let mut maintenance = Box::pin(dispatch.maintain_claim_requests());
    reach_storage_gate(maintenance.as_mut(), &store.delete_entered).await;

    assert_eq!(dispatch.claim_request_cutoff().await.unwrap(), CUTOFF);
    assert_eq!(route_expiries(&store).await, [CUTOFF]);
    drop(maintenance);
    assert!(!dispatch.collect_retired_claim_requests().await.unwrap());
    assert!(route_expiries(&store).await.is_empty());
}
