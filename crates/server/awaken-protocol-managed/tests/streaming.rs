// @formal evidence session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
// @formal evidence session.runtime_projection_logical_deletion_requires_positive_exact_scoped_evidence
//! The streaming/preview plane — the crate's biggest coverage hole. The
//! send-then-stream backfill is covered in `adapter.rs`; this suite drives the
//! *live* broadcast path instead:
//!
//! 1. LIVE broadcast ordering: subscribe first, then drive a Run, and assert the
//!    committed frames arrive on the open receiver in Run order.
//! 2. The preview plane end-to-end: the Runtime-owned Thread live subscription
//!    drives root/child `event_start`/`event_delta` projection without re-entering
//!    the committed Session broadcast.
//! 3. `session.thread_status_terminated` driven over HTTP and asserted on the SSE
//!    `event:` name.
//! 4. Replay characterization (`Last-Event-ID` / cursor), the `stream_thread_events`
//!    endpoint, and `Lagged`/`Closed` broadcast handling.
//!
//! The live-broadcast assertions subscribe before durable Event-batch admission,
//! then use the same application reconciliation and committed projector as the
//! HTTP routes. This makes receipt -> Run commit -> projection ordering explicit
//! without a private synchronous Runtime path or timing sleeps.

mod support;

use std::sync::Arc;

use awaken_protocol_managed::ManagedState;
use awaken_protocol_managed::types::Event;
use awaken_protocol_managed_testkit::CoordinatedRuntimeFake;
use awaken_session_execution_contract::SessionAgentCoordination as _;

/// The Managed router as a real deployment mounts it — behind the trusted edge
/// stamp the composition root installs. Handlers fail closed on an absent
/// Workspace, so an unstamped router is not a representative fixture.
fn router(state: Arc<ManagedState>) -> axum::Router {
    support::edge_stamped(awaken_protocol_managed::router(state))
}

async fn independent_replicas() -> (CoordinatedRuntimeFake, Arc<ManagedState>, Arc<ManagedState>) {
    let runtime = CoordinatedRuntimeFake::default();
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let a = Arc::new({
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    });
    let b = Arc::new({
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    });
    (runtime, a, b)
}
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::broadcast::error::TryRecvError;
use tower::ServiceExt;

// --- State-level helpers (deterministic broadcast driving) -------------------

async fn state_create(state: &ManagedState) -> String {
    Box::pin(
        state.create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder",
                "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        ),
    )
    .await
    .expect("create")
    .id
}

async fn state_send_user(
    state: &Arc<ManagedState>,
    runtime: &CoordinatedRuntimeFake,
    id: &str,
    text: &str,
) {
    let req = serde_json::from_value(serde_json::json!({
        "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
    }))
    .unwrap();
    Box::pin(state.send_events(id, req)).await.expect("send");
    Box::pin(support::drive_completed_user_dispatches(
        state,
        id,
        &runtime.completed_user_dispatches(),
    ))
    .await;
    // GET owns warm projection in production. Calling the route after the
    // committed Run keeps this integration helper on that same projector and
    // publishes its appended suffix to an already-open receiver.
    let app = router(state.clone());
    let _ = Box::pin(http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    ))
    .await;
}

async fn state_send_interrupts(state: &Arc<ManagedState>, id: &str, total: usize) {
    // Keep each batch within the supervisor's 50-step slice so every receipt
    // crosses the same durable admission/projector path used in production.
    let mut remaining = total;
    while remaining > 0 {
        let batch_size = remaining.min(50);
        let events = (0..batch_size)
            .map(|_| serde_json::json!({ "type": "user.interrupt" }))
            .collect::<Vec<_>>();
        let req = serde_json::from_value(serde_json::json!({ "events": events })).unwrap();
        state
            .send_events(id, req)
            .await
            .expect("batch of processed interrupts");
        remaining -= batch_size;
    }
}

fn sse_committed_ids(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .filter_map(|event| event["id"].as_str().map(str::to_owned))
        .collect()
}

/// Drain every frame currently buffered on `rx` (all frames are published
/// before this call through the awaited admission/reconciliation/projection
/// sequence). Returns the frames and how many `Lagged` gaps were observed.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> (Vec<Event>, usize) {
    let mut frames = Vec::new();
    let mut lagged = 0;
    loop {
        match rx.try_recv() {
            Ok(frame) => frames.push(frame),
            Err(TryRecvError::Lagged(_)) => lagged += 1,
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    (frames, lagged)
}

fn committed_types(frames: &[Event]) -> Vec<&'static str> {
    frames.iter().map(Event::type_str).collect()
}

// --- HTTP helpers ------------------------------------------------------------

async fn http_json(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let resp = Box::pin(app.clone().oneshot(req)).await.unwrap();
    let status = resp.status();
    let bytes = Box::pin(resp.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{method} {uri}: {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// GET an SSE endpoint and collect the whole (terminating) body as a string.
async fn http_sse(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    mut preview_consumed: Option<Arc<tokio::sync::Notify>>,
) -> support::Call<String> {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let resp = Box::pin(app.clone().oneshot(b.body(Body::empty()).unwrap()))
        .await
        .unwrap();
    let status = resp.status();
    let mut body = resp.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = Box::pin(body.frame()).await {
        let frame = frame.expect("SSE body remains valid through its exact terminal");
        if let Some(data) = frame.data_ref() {
            bytes.extend_from_slice(data);
            if preview_consumed.is_some() {
                let preview_count = String::from_utf8_lossy(&bytes)
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                    .filter(|event| event["type"] == "event_delta")
                    .count();
                if preview_count == 2 {
                    preview_consumed.take().unwrap().notify_one();
                }
            }
        }
    }

    support::Call::new(status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn http_create(app: &Router) -> String {
    http_json(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({
            "agent": "coder",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
        }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn http_primary_thread_id(app: &Router, session_id: &str) -> String {
    http_json(
        app,
        "GET",
        &format!("/v1/sessions/{session_id}/threads"),
        serde_json::Value::Null,
    )
    .await["data"]
        .as_array()
        .expect("Thread list data")
        .iter()
        .find(|thread| thread["parent_thread_id"].is_null())
        .and_then(|thread| thread["id"].as_str())
        .expect("primary Thread")
        .to_string()
}

// === 1(a) LIVE broadcast ordering ===========================================

/// Causes: C1 a subscriber opens before durable admission; C2 the shared Runtime
/// commits one primary Run plus its coordinated child Run; C3 GET invokes the
/// sole projector after application settlement. Effects: E1 the receipt is live;
/// E2 aggregate, primary, child, messages, usage, and idle follow in committed
/// order exactly once; E3 no lag; E4 the same accepted input id is enriched
/// only after its Run settles. Decision rule L1=C1+C2+C3=>E1+E2+E3+E4.
#[tokio::test]
async fn live_broadcast_delivers_one_runs_committed_frames_in_order() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `live broadcast delivers one runs committed frames in order` is one
    // independent branch selecting `all output, state, side-effect, error, and terminal assertions
    // below hold together`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let id = state_create(&state).await;

    // Subscribe first: a fresh session has no committed events, so nothing is
    // backfilled and every frame below arrives live.
    let (snapshot, mut rx) = state.stream_subscribe(&id).expect("subscribe");
    assert!(snapshot.is_empty(), "a fresh session backfills nothing");

    state_send_user(&state, &runtime, &id, "hi").await;

    let (frames, lagged) = drain(&mut rx);
    assert_eq!(lagged, 0, "no lag on a single small Run");
    assert_eq!(
        committed_types(&frames),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "agent.tool_use",
            "agent.tool_result",
            "session.thread_created",
            "session.thread_status_running",
            "agent.thread_message_sent",
            "agent.thread_message_received",
            "session.thread_status_idle",
            "session.thread_status_idle",
            "user.message",
            "session.usage",
            "session.status_idle"
        ],
        "the Run's committed frames arrive live, in order"
    );
    // L1 the full committed reducer publishes one accepted identity first;
    // the same id is enriched after the Run commits. A second User command is
    // never synthesized and the accepted id remains the listable GET cursor.
    let user_frames = frames
        .iter()
        .filter(|event| event.type_str() == "user.message")
        .collect::<Vec<_>>();
    assert_eq!(user_frames.len(), 2, "L1 acceptance and completion");
    assert_eq!(
        user_frames[0].id, user_frames[1].id,
        "L1 exact receipt identity"
    );
    assert!(
        user_frames[0].processed_at.is_none() && user_frames[1].processed_at.is_some(),
        "L1 monotonic completion"
    );
    assert_eq!(
        serde_json::to_value(&user_frames[0].kind).unwrap(),
        serde_json::to_value(&user_frames[1].kind).unwrap(),
        "L1 unchanged authored payload"
    );
}

/// Causes: C1 one receiver remains open; C2 two Event batches commit consecutive
/// primary/child Runs; C3 the second Run targets the existing child. Effects: E1
/// two aggregate/primary/child brackets arrive back-to-back; E2 child creation is
/// emitted only by the first Run; E3 every cross-post appears once. Decision rule
/// L2=C1+C2+C3=>E1+E2+E3.
#[tokio::test]
async fn live_broadcast_preserves_order_across_two_runs() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `live broadcast` is one independent branch selecting `preserves order
    // across two runs`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    state_send_user(&state, &runtime, &id, "one").await;
    state_send_user(&state, &runtime, &id, "two").await;

    let (frames, _) = drain(&mut rx);
    assert_eq!(
        committed_types(&frames),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "agent.tool_use",
            "agent.tool_result",
            "session.thread_created",
            "session.thread_status_running",
            "agent.thread_message_sent",
            "agent.thread_message_received",
            "session.thread_status_idle",
            "session.thread_status_idle",
            "user.message",
            "session.usage",
            "session.status_idle",
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "agent.tool_use",
            "agent.tool_result",
            "session.thread_status_running",
            "agent.thread_message_sent",
            "agent.thread_message_received",
            "session.thread_status_idle",
            "session.thread_status_idle",
            "user.message",
            "session.usage",
            "session.status_idle",
        ],
        "both Runs' brackets arrive live, in commit order"
    );
}

#[tokio::test]
async fn accepted_followup_running_keeps_its_get_cursor_when_child_run_commits_later() {
    // Causes: F1 the first child Run completed; F2 a follow-up send/result and
    // exact child activity admission commit while its new Running lifecycle is
    // absent; F3 GET publishes that prefix; F4 the child Running commits at the
    // next source cursor; F5 a cold replica reads the full prefix. Effects:
    // E1 F2+F3 emits one coordination Running before MessageSent; E2 F4 adds
    // no second Running or rekeys/withdraws any issued Event; E3 F5 and every
    // old cursor return exactly the same suffix. Decision rows R1=F1+F2+F3
    // =>E1; R2=R1+F4=>E1+E2; R3=R2+F5=>E3. Both reads use the production GET
    // projector; the fake assigns the delayed commit a fresh, later cursor.
    use awaken_session_execution_contract::SessionAgentCoordination as _;

    let runtime = CoordinatedRuntimeFake::default();
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let state = Arc::new({
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    });
    let id = state_create(&state).await;
    state_send_user(&state, &runtime, &id, "first").await;
    let prior = state.list_events(&id, None, None, false).unwrap().data;
    let old_cursor = prior.last().expect("F1 committed first Run").id.clone();
    let (_snapshot, mut receiver) = state.stream_subscribe(&id).unwrap();

    runtime.defer_child_completion();
    runtime.defer_next_child_opening();
    let request = serde_json::from_value(serde_json::json!({
        "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "follow up" }] }]
    }))
    .unwrap();
    state.send_events(&id, request).await.unwrap();
    support::drive_retained_session_events(&state, &id).await;
    let admitted = state
        .session_application()
        .admit_session_run_activity(
            &awaken_session_types::SessionId::from(&id),
            "researcher",
            &awaken_agent_contract::agent::run::Id("coord-run-2".into()),
            awaken_session_execution_contract::SessionRunActivityAdmissionMode::RecoverOrAdmit,
        )
        .await
        .unwrap();
    assert!(
        matches!(
            admitted,
            awaken_session_execution_contract::SessionRunActivityAdmission::Admitted { .. }
        ),
        "F2 exact child activity receipt"
    );
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;
    let app = router(state.clone());
    let before_open = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let before_events = before_open["data"].as_array().unwrap();
    let issued = &before_events[prior.len()..];
    let sent = issued
        .iter()
        .position(|event| event["type"] == "agent.thread_message_sent")
        .expect("R1 accepted follow-up is listable");
    assert_eq!(
        issued[sent - 1]["type"],
        "session.thread_status_running",
        "R1/E1 accepted send reopens the Idle child before its message"
    );
    let issued_suffix = state
        .list_events(&id, Some(&old_cursor), None, false)
        .unwrap();
    assert_eq!(
        issued_suffix.data.len(),
        issued.len(),
        "R1 exact old cursor"
    );
    let (published, lagged) = drain(&mut receiver);
    assert_eq!(lagged, 0, "R1 no delivery gap");
    assert!(
        !published.is_empty(),
        "R1 live subscriber receives the prefix"
    );

    runtime.commit_deferred_child_opening();
    let after_open = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        after_open["data"], before_open["data"],
        "R2/E2 exact prefix"
    );
    assert!(
        drain(&mut receiver).0.is_empty(),
        "R2 no duplicate broadcast"
    );

    let cold = Arc::new({
        let session_fixture_runtime = runtime;
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    });
    let cold_events = http_json(
        &router(cold.clone()),
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(cold_events["data"], before_open["data"], "R3/E3 cold");
    assert_eq!(
        serde_json::to_value(
            cold.list_events(&id, Some(&old_cursor), None, false)
                .unwrap()
                .data
        )
        .unwrap(),
        serde_json::to_value(issued_suffix.data).unwrap(),
        "R3/E3 old cursor"
    );
}

// === 2 Preview plane: Runtime Thread live source =============================

/// Root live-preview cause/effect graph: C1 a Session SSE opts into
/// `agent.message` before the Run; C2 two exact root observations share one
/// Run/Step/response coordinate; C3 the Session broadcaster is observed in
/// parallel; C4 a committed terminal closes SSE. Effects: E1 C2 emits one start
/// and both deltas immediately; E2 all frames share one stable id; E3 C3 receives
/// no Preview frame; E4 C4 ends the response after the previews.
///
/// | Rule | Subscription | Coordinate | Source | Effect |
/// |---|---|---|---|---|
/// | R1 | opted in | exact root, first chunk | Thread live | E1,E2 |
/// | R2 | opted in | exact root, repeated chunk | Thread live | E1,E2 |
/// | R3 | open | exact root | Session broadcast | E3 |
/// | R4 | open | committed terminal | Session broadcast | E4 |
#[tokio::test]
async fn root_stream_immediately_projects_thread_live_observations_only() {
    // Causes: the fixtures below establish `root stream immediately` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    let (_snapshot, mut committed_rx) = state.stream_subscribe(&id).expect("R3 subscribe");
    let stream_app = app.clone();
    let uri = format!("/v1/sessions/{id}/events/stream?event_deltas[]=agent.message");
    let consumed = Arc::new(tokio::sync::Notify::new());
    let body_consumed = consumed.clone();
    let stream =
        tokio::spawn(async move { http_sse(&stream_app, &uri, &[], Some(body_consumed)).await });
    runtime.wait_for_live_subscription().await;

    runtime.publish_root_live_text(&id, "root ");
    runtime.publish_root_live_text(&id, "progress");
    // R1/R2 must be observed in the real HTTP body before the terminal is
    // submitted; subscription creation alone is not preview delivery evidence.
    tokio::time::timeout(std::time::Duration::from_secs(2), consumed.notified())
        .await
        .expect("R1-R2 actual preview deltas arrive before deletion");
    assert!(
        matches!(committed_rx.try_recv(), Err(TryRecvError::Empty)),
        "R3/E3"
    );
    let deleted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK, "R4/E4");

    let sse = stream.await.unwrap().expect(StatusCode::OK);
    let data = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .collect::<Vec<_>>();
    let start = data
        .iter()
        .find(|event| event["type"] == "event_start")
        .expect("R1/E1 start");
    let preview_id = start["event"]["id"].as_str().expect("R1/E2 id");
    let deltas = data
        .iter()
        .filter(|event| event["type"] == "event_delta")
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 2, "R1-R2/E1");
    assert!(
        deltas.iter().all(|event| event["event_id"] == preview_id),
        "R1-R2/E2"
    );
    assert!(
        sse.contains("root ") && sse.contains("progress"),
        "R1-R2/E1"
    );
    assert!(
        sse.find("event: event_delta").is_some_and(|preview| {
            sse.find("event: session.deleted")
                .is_some_and(|terminal| preview < terminal)
        }),
        "R4/E4 sse:\n{sse}"
    );
}

// === 3 child Thread isolation and durable archive ===========================

/// Causes: C1 one real child Thread has committed created/running/message/idle
/// facts; C2 primary and child SSE backfill the same disposable event log while
/// parent-only creation remains absent from the child stream; C3 the
/// parent-partition command commits its absorbing archive disposition. Effects:
/// E1 child SSE sees
/// its own lifecycle and reverses cross-post direction without leaking primary
/// Session status; E2 archive returns the terminated Thread and the unique
/// projector commits one terminal fact.
/// Decision table: S1=C1+C2=>E1; S2=C1+C3=>E2.
// Test design: child_thread_sse_is_isolated_and_archive_projects_durable_terminal
// Cause/effect graph: child SSE admits only child facts; archive appends one replayable terminal frame.
// Decision table: matching child=ordered frames; root/sibling=excluded; archive=one terminal; unknown=404.
#[tokio::test]
async fn child_thread_sse_is_isolated_and_archive_projects_durable_terminal() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;

    // A Run delegates once, creating its stable child Thread.
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "go" }] }] }),
    )
    .await;
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;
    let threads = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    let primary_id = threads["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["parent_thread_id"].is_null())
        .and_then(|thread| thread["id"].as_str())
        .expect("S1 public primary Thread");
    assert!(primary_id.starts_with("sthr_"), "S1 public Thread id");
    let child_id = threads["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["parent_thread_id"] == primary_id)
        .expect("a child thread was spawned")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let primary_sse = http_sse(&app, &format!("/v1/sessions/{id}/events/stream"), &[], None)
        .await
        .expect_for(StatusCode::OK, "S1");
    let child_idle = primary_sse
        .find("event: session.thread_status_idle")
        .expect("S1 child idle remains observable on primary");
    let aggregate_idle = primary_sse
        .find("event: session.status_idle")
        .expect("S1 child idle must not truncate primary before aggregate idle");
    assert!(
        child_idle < aggregate_idle,
        "S1 primary_sse:\n{primary_sse}"
    );
    assert!(
        primary_sse.contains("event: session.usage"),
        "S1 trailing aggregate telemetry survives child idle — sse:\n{primary_sse}"
    );

    let child_sse = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{child_id}/stream"),
        &[],
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        !child_sse.contains("event: session.status_")
            && !child_sse.contains("event: session.thread_created")
            && child_sse.contains("event: session.thread_status_running")
            && child_sse.contains("event: session.thread_status_idle"),
        "S1 child lifecycle is isolated from aggregate Session status — sse:\n{child_sse}"
    );
    assert!(
        child_sse.contains("event: agent.thread_message_received")
            && child_sse.contains("event: agent.thread_message_sent"),
        "S1 child stream projects both message directions from its perspective — sse:\n{child_sse}"
    );

    let archive = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{id}/threads/{child_id}/archive"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(archive.status(), StatusCode::OK, "S2/E2");
    let events = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(
        events["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "session.thread_status_terminated")
            .count()
            == 1,
        "S2/E2"
    );
    let archived_child_sse = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{child_id}/stream"),
        &[],
        None,
    )
    .await
    .expect_for(StatusCode::OK, "S2/E2");
    assert!(
        archived_child_sse
            .find("event: session.thread_status_idle")
            .is_some_and(|idle| {
                archived_child_sse
                    .find("event: session.thread_status_terminated")
                    .is_some_and(|terminated| idle < terminated)
            }),
        "S2/E2 child replay closes with durable terminal — sse:\n{archived_child_sse}"
    );
}

/// Causes: C1 the shared Runtime has committed and projected a coordinated Run;
/// C2 a receiver opens after that prefix; C3 the idle child's durable disposition
/// changes Active -> Archived. Effects: E1 C2 has no live replay of the historical
/// prefix; E2 C3 publishes exactly one terminal frame; E3 list/backfill and the
/// Runtime archive authority each contain one transition. Decision rule
/// B1=C1+C2+C3=>E1+E2+E3.
#[tokio::test]
async fn durable_child_archive_broadcasts_and_backfills_one_terminal_frame() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `durable child archive broadcasts and backfills one terminal frame` is
    // one independent branch selecting `all output, state, side-effect, error, and terminal
    // assertions below hold together`; a multi-row decision table is not applicable, and sibling
    // tests own alternate causes.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let id = state_create(&state).await;
    // Project the delegation, then subscribe: the returned snapshot owns backfill
    // while this receiver contains only facts committed after subscription.
    state_send_user(&state, &runtime, &id, "go").await;
    let (snapshot, mut rx) = state.stream_subscribe(&id).expect("subscribe");
    assert!(
        !snapshot.is_empty(),
        "B1/E1 committed prefix is backfill-only"
    );
    assert!(
        matches!(rx.try_recv(), Err(TryRecvError::Empty)),
        "B1/E1 receiver starts caught up"
    );

    let child_id = CoordinatedRuntimeFake::CHILD_THREAD_ID;
    state
        .archive_thread(&id, child_id)
        .await
        .expect("B1 durable archive succeeds");

    // The committed disposition is projected to an already-open stream.
    let (frames, _) = drain(&mut rx);
    assert_eq!(
        committed_types(&frames)
            .into_iter()
            .filter(|kind| *kind == "session.thread_status_terminated")
            .count(),
        1,
        "B1 live"
    );
    // Reconnect/backfill observes the same committed projection once.
    let list = state.list_events(&id, None, None, false).expect("list");
    assert_eq!(
        list.data
            .iter()
            .filter(|event| event.type_str() == "session.thread_status_terminated")
            .count(),
        1,
        "B1 backfill"
    );
    assert_eq!(runtime.archive_commits().len(), 1, "B1 durable transition");
}

// === 1(c) stream_thread_events endpoint =====================================

/// Causes: C1 a completed root Run has committed output; C2 the client selects
/// the listed public primary Thread id; C3 SSE backfills the committed prefix.
/// Effects: E1 root Running/Idle bracket the output on the Thread stream; E2 all
/// status payloads reuse C2's `sthr_` id; E3 the aggregate Idle closes SSE.
/// Decision table: P1(C1+C2+C3)->E1+E2+E3. Unknown/stale selectors are U1/U2
/// in the rejection test below.
// Test design: stream_thread_events_backfills_the_primary_thread
// Cause/effect graph: primary Thread SSE backfills committed root facts then appends ordered live facts.
// Decision table: primary=backfill+live; child=excluded; reconnect=full replay; unknown=404.
#[tokio::test]
async fn stream_thread_events_backfills_the_primary_thread() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;
    // Codec integration rule P1: a listed public root is `sthr_` and selects
    // the internal Session root on the per-Thread stream route. A fabricated
    // internal/sentinel id is covered by the unknown-Thread rejection below.
    let primary_id = http_primary_thread_id(&app, &id).await;
    assert!(primary_id.starts_with("sthr_"), "P1");

    let sse = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{primary_id}/stream"),
        &[],
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        sse.contains("event: session.thread_status_running"),
        "P1/E1 sse:\n{sse}"
    );
    assert!(sse.contains("event: agent.message"), "sse:\n{sse}");
    assert!(
        sse.contains("event: session.thread_status_idle"),
        "P1/E1 sse:\n{sse}"
    );
    assert!(
        sse.contains(&format!(r#""session_thread_id":"{primary_id}""#)),
        "P1/E2 sse:\n{sse}"
    );
    assert!(
        sse.contains("event: session.status_idle"),
        "P1/E3 sse:\n{sse}"
    );
}

/// Causes: C1 a coordinated Run has terminal committed truth; C2 the listed
/// primary Thread selects `event_deltas[]=agent.message`. Effects: E1 C2 is
/// admitted through the shared parser; E2 committed aggregate Idle terminates
/// the body without requiring a live delta. Decision rule O1=C1+C2=>E1+E2;
/// unsupported values are covered by E4 below.
#[tokio::test]
async fn stream_thread_events_accepts_the_preview_opt_in() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `stream thread events` is one independent branch selecting `accepts the
    // preview opt in`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;
    let primary_id = http_primary_thread_id(&app, &id).await;
    let _ = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{primary_id}/stream?event_deltas[]=agent.message"),
        &[],
        None,
    )
    .await
    .expect(StatusCode::OK);
}

/// Managed child live-preview cause/effect table:
/// C0 the reserved Event batch is activated, then the completion-woken
/// supervisor turn anchors its initial committed prefix and exposes the real
/// child selector; the child Run then obtains its own Session activity receipt
/// before writing. C1 exact child text is followed by a tool
/// delta (ordinary-message proof); C2 child
/// stream opts into `agent.message`; C3 child reasoning is observed; C4
/// primary/Session share only the committed broadcast; C5 later text becomes the
/// terminal report; C6 ordinary Message and terminal lifecycle commit. Effects:
/// E0 C0 lets both subscriptions start from the same caught-up prefix;
/// E1 C1+C2 emits child-only start/delta; E2 C3 emits nothing; E3 C4 receives no
/// child preview; E4 C5 emits no preview and commits only child sent/primary
/// received; E5 C6 commits the ordinary `agent.message` with E1's id, settles the
/// application-owned child activity epoch at its exact terminal observation,
/// then projects child and aggregate Idle. The root's settlement alone cannot
/// close the interval while that admitted child remains active.
///
/// | Rule | Ordinary proof | Terminal report | Stream | Effects |
/// |---|---|---|---|---|
/// | L0 | n/a | no | committed projector | E0 |
/// | L1 | tool | no | child opted-in | E1,E2,E5 |
/// | L2 | tool | no | primary/Session | E3 |
/// | L3 | absent | yes | child opted-in | E2,E4 |
#[tokio::test]
async fn child_stream_previews_only_ordinary_text_and_never_the_terminal_report() {
    // Causes: the fixtures below establish `child stream previews only ordinary text and` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    runtime.defer_child_completion();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.message",
                "content": [{ "type": "text", "text": "coordinate" }]
            }]
        }),
    )
    .await;
    support::drive_retained_session_events(&state, &id).await;
    support::drive_retained_session_events(&state, &id).await;
    // Production Host/Run ingress admits each coordinated child here before
    // it can commit messages. The Runtime fake's link is topology, not a
    // Session activity receipt.
    let admitted = state
        .session_application()
        .admit_session_run_activity(
            &awaken_session_types::SessionId::from(&id),
            "researcher",
            &awaken_agent_contract::agent::run::Id("coord-run-1".into()),
            awaken_session_execution_contract::SessionRunActivityAdmissionMode::RecoverOrAdmit,
        )
        .await
        .unwrap();
    let awaken_session_execution_contract::SessionRunActivityAdmission::Admitted {
        session_activity_epoch: child_epoch,
    } = admitted
    else {
        panic!("L0 child Run must own its committed Session activity");
    };

    let child_id = CoordinatedRuntimeFake::CHILD_THREAD_ID;
    let threads = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    assert!(
        threads["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["id"] == child_id),
        "L0/E0 canonical projection exposes the real child selector: {threads}"
    );
    let (snapshot, mut session_frames) = state.stream_subscribe(&id).unwrap();
    assert!(!snapshot.is_empty(), "L0/E0 committed prefix is backfilled");
    let child_app = app.clone();
    let child_uri =
        format!("/v1/sessions/{id}/threads/{child_id}/stream?event_deltas[]=agent.message");
    let child_stream =
        tokio::spawn(async move { http_sse(&child_app, &child_uri, &[], None).await });
    runtime.wait_for_live_subscription().await;
    // The child route's repeated refresh is idempotent, so the already-caught-up
    // Session receiver remains empty before connection-local previews arrive.
    let (committed_prefix, lagged) = drain(&mut session_frames);
    assert_eq!(lagged, 0, "L2 setup");
    assert!(committed_prefix.is_empty(), "L0/E0 no duplicate projection");

    runtime.publish_child_live_reasoning("private chain");
    runtime.publish_child_live_text("live progress");
    runtime.publish_child_live_tool();
    assert!(
        matches!(session_frames.try_recv(), Err(TryRecvError::Empty)),
        "L2/E3 child preview never enters the Session/primary broadcast"
    );

    runtime.commit_child_ordinary_message("live progress");
    let _ = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    runtime.publish_child_live_reasoning("terminal private chain");
    runtime.publish_child_live_text("terminal report");
    let child_boundary = runtime.complete_deferred_child("terminal report", admitted);
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;
    state
        .session_application()
        .lifecycle()
        .settle_activity_observed(
            &awaken_session_types::SessionId::from(&id),
            child_epoch,
            Some(child_boundary),
        )
        .await
        .unwrap();
    let terminal_events = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let terminal_types = terminal_events["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert!(
        terminal_types.contains(&"session.thread_status_idle")
            && terminal_types.contains(&"session.status_idle"),
        "L1,L3/E5 canonical projector closes child and aggregate: {terminal_types:?}"
    );
    let sse = child_stream.await.unwrap().expect(StatusCode::OK);
    assert!(sse.contains("event: event_start"), "L1/E1 sse:\n{sse}");
    assert!(sse.contains("event: event_delta"), "L1/E1 sse:\n{sse}");
    assert!(sse.contains("live progress"), "L1/E1");
    assert!(!sse.contains("private chain"), "L1,L3/E2");
    assert_eq!(
        sse.matches("terminal report").count(),
        1,
        "L3/E4 report appears only in committed thread_message_sent"
    );
    assert!(sse.contains("event: agent.message"), "L1/E5 sse:\n{sse}");
    assert!(
        sse.contains("event: agent.thread_message_sent"),
        "L3/E4 sse:\n{sse}"
    );
    assert!(
        sse.contains("event: session.thread_status_idle"),
        "L1,L3/E5 sse:\n{sse}"
    );

    let data = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .collect::<Vec<_>>();
    let preview_id = data
        .iter()
        .find(|event| event["type"] == "event_start")
        .and_then(|event| event["event"]["id"].as_str())
        .expect("L1 preview id");
    let committed_id = data
        .iter()
        .find(|event| event["type"] == "agent.message")
        .and_then(|event| event["id"].as_str())
        .expect("L1 committed id");
    assert_eq!(preview_id, committed_id, "L1/E5 reconciliation");

    let primary_id = http_primary_thread_id(&app, &id).await;
    let primary = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{primary_id}/stream?event_deltas[]=agent.message"),
        &[],
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(!primary.contains("event: event_start"), "L2/E3");
    assert!(!primary.contains("event: event_delta"), "L2/E3");
    assert!(
        primary.contains("event: agent.thread_message_received"),
        "L3/E4"
    );
}

/// Causes: C1 an arbitrary unknown Thread id; C2 the retired internal primary
/// sentinel. Effect E1: both fail closed with 404 rather than aliasing the
/// Session root. Decision table: U1(C1)->E1; U2(C2)->E1.
#[tokio::test]
async fn stream_thread_events_unknown_thread_is_404() {
    // Effects: the observable result `is 404` and every asserted state transition or side effect
    // must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let app = router(Arc::new(
        support::state(CoordinatedRuntimeFake::default()).await,
    ));
    let id = http_create(&app).await;
    for thread_id in ["nope".to_string(), format!("{id}:primary")] {
        let _ = http_sse(
            &app,
            &format!("/v1/sessions/{id}/threads/{thread_id}/stream"),
            &[],
            None,
        )
        .await
        .expect_for(StatusCode::NOT_FOUND, "expected status");
    }
}

/// Causes: C1 an `event_deltas[]` member is supported/unsupported; C2 its repeated
/// count is at/beyond 100. Constraint: only message/thinking are valid and the
/// inclusive maximum is 100. Effects: E1 a supported 100-value request streams;
/// E2 an invalid value or 101 values returns 400 `invalid_request_error` before
/// opening a stream. Decision table: D1=supported+100=>E1;
/// D2=unsupported+any=>E2; D3=supported+101=>E2.
#[tokio::test]
async fn session_stream_event_deltas_follow_the_boundary_decision_rule() {
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;

    let repeated = |count: usize| {
        std::iter::repeat_n("event_deltas[]=agent.message", count)
            .collect::<Vec<_>>()
            .join("&")
    };
    let _ = http_sse(
        &app,
        &format!("/v1/sessions/{id}/events/stream?{}", repeated(100)),
        &[],
        None,
    )
    .await
    .expect_for(StatusCode::OK, "E4 inclusive maximum");

    for (rule, query) in [
        (
            "E4 unsupported value",
            "event_deltas[]=agent.tool_use".to_string(),
        ),
        ("E4 over maximum", repeated(101)),
    ] {
        let body = http_sse(
            &app,
            &format!("/v1/sessions/{id}/events/stream?{query}"),
            &[],
            None,
        )
        .await
        .expect_for(StatusCode::BAD_REQUEST, "expected status");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["error"]["type"], "invalid_request_error", "{rule}");
    }
}

// === 1(b) Full-replay + dedupe-by-id contract ===============================

/// Causes: C1 the shared Runtime has committed one coordinated Run; C2 reconnect
/// supplies the id of the last committed event as `Last-Event-ID`. Effects: E1
/// the stream still returns the full aggregate Running/message/Idle snapshot; E2
/// no committed fact is omitted. Decision rule F1=C1+C2=>E1+E2 because this API's
/// replay contract deliberately ignores incremental resume and relies on id
/// dedupe at the client.
#[tokio::test]
async fn the_stream_full_replays_and_ignores_last_event_id() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `the stream full replays and ignores last event id` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;

    // Discover the final id from the canonical chronological event list.
    let list = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let events = list["data"].as_array().unwrap();
    let last_id = events.last().unwrap()["id"].as_str().unwrap().to_string();

    // Reconnect claiming to have already seen the LAST event: an incremental resume
    // would send nothing after it, but the full snapshot is replayed regardless.
    let sse = http_sse(
        &app,
        &format!("/v1/sessions/{id}/events/stream"),
        &[("last-event-id", &last_id)],
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        sse.contains("event: session.status_running")
            && sse.contains("event: agent.message")
            && sse.contains("event: session.status_idle"),
        "the full committed log is replayed, Last-Event-ID notwithstanding — sse:\n{sse}"
    );
}

// === 1(d) Lagged / Closed broadcast handling ================================

/// Causes: C1 an open receiver does not drain; C2 bounded durable batches
/// publish 65 processed interrupt receipts, exceeding the test-support
/// 64-frame channel (production retains 1,024); C3 canonical Run
/// reconciliation/projector publishes terminal facts
/// afterward. Effects: E1 the
/// receiver reports a lag gap; E2 later `session.status_idle` remains observable.
/// Decision table: G1=C1+receipts<=1024=>no required gap (boundary owned by the
/// channel); G2=C1+C2+C3=>E1+E2 (covered here).
#[tokio::test]
async fn a_lagging_subscriber_skips_frames_but_still_receives_later_ones() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    // Admission owns every receipt, while only a processed/anchored receipt is
    // a replayable live frame. Interrupts are immediate receipt commands; keep
    // each batch within the supervisor's 50-step slice so all 65 facts cross
    // the same durable projection path instead of relying on unanchored User
    // inputs that are intentionally withheld from the public event prefix.
    state_send_interrupts(&state, &id, 65).await;
    state_send_user(&state, &runtime, &id, "after lag").await;

    let (frames, lagged) = drain(&mut rx);
    assert!(
        lagged >= 1,
        "the receiver observed a Lagged gap; retained frames={}",
        frames.len()
    );
    assert!(
        committed_types(&frames).contains(&"session.status_idle"),
        "later committed frames (idle) still arrive despite the lag"
    );
}

/// Cause/effect graph: C1 24 HTTP SSE responses are opened and their bodies are
/// deliberately not polled; C2 65 committed receipts exceed the test channel's
/// 64-frame capacity; C3 a terminal Run commits; C4 every client reconnects.
/// Effects: E1 C1/C2 do not block admission or terminal commit; E2 every slow
/// body catches the later Idle and closes within budget; E3 every reconnect full
/// replays the canonical committed ids exactly once. Decision table:
/// R1=C1+C2&&!C3=>lag allowed, stream open; R2=C1+C2+C3=>E1+E2;
/// R3=C1+C2+C3+C4=>E1+E2+E3. Durable Session events remain replay authority;
/// the broadcast channel is only a best-effort tail.
#[tokio::test]
async fn slow_http_consumers_and_reconnect_storm_recover_from_durable_replay() {
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    let uri = format!("/v1/sessions/{id}/events/stream");
    let mut slow_bodies = Vec::new();
    for _ in 0..24 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("open slow SSE response");
        assert_eq!(response.status(), StatusCode::OK);
        slow_bodies.push(response.into_body());
    }

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        state_send_interrupts(&state, &id, 65).await;
        state_send_user(&state, &runtime, &id, "terminal after slow consumers").await;
    })
    .await
    .expect("R2/E1 slow consumers never backpressure committed admission");

    for (index, body) in slow_bodies.into_iter().enumerate() {
        let bytes =
            tokio::time::timeout(std::time::Duration::from_secs(2), Box::pin(body.collect()))
                .await
                .unwrap_or_else(|_| panic!("R2/E2 slow body {index} did not terminate"))
                .expect("collect slow SSE body")
                .to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("SSE is UTF-8");
        assert!(
            body.contains("event: session.status_idle"),
            "R2/E2 slow body {index} catches the terminal suffix"
        );
        // R2 requires every durable identity, not merely the buffered Idle
        // suffix. These bodies opened before 65 receipts overflowed the
        // receiver; skipping Lagged without index catch-up fails this rule.
        let expected = state
            .list_events(&id, None, Some(100), false)
            .unwrap()
            .data
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        assert_eq!(
            sse_committed_ids(&body),
            expected,
            "R2 complete lag recovery {index}"
        );
    }

    let canonical = state
        .list_events(&id, None, Some(100), false)
        .expect("canonical replay list")
        .data
        .into_iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    assert!(
        !canonical.is_empty(),
        "the terminal Run produces a durable replay prefix"
    );
    for reconnect in 0..24 {
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            http_sse(
                &app,
                &uri,
                &[("last-event-id", "stale-client-cursor")],
                None,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("R3 reconnect {reconnect} timed out"))
        .expect(StatusCode::OK);
        assert_eq!(
            sse_committed_ids(&body),
            canonical,
            "R3/E3 reconnect {reconnect} full-replays every durable id once"
        );
    }
}

/// Causes: C1 a fresh Session receiver has empty backfill; C2 the sole state-owned
/// Sender is dropped before any terminal fact. Effect E1: `recv` returns Closed,
/// the exact signal used to end the SSE body. Decision rule C1+C2=>E1; a retained
/// Sender instead leaves the receiver open and is outside this terminal case.
#[tokio::test]
async fn a_closed_sender_ends_the_subscription() {
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = support::state(CoordinatedRuntimeFake::default()).await;
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    // Drop the only Sender (held in the state's live-broadcast map).
    drop(state);

    assert!(
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
                .await
                .expect("timed out waiting for rx.recv()"),
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ),
        "the receiver observes Closed once the session's sender is gone"
    );
}

#[tokio::test]
async fn an_open_stream_reconciles_silent_commits_from_an_independent_replica() {
    // Causes: S1 two independent Managed caches share real SQLite root truth
    // and the Runtime port; S2 only B accepts/drives/projects one user command;
    // S3 A's HTTP SSE body stays open with no local broadcast, reconnect or GET.
    // Effects: E1 A reads and delivers every canonical committed id and final
    // marker in its bounded silent window; E2 only one command/Run was admitted;
    // E3 root/child wire projection is identical to B's authoritative history.
    // Rule S1+S2+S3=>E1-E3. PostgreSQL multi-pool qualification reuses the same
    // production history and projection ports; this slice owns cache isolation.
    let (runtime, a, b) = independent_replicas().await;
    let id = state_create(&a).await;
    let response = router(a)
        .oneshot(
            Request::builder()
                .uri(format!("/v1/sessions/{id}/events/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "S1 open A stream");
    let observer = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(8),
            response.into_body().collect(),
        )
        .await
        .expect("S3/E1 silent replica stream completes from durable truth")
        .expect("S3 production SSE body")
        .to_bytes()
    });
    state_send_user(&b, &runtime, &id, "commit only through B").await;
    let body = String::from_utf8(observer.await.unwrap().to_vec()).unwrap();
    let expected = b.list_events(&id, None, Some(100), false).unwrap().data;
    assert_eq!(
        sse_committed_ids(&body),
        expected
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>(),
        "S3/E1+E3"
    );
    assert!(
        body.contains("event: session.status_idle"),
        "S3 exact terminal"
    );
    assert!(
        runtime
            .completed_user_dispatches()
            .lock()
            .unwrap()
            .is_empty(),
        "S2 exact completed boundary was drained"
    );
    assert_eq!(
        expected
            .iter()
            .filter(|event| event.type_str() == "user.message")
            .count(),
        1,
        "S2/E2 one accepted command, no resend"
    );
    let user = expected
        .iter()
        .find(|event| event.type_str() == "user.message")
        .unwrap();
    assert!(
        user.processed_at.is_some(),
        "S3 completed admission observation"
    );
    assert!(
        body.contains(&serde_json::to_string(user).unwrap()),
        "S3 exact completed receipt wire"
    );
}

#[tokio::test]
async fn deletion_after_sse_snapshot_delivers_the_exact_committed_terminal_without_cache() {
    // Causes: D1 HTTP has captured its snapshot/receiver but its body has never
    // been polled; D2 the same producer commits Delete and removes that cache;
    // D3 the real body starts only after D2. D1+D2+D3 => snapshot still readable,
    // no NotFound body error, one terminal equal to the producer's receipt.
    // This deterministically covers the cache-vs-broadcast deletion race.
    let state = Arc::new(support::state(CoordinatedRuntimeFake::default()).await);
    let app = router(state.clone());
    let id = http_create(&app).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/sessions/{id}/events/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "D1 accepted snapshot");
    let (_, mut receipt) = state.stream_subscribe(&id).unwrap();
    http_json(
        &app,
        "DELETE",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    let committed = drain(&mut receipt)
        .0
        .into_iter()
        .find(|event| event.type_str() == "session.deleted")
        .expect("D2 committed producer receipt");
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        response.into_body().collect(),
    )
    .await
    .expect("D3 exact terminal closes")
    .expect("D3 cache absence is not a body error");
    let body = String::from_utf8(body.to_bytes().to_vec()).unwrap();
    let terminals = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["type"] == "session.deleted")
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1, "D3 exactly one logical terminal");
    assert_eq!(
        terminals[0]["id"], committed.id,
        "D3 same authoritative identity"
    );
    assert!(
        terminals[0]["processed_at"].is_null(),
        "D3 cleanup timestamps are not invented deletion timestamps"
    );
}

#[tokio::test]
async fn an_open_stream_observes_another_replicas_durable_deletion_without_local_broadcast() {
    // Causes: D4 independent caches share one real durable root; D5 only B
    // commits deletion and A's receiver gets no local wake; D6 A's already-open
    // body stays connected. D4+D5+D6 => the retained hidden root or scoped
    // tombstone proves one logical deletion with the same producer identity.
    // No reconnect, history GET, command resend or second deletion admission.
    let (_, a, b) = independent_replicas().await;
    let id = state_create(&a).await;
    // Keep the aggregate's real activity fence open before subscribing. An
    // already-idle Session may legitimately finish SSE before a later delete.
    a.session_application()
        .activities()
        .begin_activity(&awaken_session_types::SessionId::from(&id))
        .await
        .expect("D4 admitted activity keeps the stream open");
    let app = router(a.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/sessions/{id}/events/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let observer = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(8),
            response.into_body().collect(),
        )
        .await
        .expect("D6 durable deletion closes A")
        .expect("D6 valid terminal body")
    });
    http_json(
        &router(b.clone()),
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    let (_, mut producer) = b.stream_subscribe(&id).unwrap();
    let (_, mut local) = a.stream_subscribe(&id).unwrap();
    b.delete_session(&id).await.unwrap();
    let committed = drain(&mut producer)
        .0
        .into_iter()
        .find(|event| event.type_str() == "session.deleted")
        .unwrap();
    assert!(
        matches!(local.try_recv(), Err(TryRecvError::Empty)),
        "D5 no A broadcast"
    );
    let body = String::from_utf8(observer.await.unwrap().to_bytes().to_vec()).unwrap();
    let terminals = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["type"] == "session.deleted")
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1, "D6 exactly one deletion");
    assert_eq!(
        terminals[0]["id"], committed.id,
        "D6 shared canonical identity"
    );
}

#[tokio::test]
async fn an_open_http_stream_delivers_processing_enrichment_of_its_issued_receipt() {
    // Cause/effect table E1: an admitted User Run returned, but its exact
    // Dispatch-applied boundary has not finalized the retained command; E2 the
    // HTTP body actually observes that id with null processed_at; E3 the
    // canonical boundary then finalizes. Effect: the SAME connection delivers
    // the same immutable id/payload with its first processing observation and
    // closes on the correct aggregate Idle, without resending the command.
    let runtime = CoordinatedRuntimeFake::default();
    let state = Arc::new(support::state(runtime.clone()).await);
    let id = state_create(&state).await;
    let sent = state.send_events(&id, serde_json::from_value(serde_json::json!({
        "events": [{"type":"user.message", "content":[{"type":"text", "text":"observe processing"}]}]
    })).unwrap()).await.unwrap();
    let receipt = sent.data[0].id.clone();
    support::drive_retained_session_events(&state, &id).await;
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/v1/sessions/{id}/events/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "E1 pending receipt stream"
    );
    let (observed, observation) = tokio::sync::oneshot::channel();
    let observed_id = receipt.clone();
    let observer = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(8), async move {
            let mut body = response.into_body();
            let mut output = Vec::new();
            let mut observed = Some(observed);
            while let Some(frame) = body.frame().await {
                let frame = frame.expect("E2 production stream frame");
                if let Some(bytes) = frame.data_ref() {
                    output.extend_from_slice(bytes);
                    let text = String::from_utf8_lossy(&output);
                    if text
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                        .any(|event| event["id"] == observed_id && event["processed_at"].is_null())
                        && let Some(observed) = observed.take()
                    {
                        observed.send(()).expect("E2 observer remains alive");
                    }
                }
            }
            String::from_utf8(output).unwrap()
        })
        .await
        .expect("E3 processing completion and exact Idle settle the open body")
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), observation)
        .await
        .expect("E2 actual pending frame observed before finalization")
        .unwrap();
    support::drive_completed_user_dispatches(&state, &id, &runtime.completed_user_dispatches())
        .await;
    let text = observer.await.unwrap();
    let frames = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["id"] == receipt)
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 2, "E2/E3 exactly admission then enrichment");
    assert!(frames[0]["processed_at"].is_null(), "E2");
    assert!(frames[1]["processed_at"].as_str().is_some(), "E3");
    let mut original = frames[0].clone();
    original["processed_at"] = frames[1]["processed_at"].clone();
    assert_eq!(
        original, frames[1],
        "E3 only the completion observation changed"
    );
}
