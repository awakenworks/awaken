// @formal surface exclude=test_only
// @formal owner crates/apps/awaken-coordinator/src/managed_lifecycle.rs
// @formal reason: Open-SSE wake through the outbox fan-out with a webhook-only control arm and replay identity checks.
//! T32-C02: an already-open SSE connection learns new lifecycle facts only
//! through the outbox fan-out's Managed receiver, each public frame appears
//! once, and replaying the delivered facts adds no public identity.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};
use awaken_protocol_managed::{ManagedLifecycleFactDelivery, ManagedState, router};
use awaken_protocol_managed_testkit::CoordinatedRuntimeFake;
use awaken_protocol_managed_testkit::ManagedStateFixture as _;
use awaken_session_application::{
    LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL, LifecycleOutboxRelay, SessionRootReads,
};
use awaken_session_application_contract::LifecycleFactNotifier;
use awaken_session_contract::LifecycleFactDelivery;
use awaken_session_contract::ManagedLifecycleFact;
use awaken_session_execution_contract::{AdmitSessionRun, session_run_id};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

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

/// Read data frames until the stream ends or `deadline` passes. Returns the
/// text read and whether the stream ended.
async fn read_until(body: &mut Body, deadline: tokio::time::Instant) -> (String, bool) {
    let mut text = String::new();
    loop {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(frame)) => {
                if let Ok(data) = frame.expect("SSE frame").into_data() {
                    text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Ok(None) => return (text, true),
            Err(_) => return (text, false),
        }
    }
}

/// The public-identity oracle over one SSE transcript: every `id:` appears
/// once and the aggregate idle frame is sent exactly once.
fn duplicate_public_identity(sse: &str) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for id in sse.lines().filter_map(|line| line.strip_prefix("id:")) {
        if !seen.insert(id.trim().to_owned()) {
            return Err(format!("public id {} sent twice", id.trim()));
        }
    }
    let idle = sse.matches("event: session.status_idle").count();
    if idle != 1 {
        return Err(format!("aggregate idle sent {idle} times"));
    }
    Ok(())
}

/// | Rule | Managed receiver in the fan-out | Effect on an open SSE |
/// |---|---|---|
/// | F1 (NC) | absent: webhook-only relay | the webhook gets the facts; the SSE stays open with no idle frame |
/// | F2 | the same facts delivered once to the Managed receiver | child + aggregate idle, each public frame once, stream closes |
/// | F3 | the same facts replayed twice | no new public event identity |
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_sse_is_woken_only_by_the_managed_fanout_and_never_duplicates_a_frame() {
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()).await);
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let application = state.session_application();
    let webhook = Arc::new(RecordingWebhook::default());
    let webhook_only: Arc<dyn LifecycleFactDelivery> = webhook.clone();
    let relay = Arc::new(LifecycleOutboxRelay::new(
        webhook_only,
        application.lifecycle().outbox_handle(),
    ));
    let port: Arc<dyn LifecycleFactNotifier> = relay.clone();
    application
        .lifecycle()
        .set_notifier(port)
        .expect("F1 the sole outbox consumer is webhook-only");
    let replay = relay
        .replay_loop(LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL)
        .expect("F1 replay loop");
    lifecycle.spawn("webhook-only-outbox", move |cancel| replay.run(cancel));

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
        .expect("create Session");
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
        .expect("open SSE");
    assert_eq!(response.status(), StatusCode::OK, "precondition");
    let mut body = response.into_body();

    let operation_id = "fanout-control-background-run";
    let access = SessionRootReads::authorize(
        &*application.queries(),
        "default",
        &awaken_session_types::SessionId::from(&session.id),
    )
    .await
    .expect("the default Workspace owns the Session");
    application
        .run_admissions()
        .run_admitted_session_for_owner(
            &access,
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
                private_task_input: None,
                traceparent: None,
                execution_requirements: Default::default(),
                replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
            },
            Some(Arc::new(NoopSink)),
        )
        .await
        .expect("background application completion");
    let active_epochs = application
        .queries()
        .session(&awaken_session_types::SessionId::from(&session.id))
        .await
        .expect("committed active Session")
        .active_activity_epochs;
    application
        .lifecycle()
        .settle_activity_observed(
            &awaken_session_types::SessionId::from(&session.id),
            *active_epochs.iter().next().expect("one activity epoch"),
            None,
        )
        .await
        .expect("production-equivalent settlement observation");

    let outbox = application.lifecycle().outbox_handle();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if webhook
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|fact| fact.object_id == session.id)
                && outbox.pending_lifecycle().await.unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("F1 the webhook-only consumer delivers and acknowledges");
    let (before, ended) = read_until(
        &mut body,
        tokio::time::Instant::now() + Duration::from_millis(500),
    )
    .await;
    assert!(
        !ended && !before.contains("event: session.status_idle"),
        "F1 without the Managed receiver the open SSE is not refreshed: {before}"
    );

    let facts = webhook
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|fact| fact.object_id == session.id)
        .cloned()
        .collect::<Vec<_>>();
    let managed = ManagedLifecycleFactDelivery::new(&state);
    for fact in &facts {
        managed.deliver(fact).await.expect("F2 Managed delivery");
    }
    let (after, ended) = read_until(
        &mut body,
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await;
    let sse = format!("{before}{after}");
    assert!(ended, "F2 aggregate idle closes the open SSE: {sse}");
    assert!(
        sse.contains("event: session.thread_status_idle"),
        "F2 child idle is broadcast: {sse}"
    );
    duplicate_public_identity(&sse).expect("F2 each public frame once");

    let identity = |state: &ManagedState| {
        state
            .list_events(&session.id, None, None, false)
            .expect("list projected events")
            .data
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>()
    };
    let projected = identity(&state);
    for fact in facts.iter().chain(facts.iter()) {
        managed.deliver(fact).await.expect("F3 replay");
    }
    assert_eq!(identity(&state), projected, "F3 replay adds no identity");

    // NC for the identity oracle: a transcript that repeats a frame.
    let duplicated = format!("{sse}{sse}");
    assert!(
        duplicate_public_identity(&duplicated).is_err(),
        "NC a duplicated frame is detected"
    );
    lifecycle
        .shutdown(Duration::from_secs(2))
        .await
        .expect("bounded shutdown");
}
