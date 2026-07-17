//! The streaming/preview plane — the crate's biggest coverage hole. The
//! send-then-stream backfill is covered in `adapter.rs`; this suite drives the
//! *live* broadcast path instead:
//!
//! 1. LIVE broadcast ordering: subscribe first, then drive a turn, and assert the
//!    committed frames arrive on the open receiver in turn order.
//! 2. The preview plane end-to-end: a runtime whose `run_streaming` drives the sink
//!    publishes `event_start`/`event_delta` preview frames, and the committed
//!    `agent.message` reuses the preview-minted id (preview → committed
//!    reconciliation by id).
//! 3. `session.thread_status_terminated` driven over HTTP and asserted on the SSE
//!    `event:` name.
//! 4. Replay characterization (`Last-Event-ID` / cursor), the `stream_thread_events`
//!    endpoint, and `Lagged`/`Closed` broadcast handling.
//!
//! The live-broadcast assertions drive `ManagedState::stream_subscribe` + a turn
//! directly (the crate's own idiom for the broadcast — see
//! `state.rs::delete_broadcasts_session_deleted_then_removes_the_record`): it is
//! fully deterministic (every frame is published synchronously by the awaited
//! `send_events`, then drained) with no ordering sleeps, which an HTTP oneshot of
//! the infinite SSE body cannot be without concurrency + timing.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::Sink;
use awaken_protocol_managed::types::{
    OutboundKind, PreviewContent, PreviewDelta, PreviewFrame, SendEventsResponse, StreamFrame,
};
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeReport, RunError, SessionRuntime, StepOutcome, Terminus, router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::broadcast::error::TryRecvError;
use tower::ServiceExt;

// --- Fakes -------------------------------------------------------------------

/// A one-text-reply runtime (no streaming path): `echo: <user text>`.
struct EchoFake;

#[async_trait::async_trait]
impl SessionRuntime for EchoFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        let text = Message::new(Id("u".into()), Role::User, content).text_content();
        Ok(end_turn(vec![Message::text(
            Id("a".into()),
            Role::Assistant,
            format!("echo: {text}"),
        )]))
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    async fn add_system(&self, _t: &str, _x: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A runtime that mirrors its turn's text through the live-preview sink as
/// `TextDelta` events (chunked), then commits a single `agent.message` carrying the
/// same full text. Proves the preview → committed reconciliation: the committed
/// message must reuse the id the preview `event_start` announced.
struct StreamingFake {
    chunks: Vec<&'static str>,
}

#[async_trait::async_trait]
impl SessionRuntime for StreamingFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        // send_events always drives `run_streaming`; `run` is only the non-streaming
        // fallback and is unused here.
        Ok(end_turn(vec![Message::text(
            Id("a".into()),
            Role::Assistant,
            self.chunks.concat(),
        )]))
    }
    async fn run_streaming(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
        sink: Arc<dyn Sink>,
    ) -> Result<StepOutcome, RunError> {
        // Mirror the reply as live text deltas before committing it.
        for chunk in &self.chunks {
            let ev = StreamEvent {
                run_id: awaken_agent_contract::agent::run::Id("r1".into()),
                kind: AgentEvent::Delta(Delta::TextDelta {
                    delta: (*chunk).into(),
                }),
            };
            sink.send(ev).await.expect("best-effort sink send");
        }
        Ok(end_turn(vec![Message::text(
            Id("a".into()),
            Role::Assistant,
            self.chunks.concat(),
        )]))
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    async fn add_system(&self, _t: &str, _x: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A runtime whose single turn delegates once (an inline `agent_run` tool call to
/// `researcher`), spawning a subagent child thread whose archive is the terminate
/// path under test.
struct DelegateFake;

#[async_trait::async_trait]
impl SessionRuntime for DelegateFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Ok(end_turn(vec![
            Message::new(
                Id("a".into()),
                Role::Assistant,
                vec![
                    ContentBlock::text("delegating"),
                    ContentBlock::ToolUse {
                        id: "d1".into(),
                        name: "agent_run".into(),
                        input: serde_json::json!({ "agent_id": "researcher", "input": "find docs" }),
                    },
                ],
            ),
            Message::new(
                Id("t".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "d1".into(),
                    content: vec![ContentBlock::text("here are the docs")],
                }],
            ),
        ]))
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    async fn add_system(&self, _t: &str, _x: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

fn end_turn(messages: Vec<Message>) -> StepOutcome {
    StepOutcome {
        messages,
        stop: Terminus::End,
        pending: None,
        compacted: false,
        rescheduled: false,
        failure: None,
    }
}

// --- State-level helpers (deterministic broadcast driving) -------------------

async fn state_create(state: &ManagedState) -> String {
    state
        .create_session(
            serde_json::from_value(serde_json::json!({ "agent": "coder" })).unwrap(),
            None,
        )
        .await
        .expect("create")
        .id
}

async fn state_send_user(state: &ManagedState, id: &str, text: &str) -> SendEventsResponse {
    let req = serde_json::from_value(serde_json::json!({
        "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
    }))
    .unwrap();
    state.send_events(id, req).await.expect("send")
}

/// Drain every frame currently buffered on `rx` (all frames are published
/// synchronously by the awaited `send_events`, so a non-blocking drain is complete
/// and deterministic). Returns the frames and how many `Lagged` gaps were observed.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<StreamFrame>) -> (Vec<StreamFrame>, usize) {
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

fn committed_types(frames: &[StreamFrame]) -> Vec<&'static str> {
    frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::Committed(e) => Some(e.type_str()),
            StreamFrame::Preview(_) => None,
        })
        .collect()
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
        serde_json::json!({ "agent": "coder" }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// === 1(a) LIVE broadcast ordering ===========================================

/// A subscriber open BEFORE a turn runs receives that turn's committed events on
/// the live broadcast, in turn order (`running` → `agent.message` → `idle`) — the
/// live path, distinct from the send-then-stream backfill the adapter suite covers.
#[tokio::test]
async fn live_broadcast_delivers_a_turns_committed_frames_in_order() {
    let state = ManagedState::new(EchoFake);
    let id = state_create(&state).await;

    // Subscribe first: a fresh session has no committed events, so nothing is
    // backfilled and every frame below arrives live.
    let (snapshot, mut rx) = state.stream_subscribe(&id).expect("subscribe");
    assert!(snapshot.is_empty(), "a fresh session backfills nothing");

    state_send_user(&state, &id, "hi").await;

    let (frames, lagged) = drain(&mut rx);
    assert_eq!(lagged, 0, "no lag on a single small turn");
    assert_eq!(
        committed_types(&frames),
        vec![
            "session.status_running",
            "agent.message",
            "session.status_idle"
        ],
        "the turn's committed frames arrive live, in order"
    );
}

/// Two consecutive turns on one open subscription deliver both brackets back to
/// back with no interleaving or gap — the broadcast preserves commit order across
/// turns.
#[tokio::test]
async fn live_broadcast_preserves_order_across_two_turns() {
    let state = ManagedState::new(EchoFake);
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    state_send_user(&state, &id, "one").await;
    state_send_user(&state, &id, "two").await;

    let (frames, _) = drain(&mut rx);
    assert_eq!(
        committed_types(&frames),
        vec![
            "session.status_running",
            "agent.message",
            "session.status_idle",
            "session.status_running",
            "agent.message",
            "session.status_idle",
        ],
        "both turns' brackets arrive live, in commit order"
    );
}

// === 2 Preview plane: preview → committed reconciliation ====================

/// The preview plane end-to-end: a `run_streaming` turn publishes `event_start` +
/// `event_delta` preview frames on the live broadcast, and the committed
/// `agent.message` reuses the id the `event_start` announced — the identity the SDK
/// reconciles preview → buffered on. The concatenated preview text equals the
/// committed message text.
#[tokio::test]
async fn preview_frames_reconcile_to_the_committed_agent_message_by_id() {
    let state = ManagedState::new(StreamingFake {
        chunks: vec!["Hel", "lo ", "world"],
    });
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    state_send_user(&state, &id, "hi").await;

    let (frames, _) = drain(&mut rx);

    // The preview announces the upcoming agent.message id up front.
    let preview_id = frames
        .iter()
        .find_map(|f| match f {
            StreamFrame::Preview(PreviewFrame::EventStart { event })
                if event.event_type == "agent.message" =>
            {
                Some(event.id.clone())
            }
            _ => None,
        })
        .expect("an event_start preview announced the agent.message");

    // The preview deltas carry the streamed suffix text, keyed to that id.
    let previewed: String = frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::Preview(PreviewFrame::EventDelta {
                event_id,
                delta:
                    PreviewDelta::ContentDelta {
                        content: PreviewContent::Text { text },
                        ..
                    },
            }) if *event_id == preview_id => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        previewed, "Hello world",
        "the preview deltas stream the full text"
    );

    // The committed agent.message reuses the preview-minted id (reconciliation).
    let (committed_id, committed_text) = frames
        .iter()
        .find_map(|f| match f {
            StreamFrame::Committed(e) => match &e.kind {
                OutboundKind::AgentMessage { content } => Some((
                    e.id.clone(),
                    Message::new(Id("x".into()), Role::Assistant, content.clone()).text_content(),
                )),
                _ => None,
            },
            _ => None,
        })
        .expect("the committed agent.message is on the broadcast");
    assert_eq!(
        committed_id, preview_id,
        "the committed agent.message reuses the preview-announced id"
    );
    assert_eq!(
        committed_text, "Hello world",
        "committed text matches the preview"
    );
}

/// The same streaming turn observed over HTTP with `event_deltas[]=agent.message`
/// (send-then-stream): the preview frames are stream-only and gone by the time the
/// connection opens, but the committed `agent.message` in the backfill still carries
/// the preview-minted id — the durable half of the reconciliation is visible on the
/// event list regardless of whether a client caught the live previews.
#[tokio::test]
async fn a_streamed_turns_committed_message_id_is_preview_minted() {
    let state = Arc::new(ManagedState::new(StreamingFake {
        chunks: vec!["a", "b"],
    }));
    let app = router(state);
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;

    // The committed agent.message id was drawn from the shared evt_N counter by the
    // preview sink (not a fresh mint at append) — it sits BEFORE the running marker's
    // id in the sequence because the preview minted it during the run, ahead of the
    // append-step ids.
    let list = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let events = list["data"].as_array().unwrap();
    let msg = events
        .iter()
        .find(|e| e["type"] == "agent.message")
        .unwrap();
    let running = events
        .iter()
        .find(|e| e["type"] == "session.status_running")
        .unwrap();
    let num = |e: &serde_json::Value| -> u64 {
        e["id"]
            .as_str()
            .unwrap()
            .trim_start_matches("evt_")
            .parse()
            .unwrap()
    };
    assert!(
        num(msg) < num(running),
        "the agent.message id ({}) was preview-minted ahead of the running marker ({})",
        msg["id"],
        running["id"]
    );
}

// === 3 session.thread_status_terminated over HTTP ===========================

/// Driving the child-thread terminate path and asserting the SSE `event:` name: a
/// delegation spawns a subagent child thread; archiving that child thread commits a
/// `session.thread_status_terminated` event; a subsequent stream (after the session
/// itself is archived, so the backfill reaches a terminal and ends) carries it as an
/// SSE `event: session.thread_status_terminated` line.
#[tokio::test]
async fn archiving_a_child_thread_streams_thread_status_terminated() {
    let app = router(Arc::new(ManagedState::new(DelegateFake)));
    let id = http_create(&app).await;

    // A turn delegates once → a child thread `<id>:thread:0` is created.
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "go" }] }] }),
    )
    .await;
    let threads = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    let child_id = threads["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["parent_thread_id"] == format!("{id}:primary"))
        .expect("a child thread was spawned")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Archiving the child thread commits the terminated event...
    let archived = http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/threads/{child_id}/archive"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(archived["status"], "terminated");

    // ...and archiving the session gives the backfill a terminal to end on, so the
    // whole SSE body is delivered.
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/archive"),
        serde_json::Value::Null,
    )
    .await;

    let (status, sse) = http_sse(&app, &format!("/v1/sessions/{id}/events/stream"), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        sse.contains("event: session.thread_status_terminated"),
        "the child-thread terminate is an SSE event: name — sse:\n{sse}"
    );
    // The terminated frame carries the delegate identity in its data payload.
    assert!(
        sse.contains(&format!("\"session_thread_id\":\"{child_id}\"")),
        "the terminated frame names the child thread — sse:\n{sse}"
    );
}

/// KNOWN BUG (adjudicate): `state/threads.rs::archive_thread` commits
/// `session.thread_status_terminated` to the event log but — unlike `append_step`,
/// `append_outcome`, and the delete path — never calls `broadcast_committed_from`,
/// so a client with an ALREADY-OPEN live stream never receives the child-thread
/// termination in real time; only a reconnecting client sees it via backfill. This
/// test characterizes the current behavior (the open subscriber gets nothing) so the
/// gap is pinned; it is not asserting the behavior is correct.
#[tokio::test]
async fn archive_thread_does_not_broadcast_to_an_open_live_stream() {
    let state = ManagedState::new(DelegateFake);
    let id = state_create(&state).await;
    // Run the delegation turn, then open a subscription and drain the backfill turn
    // frames so the receiver is caught up.
    state_send_user(&state, &id, "go").await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");
    let (_caught_up, _) = drain(&mut rx);

    let child_id = format!("{id}:thread:0");
    state.archive_thread(&id, &child_id).expect("archive child");

    // Nothing was broadcast for the terminate — the open stream is silent.
    let (frames, _) = drain(&mut rx);
    assert!(
        !committed_types(&frames).contains(&"session.thread_status_terminated"),
        "archive_thread does not publish the terminated event to the open broadcast"
    );
    // But it WAS committed to the durable log (a reconnect would replay it).
    let list = state.list_events(&id, None, None).expect("list");
    assert!(
        list.data
            .iter()
            .any(|e| e.type_str() == "session.thread_status_terminated"),
        "the terminated event is in the committed log"
    );
}

// === 1(c) stream_thread_events endpoint =====================================

/// `GET /v1/sessions/{id}/threads/{tid}/stream` (previously untested): the primary
/// thread's stream backfills the session's committed events as SSE `event:` names,
/// terminating on the turn's idle.
#[tokio::test]
async fn stream_thread_events_backfills_the_primary_thread() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;

    let (status, sse) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{id}:primary/stream"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(sse.contains("event: agent.message"), "sse:\n{sse}");
    assert!(sse.contains("event: session.status_idle"), "sse:\n{sse}");
}

/// The per-thread stream rejects the `event_deltas[]` preview opt-in outright (only
/// the session-level stream supports previews), before resolving the thread — a 400.
#[tokio::test]
async fn stream_thread_events_rejects_the_preview_opt_in() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = http_create(&app).await;
    let (status, _) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/threads/{id}:primary/stream?event_deltas[]=agent.message"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// An unknown thread id on the per-thread stream is a 404 (fail-closed).
#[tokio::test]
async fn stream_thread_events_unknown_thread_is_404() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = http_create(&app).await;
    let (status, _) = http_sse(&app, &format!("/v1/sessions/{id}/threads/nope/stream"), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The session-level stream's `event_deltas[]` parser rejects an unsupported value
/// with a 400 `invalid_request_error` (only `agent.message` / `agent.thinking` are
/// accepted), matching the official wire.
#[tokio::test]
async fn session_stream_rejects_an_unsupported_event_deltas_value() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = http_create(&app).await;
    let (status, body) = http_sse(
        &app,
        &format!("/v1/sessions/{id}/events/stream?event_deltas[]=agent.tool_use"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["type"], "invalid_request_error");
}

// === 1(b) Replay / Last-Event-ID characterization ===========================

/// CHARACTERIZATION: the SSE stream has NO incremental (cursor / `Last-Event-ID`)
/// resume — it always replays the FULL committed snapshot, and a `Last-Event-ID`
/// header is ignored. Replay-safety is instead provided by the snapshot/live
/// dedupe-by-id (a client discards ids it already saw). This pins the current
/// behavior; whether server-side incremental resume is wanted is an open product
/// question (see report), not asserted here as a bug.
#[tokio::test]
async fn the_stream_full_replays_and_ignores_last_event_id() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = http_create(&app).await;
    http_json(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;

    // Discover the committed ids (running is the earliest committed event).
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

/// The `Lagged` broadcast path: a subscriber that falls more than a channel-capacity
/// behind observes a `Lagged` gap (its oldest frames were dropped), yet the stream's
/// contract is best-effort — subsequent committed frames (including the terminal
/// `session.status_idle`) STILL arrive. Driven by overflowing the 1024-frame channel
/// with one batch of turns while the receiver does not drain (the exact condition the
/// stream's `Lagged => continue` arm tolerates).
#[tokio::test]
async fn a_lagging_subscriber_skips_frames_but_still_receives_later_ones() {
    let state = ManagedState::new(EchoFake);
    let id = state_create(&state).await;
    let (_snap, mut rx) = state.stream_subscribe(&id).expect("subscribe");

    // One batch of 400 turns → 1200 committed frames > the 1024 channel capacity,
    // published synchronously while `rx` is not drained → the receiver lags.
    let events: Vec<serde_json::Value> = (0..400)
        .map(|_| serde_json::json!({ "type": "user.message", "content": [{ "type": "text", "text": "x" }] }))
        .collect();
    let req = serde_json::from_value(serde_json::json!({ "events": events })).unwrap();
    state.send_events(&id, req).await.expect("batch of turns");

    let (frames, lagged) = drain(&mut rx);
    assert!(lagged >= 1, "the receiver observed a Lagged gap");
    assert!(
        committed_types(&frames).contains(&"session.status_idle"),
        "later committed frames (idle) still arrive despite the lag"
    );
}

/// The `Closed` broadcast path: when the session's sender is gone (here, the whole
/// state is dropped), an open receiver observes `Closed` — the signal the stream's
/// `Closed => break` arm ends the SSE body on. A fresh session's subscription tails
/// live (empty backfill), so this is the terminating condition when no committed
/// terminal ever arrives.
#[tokio::test]
async fn a_closed_sender_ends_the_subscription() {
    let state = ManagedState::new(EchoFake);
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
