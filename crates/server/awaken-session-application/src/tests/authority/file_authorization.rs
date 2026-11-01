use crate::SessionPersistence;
use crate::tests::{
    FaultingSessionRepository, NoopRuntime, application_with_persistence, create, file_resources,
    persisted,
};
use awaken_session_contract::{LifecycleOutbox as _, SessionRepositoryReads as _};
use awaken_session_execution_contract::SessionFileReadAuthorization as _;
use awaken_session_types::SessionId;
use std::sync::Arc;

#[tokio::test]
async fn file_read_observation_refuses_root_failures_without_app_or_mutation() {
    // C1 the App is dropped while its original Queries survives; C2 owner is
    // missing/mismatched or root get returns Unavailable/NotFound; C3 one real
    // SQLite root/outbox is selected. E1 deny without Runtime or root effects;
    // E2 an owner refusal never performs get; E3 each get fault is reached.
    // R1 wrong/missing owner -> E1+E2. R2 correct owner + unavailable get ->
    // E1+E3; R3 correct owner + one-shot missing get -> E1+E3. The real HTTP
    // fixture covers the positive File/lease matrix through this same owner.
    let inner = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let id = SessionId::from("file-observation-failure");
    create(inner.as_ref(), persisted(id.as_str(), false, "idle")).await;
    let before = inner.get(&id).await.unwrap();
    let before_facts = serde_json::to_value(inner.pending_lifecycle().await.unwrap()).unwrap();
    let faulting = Arc::new(FaultingSessionRepository::new(inner.clone()));
    let app = Arc::new(application_with_persistence(
        Arc::new(NoopRuntime),
        SessionPersistence::new(
            faulting.clone(),
            Arc::new(awaken_lease_testkit::ManualClock::new(100)),
        ),
    ));
    let queries = app.queries();
    let weak_app = Arc::downgrade(&app);
    drop(app);
    assert!(weak_app.upgrade().is_none(), "C1 no App back-reference");
    let worker = awaken_worker_contract::WorkerIdentity::new("worker", "incarnation", 1);
    for (case, session, workspace) in [
        ("R1-owner", id.clone(), "another-workspace"),
        ("R1-missing", SessionId::from("missing"), "workspace"),
    ] {
        assert!(
            !queries
                .session_file_is_frozen_for_worker(&session, workspace, &worker, "file", 100)
                .await,
            "{case}"
        );
    }
    assert!(faulting.observed_gets.lock().unwrap().is_empty(), "E2");
    faulting.failed_gets.lock().unwrap().insert(id.to_string());
    assert!(
        !queries
            .session_file_is_frozen_for_worker(&id, "workspace", &worker, "file", 100)
            .await,
        "R2"
    );
    faulting.failed_gets.lock().unwrap().clear();
    faulting.get_not_found_once();
    assert!(
        !queries
            .session_file_is_frozen_for_worker(&id, "workspace", &worker, "file", 100)
            .await,
        "R3"
    );
    assert!(
        !faulting
            .get_not_found_once
            .load(std::sync::atomic::Ordering::SeqCst),
        "E3 fault consumed"
    );
    assert_eq!(
        *faulting.observed_gets.lock().unwrap(),
        vec![id.to_string(), id.to_string()],
        "E3"
    );

    // R4 the original File fixture makes every predicate true. Independently
    // remove the lease, make execution terminal, or restore the original
    // preparing intent: R5-R7 must each deny despite all other File/Worker
    // evidence. Existing return_get_once supplies read DTOs; it persists none.
    let mut matched = before.clone();
    matched
        .resources
        .prepare(&id, file_resources("file"))
        .unwrap();
    matched.realization = Some(crate::session_resource::SessionRealizationLease {
        owner: worker.worker_id.clone(),
        runtime_incarnation: worker.lease_owner(),
        epoch: 1,
        expires_at_unix_ms: 200,
    });
    let mut unowned = matched.clone();
    unowned.realization = None;
    let mut terminal = matched.clone();
    terminal.execution = awaken_session_contract::SessionExecutionState::Terminated;
    let mut preparing = matched.clone();
    preparing.baseline = awaken_session_configuration_contract::SessionBaselineState::Preparing(
        crate::tests::creation::creation_command(id.as_str()).intent,
    );
    for (rule, observed, expected) in [
        ("R4", matched, true),
        ("R5", unowned, false),
        ("R6", terminal, false),
        ("R7", preparing, false),
    ] {
        faulting.return_get_once(observed);
        assert_eq!(
            queries
                .session_file_is_frozen_for_worker(&id, "workspace", &worker, "file", 100)
                .await,
            expected,
            "{rule}"
        );
        assert!(
            faulting.get_override_once.lock().unwrap().is_none(),
            "{rule} read consumed"
        );
    }
    assert_eq!(inner.get(&id).await.unwrap(), before, "E1 unchanged root");
    assert_eq!(
        serde_json::to_value(inner.pending_lifecycle().await.unwrap()).unwrap(),
        before_facts,
        "E1 unchanged outbox"
    );
}
