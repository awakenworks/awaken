// @formal surface exclude=test_only
// @formal owner crates/platform/execution/awaken-dispatch-execution/src/process_dispatch.rs
// @formal reason: Reentrant construction observations and failed-start retry regressions.
use super::*;

#[tokio::test]
async fn pool_initialization_exposes_immutable_port_selection_until_failure() {
    // The resolver is running but the OnceCell has no published pool. This
    // observation must refuse application configuration; a failed factory
    // then releases the window and permits a later valid startup.
    let fixture = coordinator_fixture(awaken_deployment_config::LocalExecution::InProcessPool(
        DispatchPoolSizing::LocalParallelism(std::num::NonZeroUsize::new(1).unwrap()),
    ));
    assert_eq!(
        fixture.dispatch.configure_before_services_start(|| 7),
        Some(7)
    );
    let mut observed = false;
    let failure = fixture.dispatch.ensure_pool(
        Some(DispatchBinding::local(fixture.owner.clone())),
        Arc::new(NoCompletion),
        || {
            assert!(fixture.dispatch.pool().is_none());
            observed = fixture.dispatch.has_started_services();
            assert!(
                fixture
                    .dispatch
                    .configure_before_services_start(|| {
                        panic!("an in-flight factory must refuse configuration without running it")
                    })
                    .is_none()
            );
            Err(crate::session_execution::execution_error(
                "controlled selection failure",
            ))
        },
    );
    assert!(failure.is_err());
    assert!(
        observed,
        "in-flight resolver already selects immutable construction ports"
    );
    assert!(
        !fixture.dispatch.has_started_services(),
        "failed startup stays retryable"
    );
    assert_eq!(
        fixture.dispatch.configure_before_services_start(|| 9),
        Some(9)
    );
    fixture
        .dispatch
        .ensure_pool(
            Some(DispatchBinding::local(fixture.owner.clone())),
            Arc::new(NoCompletion),
            || Ok(fixture.resolver.clone()),
        )
        .unwrap();
    assert!(fixture.dispatch.has_started_services());
    assert!(
        fixture
            .dispatch
            .configure_before_services_start(|| {
                panic!("published service ports must never be replaced")
            })
            .is_none()
    );
    let _drain = fixture.dispatch.begin_drain().await;
}

#[tokio::test]
async fn terminal_initialization_exposes_immutable_port_selection_until_failure() {
    let fixture = coordinator_fixture(awaken_deployment_config::LocalExecution::RemoteWorkersOnly);
    assert_eq!(
        fixture.dispatch.configure_before_services_start(|| 7),
        Some(7)
    );
    let mut observed = false;
    assert!(!fixture.dispatch.ensure_terminal_reconciliation(
        true,
        Some(fixture.owner.clone()),
        Arc::new(NoCompletion),
        || {
            assert!(fixture.dispatch.terminal_reconciler.get().is_none());
            observed = fixture.dispatch.has_started_services();
            assert!(
                fixture
                    .dispatch
                    .configure_before_services_start(|| {
                        panic!("an in-flight factory must refuse configuration without running it")
                    })
                    .is_none()
            );
            None
        },
    ));
    assert!(
        observed,
        "in-flight resolver already selects immutable construction ports"
    );
    assert!(
        !fixture.dispatch.has_started_services(),
        "failed startup stays retryable"
    );
    assert_eq!(
        fixture.dispatch.configure_before_services_start(|| 9),
        Some(9)
    );
    assert!(fixture.dispatch.ensure_terminal_reconciliation(
        true,
        Some(fixture.owner.clone()),
        Arc::new(NoCompletion),
        || Some(fixture.resolver.clone()),
    ));
    assert!(fixture.dispatch.has_started_services());
    assert!(
        fixture
            .dispatch
            .configure_before_services_start(|| {
                panic!("published service ports must never be replaced")
            })
            .is_none()
    );
    assert!(fixture.dispatch.begin_drain().await.is_none());
}
