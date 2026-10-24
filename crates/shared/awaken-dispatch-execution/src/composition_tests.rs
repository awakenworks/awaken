//! Exact owner selection rejects mixed bindings before any execution or write.

use crate::{
    DispatchService, DispatchServiceConfig, DispatchWorker, LiveRunControlService, LocalWakeSignal,
};
use awaken_dispatch::MemoryDispatchStore;
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, DispatchBinding};
use awaken_store_inmem::MemoryCommitCoordinator;
use std::sync::Arc;

#[test]
fn local_projection_requires_its_source_before_execution() {
    // C1 a split reader has no snapshot load capability; C2 combined commit
    // supplies the source. C1 rejects cache installation at configuration;
    // C2 accepts it while retaining the local source. Neither case claims work.
    let clock = awaken_time::system_clock();
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(clock));
    let projection = Arc::new(crate::RecoveryProjection::new());
    let split = DispatchWorker::from_parts(
        Arc::new(awaken_runtime::Runtime::new()),
        store.clone(),
        commit.clone(),
        commit.clone(),
        "split",
    );
    assert!(split.with_recovery_projection(projection.clone()).is_err());
    let combined = DispatchWorker::new(
        Arc::new(awaken_runtime::Runtime::new()),
        store.clone(),
        commit,
        "combined",
    );
    assert!(combined.with_recovery_projection(projection).is_ok());
    assert_eq!(store.dispatch_count(), 0);
}

#[tokio::test]
async fn service_and_live_control_require_the_workers_exact_bound_owner() {
    // C1 Worker and controller share the selected owner; C2 unrelated owner.
    // S1 C1 permits construction and joined shutdown. S2 C2 returns a typed
    // error before spawning, claiming, committing, or mutating either queue.
    let clock = awaken_time::system_clock();
    let backend = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let foreign_backend = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let owner = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(
        backend.clone(),
    ));
    let foreign = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(
        foreign_backend.clone(),
    ));
    let binding = DispatchBinding::local(owner.clone());
    let worker = Arc::new(DispatchWorker::new_bound(
        Arc::new(awaken_runtime::Runtime::new()),
        binding.clone(),
        Arc::new(MemoryCommitCoordinator::new(clock.clone())),
        "composition-test",
    ));
    assert!(
        LiveRunControlService::from_local_owner(worker.clone(), foreign.clone(), clock.clone())
            .is_err(),
        "S2 live authority mismatch"
    );
    assert!(
        DispatchService::spawn_bound(
            worker.clone(),
            DispatchBinding::local(foreign),
            clock.clone(),
            DispatchServiceConfig::default(),
            Arc::new(LocalWakeSignal::new())
        )
        .is_err(),
        "S2 scheduling authority mismatch"
    );
    assert_eq!(backend.dispatch_count(), 0);
    assert_eq!(foreign_backend.dispatch_count(), 0);
    assert!(
        LiveRunControlService::from_local_owner(worker.clone(), owner, clock.clone()).is_ok(),
        "S1 exact live owner"
    );
    DispatchService::spawn_bound(
        worker,
        binding,
        clock,
        DispatchServiceConfig::default(),
        Arc::new(LocalWakeSignal::new()),
    )
    .expect("S1 exact scheduling owner")
    .shutdown()
    .await;
}
