use crate::tests::{
    FaultingSessionRepository, NoopRuntime, application_with_persistence, create, persisted,
};
use crate::{SessionActivityError, SessionPersistence};
use awaken_session_types::SessionId;
use std::sync::Arc;
use {
    crate::SessionStore, awaken_session_contract::LifecycleOutbox as _,
    awaken_session_contract::SessionRepositoryReads as _,
};

#[tokio::test]
async fn activity_settlement_owner_survives_app_drop_and_root_cas_faults() {
    // Causes: C1 retain the existing Lifecycle after dropping its App; C2 the
    // same real SQLite CAS fails before commit or reports Conflict after commit;
    // C3 retry the original epoch and then advance the same selected Clock.
    // Effects: E1 C1 still settles without App/Runtime discovery; E2 precommit
    // failure preserves the exact root/outbox; E3 response-loss retry converges
    // to one close; E4 later exact replay preserves its original interval/fact.
    // | Rule | C2 | first result | durable effect | exact retry |
    // |---|---|---|---|---|
    // | L1 | precommit unavailable | typed Unavailable | unchanged | one close |
    // | L2 | committed response loss | original bounded readback succeeds | one close | stutter |
    // Fixture scope: original persisted() legacy data enters the real SQLite
    // Store. This is command ownership/retry, not public Create or physical Run
    // qualification. Clock assertions retain stamps, not callback/read counts.
    // Both one-shot faults must be consumed; this does not force three-conflict
    // exhaustion or count Notify callbacks. Durable Outbox remains authority.
    for fault in ["before", "after"] {
        let id = SessionId::from(format!("lifecycle-settlement-{fault}"));
        let inner = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let faulting = Arc::new(FaultingSessionRepository::new(inner.clone()));
        create(inner.as_ref(), persisted(id.as_str(), false, "idle")).await;
        let repository: Arc<dyn SessionStore> = faulting.clone();
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
        let app = Arc::new(application_with_persistence(
            Arc::new(NoopRuntime),
            SessionPersistence::new(repository, clock.clone()),
        ));
        let active = app
            .activities
            .begin_activity(&id)
            .await
            .expect("L1/L2 real activity");
        let before = inner.get(&id).await.unwrap();
        let before_outbox = serde_json::to_value(inner.pending_lifecycle().await.unwrap()).unwrap();
        let original_interval = before
            .running_interval
            .as_ref()
            .unwrap()
            .interval_id
            .clone();
        let owner = app.lifecycle();
        let original_app = Arc::downgrade(&app);
        drop(app);
        assert!(original_app.upgrade().is_none(), "C1 no App back-reference");
        if fault == "before" {
            faulting.fail_once("settle-activity");
        } else {
            faulting.commit_then_conflict_once("settle-activity");
        }
        clock.set(500);
        let first = owner
            .settle_activity_observed(&id, active.activity_epoch, None)
            .await;
        let closed = if fault == "before" {
            let error = first.expect_err("L1 original precommit refusal");
            assert!(
                matches!(&error, SessionActivityError::Unavailable(_)),
                "L1: {error}"
            );
            assert!(
                error.to_string().contains("injected root-CAS outage"),
                "L1 exact fault"
            );
            assert_eq!(
                inner.get(&id).await.unwrap(),
                before,
                "L1/E2 root unchanged"
            );
            assert_eq!(
                serde_json::to_value(inner.pending_lifecycle().await.unwrap()).unwrap(),
                before_outbox,
                "L1/E2 no durable fact before successful CAS"
            );
            owner
                .settle_activity_observed(&id, active.activity_epoch, None)
                .await
                .expect("L1 exact retry")
        } else {
            first.expect("L2 original committed-response-loss readback")
        };
        assert!(
            faulting.fail_operation_once.lock().unwrap().is_none(),
            "L1 fault reached"
        );
        assert!(
            faulting.conflict_operation_once.lock().unwrap().is_none(),
            "L2 fault reached"
        );
        assert_eq!(closed.execution.as_str(), "idle", "E1/E3");
        assert!(closed.active_activity_epochs.is_empty(), "E3");
        assert!(closed.running_interval.is_none(), "E3");
        assert_eq!(closed.revision.0, before.revision.0 + 1, "E3 one root CAS");
        assert_eq!(closed.closed_runtime_intervals.len(), 1, "E3");
        assert_eq!(
            closed.closed_runtime_intervals[0].interval_id, original_interval,
            "E3"
        );
        assert_eq!(
            closed.closed_runtime_intervals[0].ended_at_unix_ms, 500,
            "same Clock"
        );
        let facts = inner.pending_lifecycle().await.unwrap();
        assert_eq!(
            facts
                .iter()
                .filter(|f| f.object_id == id.as_str() && f.runtime_interval.is_some())
                .count(),
            1,
            "E3 one outbox interval fact"
        );
        let durable_facts = serde_json::to_value(facts).unwrap();
        clock.set(900);
        assert_eq!(
            owner
                .settle_activity_observed(&id, active.activity_epoch, None)
                .await
                .unwrap(),
            closed,
            "E4 exact replay"
        );
        assert_eq!(
            serde_json::to_value(inner.pending_lifecycle().await.unwrap()).unwrap(),
            durable_facts,
            "E4 unchanged durable fact after later Clock"
        );
    }
}
