// @formal surface exclude=composition_wiring
// @formal owner crates/contract/awaken-session-event-contract/src/lifecycle.rs
// @formal owner crates/server/awaken-session-application/src/lifecycle_outbox_relay.rs
// @formal reason: Static owner: Coordinator assembly of existing lifecycle delivery ports. Dynamic path:
// @formal reason: committed lifecycle facts flow through CompositeLifecycleFactDelivery to the existing
// @formal reason: LifecycleOutboxRelay; no lifecycle transition, retry, or durable-outbox decision is
// @formal reason: reimplemented here. Evidence:
// @formal reason: crates/contract/awaken-session-event-contract/src/lifecycle.rs and
// @formal reason: crates/server/awaken-session-application/src/lifecycle_outbox_relay.rs.
//! One Coordinator composition for Session lifecycle delivery.
//!
//! `LifecycleOutboxRelay` remains the sole durable outbox consumer. Its
//! delivery fan-out includes the optional Control webhook sink and one weak
//! Managed projection receiver; neither receiver reads or acknowledges the
//! outbox independently.

use std::sync::Arc;

use awaken_protocol_managed::{ManagedLifecycleFactDelivery, ManagedState};
use awaken_session_application::{
    LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL, LifecycleOutboxRelay, SessionApplication,
};
use awaken_session_application_contract::{
    CompositeLifecycleFactDelivery, LifecycleFactDelivery, LifecycleFactNotifier,
};

#[derive(Debug, thiserror::Error)]
pub enum ManagedLifecycleCompositionError {
    #[error("bind Session lifecycle notifier: {0}")]
    Notifier(&'static str),
    #[error("start Session lifecycle outbox supervisor: {0}")]
    Supervisor(&'static str),
}

/// Prepared connection to the existing sole outbox consumer. Preparation binds
/// its receivers and owns its replay loop; only `start` spawns that same loop.
pub struct PreparedManagedLifecycleDelivery {
    managed: Arc<ManagedLifecycleFactDelivery>,
    replay: awaken_session_application::LifecycleOutboxReplay,
}

impl PreparedManagedLifecycleDelivery {
    pub fn start(
        self,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    ) -> Arc<ManagedLifecycleFactDelivery> {
        let Self { managed, replay } = self;
        service_lifecycle.spawn("coordinator-webhook-outbox", move |cancel| {
            replay.run(cancel)
        });
        managed
    }
}

/// Bind the one lifecycle delivery chain without starting background work.
/// Process assembly starts it only after all application connections validate.
pub fn prepare_managed_lifecycle_delivery(
    state: &Arc<ManagedState>,
    application: &SessionApplication,
    webhook: Option<Arc<dyn LifecycleFactDelivery>>,
    deployments: Option<&awaken_deployment_application::DeploymentApplication>,
) -> Result<PreparedManagedLifecycleDelivery, ManagedLifecycleCompositionError> {
    let managed = Arc::new(ManagedLifecycleFactDelivery::new(state));

    let managed_delivery: Arc<dyn LifecycleFactDelivery> = managed.clone();
    let delivery: Arc<dyn LifecycleFactDelivery> = match webhook {
        Some(webhook) => Arc::new(CompositeLifecycleFactDelivery::new(vec![
            webhook,
            managed_delivery,
        ])),
        None => managed_delivery,
    };
    let notifier = Arc::new(LifecycleOutboxRelay::new(
        delivery,
        application.lifecycle_outbox_handle(),
    ));
    let notifier_port: Arc<dyn LifecycleFactNotifier> = notifier.clone();
    application
        .set_lifecycle_notifier(notifier_port)
        .map_err(ManagedLifecycleCompositionError::Notifier)?;
    if let Some(deployments) = deployments {
        let deployment_notifier: Arc<dyn LifecycleFactNotifier> = notifier.clone();
        deployments.bind_lifecycle_notifier(deployment_notifier);
    }
    let replay = notifier
        .replay_loop(LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL)
        .map_err(ManagedLifecycleCompositionError::Supervisor)?;
    Ok(PreparedManagedLifecycleDelivery { managed, replay })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use awaken_protocol_managed_testkit::ManagedStateFixture as _;
    use std::time::Duration;

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::stream::event::Event as StreamEvent;
    use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};
    use awaken_protocol_managed::{ManagedState, StateError, router};
    use awaken_protocol_managed_testkit::CoordinatedRuntimeFake;
    use awaken_session_application_contract::LifecycleFactDelivery;
    use awaken_session_execution_contract::{AdmitSessionRun, session_run_id};
    use awaken_session_state_contract::{ManagedLifecycleFact, SessionExecutionState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct RecordingWebhook(Mutex<Vec<ManagedLifecycleFact>>);

    #[async_trait::async_trait]
    impl LifecycleFactDelivery for RecordingWebhook {
        async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
            self.0.lock().unwrap().push(fact.clone());
            Ok(())
        }
    }

    struct NoopSink;

    #[async_trait::async_trait]
    impl Sink for NoopSink {
        async fn send(&self, _event: StreamEvent) -> Result<(), SinkError> {
            Ok(())
        }
    }

    fn event_identity(state: &ManagedState, session_id: &str) -> Vec<(String, &'static str)> {
        state
            .list_events(session_id, None, None, false)
            .expect("list projected events")
            .data
            .into_iter()
            .map(|event| {
                let event_type = event.type_str();
                (event.id, event_type)
            })
            .collect()
    }

    #[tokio::test]
    async fn prepared_lifecycle_delivery_waits_for_explicit_service_start() {
        // Causes: C1 one prepared Store-bound relay; C2 a committed Session
        // creation fact; C3 the prepared loop is started. R1 C1+C2+!C3 => durable
        // pending fact, zero deliveries. R2 C1+C2+C3 => that exact fact delivered
        // and acknowledged by the existing consumer. No alternate drain exists.
        let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()).await);
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        let webhook = Arc::new(RecordingWebhook::default());
        let prepared = prepare_managed_lifecycle_delivery(
            &state,
            &state.session_application(),
            Some(webhook.clone()),
            None,
        )
        .unwrap();
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent": "coder",
                    "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap();
        let outbox = state.session_application().lifecycle_outbox_handle();
        let pending = outbox.pending_lifecycle().await.unwrap();
        let fact = pending
            .iter()
            .find(|fact| fact.object_id == session.id)
            .unwrap();
        let fact_id = fact.id.clone();
        tokio::task::yield_now().await;
        assert!(
            webhook.0.lock().unwrap().is_empty(),
            "R1 zero delivery before start"
        );
        assert!(
            !outbox.pending_lifecycle().await.unwrap().is_empty(),
            "R1 retained durable fact"
        );
        prepared.start(&lifecycle);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if webhook
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|fact| fact.id == fact_id)
                    && outbox.pending_lifecycle().await.unwrap().is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("R2 one existing outbox consumer delivers and acknowledges");
        lifecycle.shutdown(Duration::from_secs(2)).await.unwrap();
    }

    /// Lifecycle wake cause/effect graph: C1 the receiver/application and a
    /// Runtime accepting the complete frozen Session projection are composed
    /// before traffic; C2 durable Session + child facts advance outside the
    /// Managed handler; C3 the fact names that Session (or an unknown object);
    /// C4 the same stable fact is replayed; C5 a webhook receiver is present.
    /// Effects: E1 the sole projector broadcasts child then aggregate idle and
    /// closes the already-open SSE; E2 webhook and Managed are both attempted by
    /// the existing composite; E3 replay appends nothing; E4 unknown objects
    /// append/broadcast nothing; E5 duplicate notifier installation fails
    /// before a second outbox consumer starts.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | Effects |
    /// |---|---|---|---|---|---|---|
    /// | L1 | yes | yes | known | no | yes | E1,E2 |
    /// | L2 | yes | already seen | known | yes | yes | E3 |
    /// | L3 | yes | any | unknown | no | any | E4 |
    /// | L4 | duplicate | - | - | - | - | E5 |
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_outbox_fanout_refreshes_open_sse_and_replay_is_idempotent() {
        // Causes: the fixtures below establish `one outbox fanout refreshes open sse and replay`
        // with the concrete inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: Session creation uses only the complete projection port; the
        // Coordinator routes one neutral Session/Run lifecycle; live delivery is best-effort and
        // cannot replace committed replay truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()).await);
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        let webhook = Arc::new(RecordingWebhook::default());
        let delivery = prepare_managed_lifecycle_delivery(
            &state,
            &state.session_application(),
            Some(webhook.clone()),
            None,
        )
        .map(|prepared| prepared.start(&lifecycle))
        .expect("L1 bind before traffic");

        let unknown = ManagedLifecycleFact {
            id: "fact-unknown-object".into(),
            object_id: "session-that-does-not-exist".into(),
            workspace_id: None,
            event_type: "deployment.status_changed".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        delivery.deliver(&unknown).await.expect("L3 ignored");
        assert!(
            matches!(
                state.stream_subscribe(&unknown.object_id),
                Err(StateError::NotFound)
            ),
            "L3/E4 no false projection is created"
        );

        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent": "coder",
                    "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .expect("L1 create Session");
        // The Session is owned by a Workspace, so the request must arrive
        // stamped the way the composition root stamps a single-machine
        // deployment. An unstamped request is refused with 404 before the
        // handler body, which is what this precondition used to trip over.
        let app = crate::workspace_path::with_platform_workspace(
            router(state.clone()),
            awaken_tenancy::DEFAULT_WORKSPACE_ID.to_string(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/v1/sessions/{}/events/stream", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("L1 open SSE");
        assert_eq!(response.status(), StatusCode::OK, "L1 precondition");

        // Bypass the Managed command handler as a background dispatch/report
        // completion does. The application commits the Session/outbox facts;
        // only delivery may wake the disposable wire projection.
        let operation_id = "lifecycle-test-background-run";
        state
            .session_application()
            .run_admitted_session_for_owner(
                "default",
                AdmitSessionRun {
                    session_id: awaken_session_types::SessionId::from(&session.id),
                    agent_id: "coder".into(),
                    operation_id: operation_id.into(),
                    run_id: session_run_id(
                        &awaken_session_types::SessionId::from(session.id.as_str()),
                        operation_id,
                    ),
                    messages: vec![Message::text(
                        MessageId::session_event_input(&session.id, operation_id),
                        Role::User,
                        "research in the background",
                    )],
                    data_subject_id: None,
                    traceparent: None,
                    execution_requirements: Default::default(),
                    replacement:
                        awaken_session_execution_contract::SessionRunReplacement::PreservePrior,
                },
                Some(Arc::new(NoopSink)),
            )
            .await
            .expect("L1 background application completion");
        // The integration fake commits Runtime truth but has no Dispatch Worker.
        // Apply the same exact-epoch settlement that the production
        // DispatchSettlementObserver performs before queue settlement; this is
        // test transport plumbing, not a second application completion path.
        let active_epochs = state
            .session_application()
            .session(&awaken_session_types::SessionId::from(&session.id))
            .await
            .expect("L1 committed active Session")
            .active_activity_epochs;
        assert_eq!(active_epochs.len(), 1, "L1 has one admitted Run activity");
        state
            .session_application()
            .settle_activity(
                &session.id,
                *active_epochs.iter().next().expect("L1 activity epoch"),
            )
            .await
            .expect("L1 production-equivalent settlement observation");
        assert_eq!(
            state
                .session_application()
                .session(&awaken_session_types::SessionId::from(&session.id))
                .await
                .expect("L1 settled Session")
                .execution,
            SessionExecutionState::Idle
        );

        let bytes = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .expect("L1 aggregate idle closes the already-open SSE")
            .expect("L1 read SSE")
            .to_bytes();
        let sse = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            sse.contains("event: session.thread_status_idle")
                && sse.contains("event: session.status_idle"),
            "L1/E1 child and aggregate completion are broadcast: {sse}"
        );
        assert!(
            webhook
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|fact| fact.object_id == session.id),
            "L1/E2 webhook receives the same durable fact"
        );

        let facts = webhook.0.lock().unwrap().clone();
        let fact = facts
            .iter()
            .find(|fact| fact.object_id == session.id)
            .expect("L2 delivered Session fact")
            .clone();
        let before = event_identity(&state, &session.id);
        delivery.deliver(&fact).await.expect("L2 first replay");
        delivery.deliver(&fact).await.expect("L2 duplicate replay");
        assert_eq!(event_identity(&state, &session.id), before, "L2/E3");

        assert!(
            matches!(
                prepare_managed_lifecycle_delivery(
                    &state,
                    &state.session_application(),
                    None,
                    None
                )
                .map(|prepared| prepared.start(&lifecycle)),
                Err(ManagedLifecycleCompositionError::Notifier(_))
            ),
            "L4/E5 application rejects a second outbox owner before it starts"
        );

        let repository = state.session_application().lifecycle_outbox_handle();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if repository.pending_lifecycle().await.unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("L1/E2 the sole consumer acknowledges successful fanout");
    }
}
