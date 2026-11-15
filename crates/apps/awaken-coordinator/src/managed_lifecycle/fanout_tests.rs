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

#[tokio::test]
async fn postgres_two_lifecycle_relays_share_delivery_and_recover_an_interruption() {
    use awaken_reliability_testkit::CrashProcess;
    use awaken_session_contract::LifecycleOutbox as _;
    use awaken_session_store::PostgresManagedSessionRepository;
    use awaken_webhook::{ReqwestSender, WebhookSender as _};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    const CASE: &str = "managed_lifecycle::fanout_tests::postgres_two_lifecycle_relays_share_delivery_and_recover_an_interruption";
    const CHILD_URL: &str = "AWAKEN_OUTBOX_RELAY_CHILD_URL";
    const CHILD_HOOK: &str = "AWAKEN_OUTBOX_RELAY_CHILD_HOOK";

    struct Receiver {
        url: String,
        sender: ReqwestSender,
    }

    #[async_trait::async_trait]
    impl LifecycleFactDelivery for Receiver {
        async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
            let status = self
                .sender
                .post(
                    &self.url,
                    vec![("webhook-id".into(), fact.id.clone())],
                    String::new(),
                )
                .await?;
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(format!("receiver returned {status}"))
            }
        }
    }

    let connect = |url: &str| {
        let options = url.parse::<PgConnectOptions>().unwrap();
        PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
    };
    if let Ok(url) = std::env::var(CHILD_URL) {
        let repo = Arc::new(
            PostgresManagedSessionRepository::with_existing_pool(connect(&url).await.unwrap())
                .await
                .unwrap(),
        );
        let receiver = Arc::new(Receiver {
            url: std::env::var(CHILD_HOOK).unwrap(),
            sender: ReqwestSender::with_timeout(Duration::from_secs(30)),
        });
        LifecycleOutboxRelay::new(receiver, repo)
            .replay_loop(Duration::from_secs(30))
            .unwrap()
            .run(awaken_runtime_contract::CancellationToken::new())
            .await
            .unwrap();
        return;
    }

    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = connect(&url).await.unwrap();
    sqlx::query("DROP SCHEMA IF EXISTS t_lifecycle_relay_replica CASCADE")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA t_lifecycle_relay_replica")
        .execute(&admin)
        .await
        .unwrap();
    let separator = if url.contains('?') { '&' } else { '?' };
    let scoped_url =
        format!("{url}{separator}options=-c%20search_path%3Dt_lifecycle_relay_replica");
    let pool = connect(&scoped_url).await.unwrap();
    let repo = Arc::new(
        PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .unwrap(),
    );

    let attempts = Arc::new(Mutex::new(
        std::collections::BTreeMap::<String, usize>::new(),
    ));
    let app = axum::Router::new().route(
        "/",
        axum::routing::post({
            let attempts = attempts.clone();
            move |headers: axum::http::HeaderMap| {
                let id = headers["webhook-id"].to_str().unwrap().to_owned();
                let attempts = attempts.clone();
                async move {
                    // The first HTTP receiver accepts the effect, then withholds
                    // its response. Stable IDs deduplicate the effect on replay.
                    let first = {
                        let mut attempts = attempts.lock().unwrap();
                        let count = attempts.entry(id.clone()).or_default();
                        *count += 1;
                        *count == 1
                    };
                    if first && id == "blocked" {
                        std::future::pending::<()>().await;
                    }
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let receiver_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let fact = |id: &str| ManagedLifecycleFact {
        id: id.into(),
        object_id: "session".into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.created".into(),
        timestamp: 1,
        runtime_interval: None,
    };

    repo.append_lifecycle(fact("blocked")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("kill");
    let process = CrashProcess::new(CASE, &marker)
        .timeout(Duration::from_secs(20))
        .env(CHILD_URL, &scoped_url)
        .env(CHILD_HOOK, &receiver_url);
    let first = tokio::task::spawn_blocking(move || process.run());
    let wait_for_delivery = |id: &'static str| {
        let attempts = attempts.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(8), async {
                while !attempts.lock().unwrap().contains_key(id) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the receiver observes the expected attempt");
        }
    };
    wait_for_delivery("blocked").await;
    repo.append_lifecycle(fact("healthy")).await.unwrap();
    let peer = Arc::new(Receiver {
        url: receiver_url,
        sender: ReqwestSender::default(),
    });
    let replay = LifecycleOutboxRelay::new(peer, repo.clone())
        .replay_loop(Duration::from_millis(25))
        .unwrap();
    let cancel = awaken_runtime_contract::CancellationToken::new();
    let second = tokio::spawn(replay.run(cancel.clone()));
    wait_for_delivery("healthy").await;
    let before_crash = attempts.lock().unwrap()["blocked"];
    let pending_at_crash = repo.pending_lifecycle().await.unwrap();
    std::fs::write(marker, b"accepted-before-ack").unwrap();
    assert!(!first.await.unwrap().unwrap().success());

    assert_eq!(before_crash, 1, "a live claim prevents overlapping sends");
    assert!(pending_at_crash.iter().any(|fact| fact.id == "blocked"));
    // Test-owned clock cut avoids a 30-second sleep and leaves only the
    // existing claim's deadline changed. The peer must still reclaim it.
    let expired = sqlx::query("UPDATE managed_lifecycle_outbox_claim SET lease_until = '-infinity' WHERE fact_id = 'blocked'")
        .execute(&pool).await.unwrap();
    assert_eq!(expired.rows_affected(), 1);
    tokio::time::timeout(Duration::from_secs(8), async {
        while !repo.pending_lifecycle().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
    assert_eq!(
        *attempts.lock().unwrap(),
        std::collections::BTreeMap::from([("blocked".into(), 2), ("healthy".into(), 1)])
    );
    let ttl = Duration::from_secs(30);
    repo.append_lifecycle(fact("verify-fence")).await.unwrap();
    let original = repo
        .claim_lifecycle("verify-fence", ttl)
        .await
        .unwrap()
        .unwrap();
    assert!(
        repo.renew_lifecycle_claim("verify-fence", &original, ttl)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE managed_lifecycle_outbox_claim SET lease_until = '-infinity' WHERE fact_id = 'verify-fence'")
        .execute(&pool).await.unwrap();
    assert!(
        !repo
            .renew_lifecycle_claim("verify-fence", &original, ttl)
            .await
            .unwrap()
    );
    let replacement = repo
        .claim_lifecycle("verify-fence", ttl)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(original, replacement);
    repo.finish_lifecycle_claim("verify-fence", &original, true)
        .await
        .unwrap();
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        [fact("verify-fence")]
    );
    repo.finish_lifecycle_claim("verify-fence", &replacement, true)
        .await
        .unwrap();
    assert!(repo.pending_lifecycle().await.unwrap().is_empty());
    pool.close().await;
    sqlx::query("DROP SCHEMA t_lifecycle_relay_replica CASCADE")
        .execute(&admin)
        .await
        .unwrap();
}
