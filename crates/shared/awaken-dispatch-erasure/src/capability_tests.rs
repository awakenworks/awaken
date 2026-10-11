//! Composition preserves one owner and genuine optional checkpoint identity.
use super::*;

#[tokio::test]
async fn erased_dispatch_retains_only_the_roles_selected_by_composition() {
    // C1 Worker view: local roles are unrepresentable (compile-fail docs).
    // C2 durable owner: continuation and scoped/global feed always exist.
    // C3 checkpoint disabled/enabled: None or the exact supplied Arc.
    // C4 repeated local binding: the cached Worker view keeps exact Arc identity.
    let backend = Arc::new(awaken_dispatch::MemoryDispatchStore::new(
        awaken_time::system_clock(),
    ));
    let coordinator = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(
        backend.clone(),
    ));
    let worker = coordinator.worker_view();
    assert!(
        Arc::ptr_eq(&worker, &coordinator.worker_view()),
        "C4 cached projection"
    );
    let binding = DispatchBinding::local(coordinator.clone());
    assert!(
        Arc::ptr_eq(&binding.queue(), &worker),
        "C4 same owner in the binding"
    );
    let DispatchTopology::Local(selected) = binding.placement() else {
        panic!("C4 local placement");
    };
    assert!(
        Arc::ptr_eq(selected, &coordinator),
        "C4 durable authority retained"
    );
    assert!(
        coordinator.stream_checkpoint_store().is_none(),
        "C3 disabled checkpoint"
    );

    fn require_continuation(_: &dyn SessionContinuationOutbox) {}
    require_continuation(coordinator.as_ref());
    let run = RunId("selected-owner".into());
    assert!(
        coordinator
            .events_after(DispatchCursor(0), 1, None)
            .await
            .is_ok(),
        "C2 global feed"
    );
    assert!(
        coordinator
            .events_after(DispatchCursor(0), 1, Some(&run))
            .await
            .is_ok(),
        "C2 scoped feed"
    );
    assert!(
        worker
            .list(&ThreadId("selected-thread".into()))
            .await
            .unwrap()
            .is_empty(),
        "C1 genuine Worker read role"
    );

    let checkpoint: Arc<dyn StreamCheckpointStore> =
        Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new());
    let paired = AnyCoordinatorDispatch::from_durable_store(
        awaken_dispatch::MemoryDispatchStore::new(awaken_time::system_clock()),
        Some(checkpoint.clone()),
    );
    require_continuation(&paired);
    assert!(
        Arc::ptr_eq(&paired.stream_checkpoint_store().unwrap(), &checkpoint),
        "C3 exact enabled checkpoint"
    );
}
