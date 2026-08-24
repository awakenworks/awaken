//! One Coordinator composition for Session lifecycle delivery.
//!
//! `WebhookOutboxNotifier` remains the sole durable outbox consumer. Its
//! delivery fan-out includes the optional Control webhook sink and one weak
//! Managed projection receiver; neither receiver reads or acknowledges the
//! outbox independently.

use std::sync::Arc;

use awaken_protocol_managed::{ManagedLifecycleFactDelivery, ManagedState};
use awaken_session_contract::{
    CompositeLifecycleFactDelivery, LifecycleFactDelivery, LifecycleFactNotifier,
};

#[derive(Debug, thiserror::Error)]
pub enum ManagedLifecycleCompositionError {
    #[error("bind Session lifecycle notifier: {0}")]
    Notifier(&'static str),
    #[error("start Session lifecycle outbox supervisor: {0}")]
    Supervisor(&'static str),
}

/// Install the one lifecycle delivery chain before the Managed router is served.
///
/// Product passes its Control webhook delivery; scenario/local composition
/// passes `None`. Both modes share this exact outbox consumer and Managed refresh
/// receiver rather than wiring parallel notification paths.
pub fn install_managed_lifecycle_delivery(
    state: &Arc<ManagedState>,
    webhook: Option<Arc<dyn LifecycleFactDelivery>>,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) -> Result<Arc<ManagedLifecycleFactDelivery>, ManagedLifecycleCompositionError> {
    install_managed_lifecycle_delivery_inner(state, webhook, None, service_lifecycle)
}

/// Install the same lifecycle delivery chain and bind Deployment wake-ups to it.
///
/// This additive entry point preserves the original Session-only composition
/// API. Both wrappers delegate to one implementation and start exactly one
/// durable outbox consumer.
pub fn install_managed_lifecycle_delivery_with_deployments(
    state: &Arc<ManagedState>,
    webhook: Option<Arc<dyn LifecycleFactDelivery>>,
    deployments: &awaken_deployment_application::DeploymentApplication,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) -> Result<Arc<ManagedLifecycleFactDelivery>, ManagedLifecycleCompositionError> {
    install_managed_lifecycle_delivery_inner(state, webhook, Some(deployments), service_lifecycle)
}

fn install_managed_lifecycle_delivery_inner(
    state: &Arc<ManagedState>,
    webhook: Option<Arc<dyn LifecycleFactDelivery>>,
    deployments: Option<&awaken_deployment_application::DeploymentApplication>,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) -> Result<Arc<ManagedLifecycleFactDelivery>, ManagedLifecycleCompositionError> {
    let managed = Arc::new(ManagedLifecycleFactDelivery::new(state));

    let managed_delivery: Arc<dyn LifecycleFactDelivery> = managed.clone();
    let delivery: Arc<dyn LifecycleFactDelivery> = match webhook {
        Some(webhook) => Arc::new(CompositeLifecycleFactDelivery::new(vec![
            webhook,
            managed_delivery,
        ])),
        None => managed_delivery,
    };
    let application = state.session_application();
    let notifier = Arc::new(awaken_webhook_managed::WebhookOutboxNotifier::deferred(
        delivery,
        application.session_repository_handle(),
    ));
    let notifier_port: Arc<dyn LifecycleFactNotifier> = notifier.clone();
    application
        .set_lifecycle_notifier(notifier_port)
        .map_err(ManagedLifecycleCompositionError::Notifier)?;
    if let Some(deployments) = deployments {
        let deployment_notifier: Arc<dyn LifecycleFactNotifier> = notifier.clone();
        deployments.bind_lifecycle_notifier(deployment_notifier);
    }
    notifier
        .start(service_lifecycle)
        .map_err(ManagedLifecycleCompositionError::Supervisor)?;
    Ok(managed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::stream::event::Event as StreamEvent;
    use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};
    use awaken_protocol_managed::test_support::CoordinatedRuntimeFake;
    use awaken_protocol_managed::{ManagedState, StateError, router};
    use awaken_session_contract::{
        LifecycleFactDelivery, ManagedLifecycleFact, SessionExecutionState,
    };
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

    /// Lifecycle wake cause/effect graph: C1 the receiver/application are composed
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
        // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
        // delivery is best-effort and cannot replace committed replay truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        let webhook = Arc::new(RecordingWebhook::default());
        let delivery =
            install_managed_lifecycle_delivery(&state, Some(webhook.clone()), &lifecycle)
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
                None,
            )
            .await
            .expect("L1 create Session");
        let app = router(state.clone());
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
        let outcome = state
            .session_application()
            .run_session_message(
                "coder",
                &session.id,
                vec![ContentBlock::text("research in the background")],
                None,
                Arc::new(NoopSink),
            )
            .await
            .expect("L1 background application completion");
        assert_eq!(outcome.session.execution, SessionExecutionState::Idle);

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
                install_managed_lifecycle_delivery(&state, None, &lifecycle),
                Err(ManagedLifecycleCompositionError::Notifier(_))
            ),
            "L4/E5 application rejects a second outbox owner before it starts"
        );

        let repository = state.session_application().session_repository_handle();
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
