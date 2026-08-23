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

use awaken_protocol_managed::test_support::CoordinatedRuntimeFake;
use awaken_protocol_managed::types::Event;
use awaken_protocol_managed::{ManagedState, router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::broadcast::error::TryRecvError;
use tower::ServiceExt;

// --- State-level helpers (deterministic broadcast driving) -------------------

async fn state_create(state: &ManagedState) -> String {
    state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder",
                "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
            }))
            .unwrap(),
            None,
        )
        .await
        .expect("create")
        .id
}

async fn reconcile_published_run(state: &ManagedState, session_id: &str) {
    // The shared fake commits through reserve/activate/state, while the real Host
    // supplies the later completion callback. Drive and settle that exact
    // application-owned activity here; no test transcript or lifecycle state is
    // synthesized beside the production authority.
    let application = state.session_application();
    support::drive_retained_session_events(state, session_id).await;
    let session = Box::pin(application.session(session_id))
        .await
        .expect("read the retained activity epochs");
    for epoch in session.active_activity_epochs {
        Box::pin(application.settle_activity(session_id, epoch))
            .await
            .expect("settle the application-owned activity epoch");
    }
}

async fn state_send_user(state: &Arc<ManagedState>, id: &str, text: &str) {
    let req = serde_json::from_value(serde_json::json!({
        "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
    }))
    .unwrap();
    state.send_events(id, req).await.expect("send");
    reconcile_published_run(state, id).await;
    // GET owns warm projection in production. Calling the route after the
    // committed Run keeps this integration helper on that same projector and
    // publishes its appended suffix to an already-open receiver.
    let app = router(state.clone());
    let _ = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
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
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// GET an SSE endpoint and collect the whole (terminating) body as a string.
async fn http_sse(app: &Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
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
/// order exactly once; E3 no lag. Decision rule L1=C1+C2+C3=>E1+E2+E3.
#[tokio::test]
async fn live_broadcast_delivers_one_runs_committed_frames_in_order() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `live broadcast delivers one runs committed frames in order` is one
    // independent branch selecting `all output, state, side-effect, error, and terminal assertions
    // below hold together`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let id = state_create(&state).await;

    // Subscribe first: a fresh session has no committed events, so nothing is
    // backfilled and every frame below arrives live.
    let (snapshot, mut rx) = state.stream_subscribe(&id).expect("subscribe");
    assert!(snapshot.is_empty(), "a fresh session backfills nothing");

    state_send_user(&state, &id, "hi").await;

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
            "session.usage",
            "session.status_idle"
        ],
        "the Run's committed frames arrive live, in order"
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
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    state_send_user(&state, &id, "one").await;
    state_send_user(&state, &id, "two").await;

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
            "session.usage",
            "session.status_idle",
        ],
        "both Runs' brackets arrive live, in commit order"
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
    let state = Arc::new(ManagedState::new(runtime.clone()));
    let app = router(state.clone());
    let id = http_create(&app).await;
    let (_snapshot, mut committed_rx) = state.stream_subscribe(&id).expect("R3 subscribe");
    let stream_app = app.clone();
    let uri = format!("/v1/sessions/{id}/events/stream?event_deltas[]=agent.message");
    let stream = tokio::spawn(async move { http_sse(&stream_app, &uri, &[]).await });
    runtime.wait_for_live_subscription().await;

    runtime.publish_root_live_text(&id, "root ");
    runtime.publish_root_live_text(&id, "progress");
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

    let (status, sse) = stream.await.unwrap();
    assert_eq!(status, StatusCode::OK);
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
#[tokio::test]
async fn child_thread_sse_is_isolated_and_archive_projects_durable_terminal() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
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
    reconcile_published_run(&state, &id).await;
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

    let (primary_status, primary_sse) =
        http_sse(&app, &format!("/v1/sessions/{id}/events/stream"), &[]).await;
    assert_eq!(primary_status, StatusCode::OK, "S1");
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

    let (child_status, child_sse) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{child_id}/stream"),
        &[],
    )
    .await;
    assert_eq!(child_status, StatusCode::OK);
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
    let (archived_status, archived_child_sse) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{child_id}/stream"),
        &[],
    )
    .await;
    assert_eq!(archived_status, StatusCode::OK, "S2/E2");
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
    let state = Arc::new(ManagedState::new(runtime.clone()));
    let id = state_create(&state).await;
    // Project the delegation, then subscribe: the returned snapshot owns backfill
    // while this receiver contains only facts committed after subscription.
    state_send_user(&state, &id, "go").await;
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
#[tokio::test]
async fn stream_thread_events_backfills_the_primary_thread() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    reconcile_published_run(&state, &id).await;
    // Codec integration rule P1: a listed public root is `sthr_` and selects
    // the internal Session root on the per-Thread stream route. A fabricated
    // internal/sentinel id is covered by the unknown-Thread rejection below.
    let primary_id = http_primary_thread_id(&app, &id).await;
    assert!(primary_id.starts_with("sthr_"), "P1");

    let (status, sse) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{primary_id}/stream"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
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
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    reconcile_published_run(&state, &id).await;
    let primary_id = http_primary_thread_id(&app, &id).await;
    let (status, _) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{primary_id}/stream?event_deltas[]=agent.message"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Managed child live-preview cause/effect table:
/// C0 the reserved Event batch is advanced once and its initial committed prefix
/// exposes the real child selector; C1 exact child text is followed by a tool
/// delta (ordinary-message proof); C2 child
/// stream opts into `agent.message`; C3 child reasoning is observed; C4
/// primary/Session share only the committed broadcast; C5 later text becomes the
/// terminal report; C6 ordinary Message and terminal lifecycle commit. Effects:
/// E0 C0 lets both subscriptions start from the same caught-up prefix;
/// E1 C1+C2 emits child-only start/delta; E2 C3 emits nothing; E3 C4 receives no
/// child preview; E4 C5 emits no preview and commits only child sent/primary
/// received; E5 C6 commits the ordinary `agent.message` with E1's id, settles the
/// application-owned activity epoch, then projects child and aggregate Idle.
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
    let state = Arc::new(ManagedState::new(runtime.clone()));
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
    let child_stream = tokio::spawn(async move { http_sse(&child_app, &child_uri, &[]).await });
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
    runtime.complete_deferred_child("terminal report");
    reconcile_published_run(&state, &id).await;
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
    let (status, sse) = child_stream.await.unwrap();
    assert_eq!(status, StatusCode::OK);
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
    let (status, primary) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{primary_id}/stream?event_deltas[]=agent.message"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
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
    let app = router(Arc::new(ManagedState::new(
        CoordinatedRuntimeFake::default(),
    )));
    let id = http_create(&app).await;
    for thread_id in ["nope".to_string(), format!("{id}:primary")] {
        let (status, _) = http_sse(
            &app,
            &format!("/v1/sessions/{id}/threads/{thread_id}/stream"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{thread_id}");
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
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    reconcile_published_run(&state, &id).await;

    let repeated = |count: usize| {
        std::iter::repeat_n("event_deltas[]=agent.message", count)
            .collect::<Vec<_>>()
            .join("&")
    };
    let (status, _) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/events/stream?{}", repeated(100)),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "E4 inclusive maximum");

    for (rule, query) in [
        (
            "E4 unsupported value",
            "event_deltas[]=agent.tool_use".to_string(),
        ),
        ("E4 over maximum", repeated(101)),
    ] {
        let (status, body) = http_sse(
            &app,
            &format!("/v1/sessions/{id}/events/stream?{query}"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
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
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let app = router(state.clone());
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    reconcile_published_run(&state, &id).await;

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
    let (status, sse) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/events/stream"),
        &[("last-event-id", &last_id)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        sse.contains("event: session.status_running")
            && sse.contains("event: agent.message")
            && sse.contains("event: session.status_idle"),
        "the full committed log is replayed, Last-Event-ID notwithstanding — sse:\n{sse}"
    );
}

// === 1(d) Lagged / Closed broadcast handling ================================

/// Causes: C1 an open receiver does not drain; C2 one durable batch publishes
/// 1,025 accepted receipts, exceeding the 1,024-frame channel; C3 canonical Run
/// reconciliation/projector publishes terminal facts afterward. Effects: E1 the
/// receiver reports a lag gap; E2 later `session.status_idle` remains observable.
/// Decision table: G1=C1+receipts<=1024=>no required gap (boundary owned by the
/// channel); G2=C1+C2+C3=>E1+E2 (covered here).
#[tokio::test]
async fn a_lagging_subscriber_skips_frames_but_still_receives_later_ones() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    // Admission itself owns every receipt. Overflow with that authoritative
    // prefix, then project the later committed Run through the normal read path.
    let events: Vec<serde_json::Value> = (0..1_025)
        .map(|_| serde_json::json!({ "type": "user.message", "content": [{ "type": "text", "text": "x" }] }))
        .collect();
    let req = serde_json::from_value(serde_json::json!({ "events": events })).unwrap();
    state.send_events(&id, req).await.expect("batch of Runs");
    reconcile_published_run(&state, &id).await;
    let app = router(state.clone());
    let _ = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;

    let (frames, lagged) = drain(&mut rx);
    assert!(lagged >= 1, "the receiver observed a Lagged gap");
    assert!(
        committed_types(&frames).contains(&"session.status_idle"),
        "later committed frames (idle) still arrive despite the lag"
    );
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
    let state = ManagedState::new(CoordinatedRuntimeFake::default());
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    // Drop the only Sender (held in the state's live-broadcast map).
    drop(state);

    assert!(
        matches!(
            rx.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ),
        "the receiver observes Closed once the session's sender is gone"
    );
}
