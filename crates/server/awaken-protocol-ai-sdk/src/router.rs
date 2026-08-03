//! The axum router: AI SDK v6 UI Message Stream routes over a `RunApplication`.
//!
//! Handlers decode the request, drive one turn or resume through the port, and
//! project the committed step into a UI Message Stream SSE response. No runtime or
//! protocol logic lives here beyond routing and framing.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::event::{AgentEvent, Fact, Transcoder};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, FromRequest, Json, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use awaken_api_contract::CursorPage;
use awaken_session_contract::{
    CursorParams, EventForwardingSink, Pending, RunApplication, RunApplicationError, RunResume,
    StepOutcome, paginate_history,
};
use awaken_tenancy::ResolvedResourceId;

use crate::encoder::{AiSdkEncoder, encode_history, encode_step};
use crate::request::{DecisionKind, process_request, result_text};
use crate::types::{AiSdkChatRequest, UIStreamEvent, attach_usage};

type Runtime = Arc<dyn RunApplication>;

const STREAM_KEEP_ALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The AI SDK v6 header `DefaultChatTransport` uses to identify the stream format.
const AI_SDK_STREAM_HEADER: &str = "x-vercel-ai-ui-message-stream";

/// A JSON body extractor for the AI SDK routes. On a decode failure (malformed
/// JSON, wrong field type, bad content-type) it returns the UI Message Stream
/// error frame (`error` + `finish("error")`) rather than axum's plain-text 400,
/// so `useChat` sees the failure as a stream error like any driver error.
struct AiSdkJson<T>(T);

impl<S, T> FromRequest<S> for AiSdkJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(sse_error(RunApplicationError::bad_request(
                rejection.body_text(),
            ))),
        }
    }
}

/// Build the AI SDK router. Mount it alongside other protocol routers; the paths
/// are the `/v1/ai-sdk/...` surface the `useChat` transport expects.
pub fn router(runtime: Runtime) -> Router {
    Router::new()
        .route("/v1/ai-sdk/chat", post(chat))
        .route("/v1/ai-sdk/threads/{thread_id}/runs", post(chat_threaded))
        .route("/v1/ai-sdk/agents/{agent_id}/runs", post(chat_agent_scoped))
        .route(
            "/v1/ai-sdk/threads/{thread_id}/messages",
            get(thread_messages),
        )
        .with_state(runtime)
}

async fn chat(
    State(rt): State<Runtime>,
    AiSdkJson(payload): AiSdkJson<AiSdkChatRequest>,
) -> Response {
    run(rt, payload).await
}

async fn chat_threaded(
    State(rt): State<Runtime>,
    Path(thread_id): Path<String>,
    resolved: Option<Extension<ResolvedResourceId>>,
    AiSdkJson(mut payload): AiSdkJson<AiSdkChatRequest>,
) -> Response {
    payload.thread_id = Some(
        resolved
            .map(|Extension(thread)| thread.0)
            .unwrap_or(thread_id),
    );
    run(rt, payload).await
}

async fn chat_agent_scoped(
    State(rt): State<Runtime>,
    Path(agent_id): Path<String>,
    AiSdkJson(mut payload): AiSdkJson<AiSdkChatRequest>,
) -> Response {
    payload.agent_id = Some(agent_id);
    run(rt, payload).await
}

/// The core chat handler: decode, drive one step, project to an SSE stream.
async fn run(rt: Runtime, payload: AiSdkChatRequest) -> Response {
    // Peek the thread id so history dedup can run before decoding.
    let peek = payload
        .thread_id
        .clone()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let known_ids: HashSet<String> = match &peek {
        Some(thread) => rt
            .history(thread)
            .await
            .into_iter()
            .map(|m| m.id.0)
            .collect(),
        None => HashSet::new(),
    };

    let processed = process_request(payload, &known_ids);
    let thread = processed.thread_id.clone();

    if processed.messages.is_empty() {
        // RunResume answers an awaiting tool decision — a single committed step, framed
        // whole (no in-flight model output to stream).
        match resume_step(&rt, &thread, &processed.decisions).await {
            Ok(outcome) => {
                let mut events = encode_step(&outcome);
                attach_usage(&mut events, rt.usage(&thread).await);
                sse_response(events)
            }
            Err(response) => response,
        }
    } else {
        // A fresh turn: stream the engine's live progress as it runs, then append
        // the committed authoritative tail.
        stream_turn(rt, thread, processed.agent_id, processed.messages)
    }
}

/// Drive a turn while streaming the engine's best-effort live progress
/// (`start`/`text-*`/`tool-input-*`) to a chunked SSE body as it happens, then
/// append the committed authoritative tail (`tool-input-available` + `finish`).
/// The committed step stays the source of truth: a runtime that streams nothing
/// (durable/ACP/non-streaming) falls back to the full committed projection.
fn stream_turn(
    rt: Runtime,
    thread: String,
    agent: Option<String>,
    messages: Vec<Message>,
) -> Response {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    // Kept for the post-turn usage read (the turn future consumes `rt`/`thread`).
    let rt_usage = Arc::clone(&rt);
    let thread_usage = thread.clone();
    tokio::spawn(async move {
        let (live_tx, mut live_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let sink: Arc<dyn StreamSink> = Arc::new(EventForwardingSink::new(move |event| {
            live_tx
                .send(event)
                .map_err(|_| awaken_agent_contract::stream::sink::Error::Closed)
        }));
        // One transcoder instance for both tiers (ADR-0058 Axis 9): live `delta()`
        // for increments + `fact(RunStarted)` to open the stream. The authoritative
        // the end comes from the committed tail, so terminal/whole-unit facts on
        // the live channel are not wired here (G10/G13).
        let mut transcoder = AiSdkEncoder::new();
        // Drive the turn on its own task so live events drain concurrently. The
        // sink lives inside that future; when the turn ends it drops, closing
        // `live_rx` and ending the drain loop.
        let turn =
            tokio::spawn(async move { rt.run_streaming(&thread, agent, messages, sink).await });
        let mut keep_alive = tokio::time::interval(STREAM_KEEP_ALIVE_INTERVAL);
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut disconnected = false;
        'live: loop {
            match next_stream_item(&mut live_rx, &mut keep_alive).await {
                StreamItem::Event(event) => {
                    let wires = match &event {
                        AgentEvent::Delta(delta) => transcoder.delta(delta),
                        AgentEvent::Fact(fact @ Fact::RunStarted) => transcoder.fact(fact),
                        AgentEvent::Fact(_) => Vec::new(),
                    };
                    for wire in wires {
                        if out_tx.send(sse_line(&wire)).is_err() {
                            disconnected = true;
                            break 'live;
                        }
                    }
                }
                StreamItem::KeepAlive => {
                    if out_tx.send(": keep-alive\n\n".to_string()).is_err() {
                        disconnected = true;
                        break;
                    }
                }
                StreamItem::Closed => break,
            }
        }
        if disconnected {
            // The client hung up: cancel the in-flight turn so it ends promptly
            // instead of running to completion detached (a token leak). The
            // cooperative cancel lets the run commit cleanly; no hard abort needed.
            let _ = rt_usage.interrupt(&thread_usage).await;
            return;
        }
        let mut close = match turn.await {
            Ok(Ok(outcome)) => transcoder.complete(&outcome),
            Ok(Err(err)) => transcoder.fail(driver_error_message(err)),
            Err(_) => transcoder.fail("turn task cancelled"),
        };
        // The AI SDK `finish` part carries the run's token accounting.
        attach_usage(&mut close, rt_usage.usage(&thread_usage).await);
        for event in close {
            if out_tx.send(sse_line(&event)).is_err() {
                return;
            }
        }
        let _ = out_tx.send("data: [DONE]\n\n".to_string());
    });
    stream_response(out_rx)
}

enum StreamItem {
    Event(AgentEvent),
    KeepAlive,
    Closed,
}

async fn next_stream_item(
    live_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    keep_alive: &mut tokio::time::Interval,
) -> StreamItem {
    tokio::select! {
        event = live_rx.recv() => event.map_or(StreamItem::Closed, StreamItem::Event),
        _ = keep_alive.tick() => StreamItem::KeepAlive,
    }
}

/// Translate the request's decisions into a resume against the awaiting tool and
/// run it. Fails closed when there is no awaiting tool or no matching decision.
async fn resume_step(
    rt: &Runtime,
    thread: &str,
    decisions: &[crate::request::Decision],
) -> Result<StepOutcome, Response> {
    let Some(pending) = rt.pending(thread).await else {
        return Err(sse_error(RunApplicationError::bad_request(
            "no awaiting run to resume",
        )));
    };
    let decision = decisions
        .iter()
        .find(|d| d.tool_call_id == pending.tool_use_id)
        .or_else(|| decisions.first())
        .ok_or_else(|| {
            sse_error(RunApplicationError::bad_request(
                "no tool decision for the awaiting tool",
            ))
        })?;
    let resume = to_resume(&decision.kind, &pending);
    rt.resume(thread, &pending.tool_use_id, resume)
        .await
        .map_err(sse_error)
}

/// Map a client decision to a neutral resume, choosing the variant that matches
/// the pending tool's binding (client-executed vs built-in awaiting approval).
fn to_resume(kind: &DecisionKind, pending: &Pending) -> RunResume {
    if pending.client_executed {
        match kind {
            DecisionKind::Output(v) => RunResume::ClientResult {
                content: vec![ContentBlock::text(result_text(v))],
                is_error: false,
            },
            DecisionKind::Error(e) => RunResume::ClientResult {
                content: vec![ContentBlock::text(e)],
                is_error: true,
            },
            DecisionKind::Denied => RunResume::ClientResult {
                content: vec![ContentBlock::text("client denied the tool")],
                is_error: true,
            },
            DecisionKind::Approved => RunResume::ClientResult {
                content: Vec::new(),
                is_error: false,
            },
        }
    } else {
        match kind {
            DecisionKind::Approved | DecisionKind::Output(_) => RunResume::Confirm {
                allow: true,
                note: None,
            },
            DecisionKind::Denied => RunResume::Confirm {
                allow: false,
                note: None,
            },
            DecisionKind::Error(e) => RunResume::Confirm {
                allow: false,
                note: Some(e.clone()),
            },
        }
    }
}

async fn thread_messages(
    State(rt): State<Runtime>,
    Path(thread_id): Path<String>,
    resolved: Option<Extension<ResolvedResourceId>>,
    Query(params): Query<CursorParams>,
) -> Response {
    let thread_id = resolved
        .map(|Extension(thread)| thread.0)
        .unwrap_or(thread_id);
    let history = rt.history(&thread_id).await;
    match paginate_history(&history, params.cursor.as_deref(), params.limit()) {
        // The house cursor-page envelope (`awaken-api-contract`): `{ items, cursor }`,
        // where `cursor` is the continuation (`null` on the last page).
        Ok(page) => {
            Json(CursorPage::new(encode_history(page.items), page.next_page)).into_response()
        }
        // A stale or fabricated cursor is a caller fault: a plain 400, not the UI
        // stream error frame (this GET is not a chat stream).
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

/// The AI SDK UI Message Stream response headers (SSE + the transport marker).
fn sse_headers() -> [(header::HeaderName, HeaderValue); 3] {
    [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        ),
        (
            header::HeaderName::from_static(AI_SDK_STREAM_HEADER),
            HeaderValue::from_static("v1"),
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        ),
    ]
}

/// Frame one UI stream event as an SSE data line (`data: <json>\n\n`).
fn sse_line(event: &UIStreamEvent) -> String {
    let json = serde_json::to_string(event).expect("ui stream event serializes");
    format!("data: {json}\n\n")
}

/// A chunked SSE body fed from `out_rx`: each frame is flushed as the producer
/// sends it, so `useChat` paints in-flight instead of awaiting for the whole turn.
fn stream_response(out_rx: mpsc::UnboundedReceiver<String>) -> Response {
    let stream = UnboundedReceiverStream::new(out_rx).map(Ok::<String, Infallible>);
    (StatusCode::OK, sse_headers(), Body::from_stream(stream)).into_response()
}

/// Frame a whole event list as one buffered SSE body (used for the non-streaming
/// resume and error paths), terminated by `[DONE]`.
fn sse_response(events: Vec<UIStreamEvent>) -> Response {
    let mut body = String::new();
    for event in &events {
        body.push_str(&sse_line(event));
    }
    body.push_str("data: [DONE]\n\n");
    (StatusCode::OK, sse_headers(), body).into_response()
}

/// The AI SDK error tail (`error` + `finish("error")`).
fn error_events(err: RunApplicationError) -> Vec<UIStreamEvent> {
    let message = driver_error_message(err);
    vec![
        UIStreamEvent::error(message),
        UIStreamEvent::finish("error"),
    ]
}

fn driver_error_message(err: RunApplicationError) -> String {
    err.message
}

/// A single buffered SSE error stream (AI SDK errors use `errorText`).
fn sse_error(err: RunApplicationError) -> Response {
    sse_response(error_events(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Idle-stream cause/effect decision table. Causes: C1 the run is live but
     * the provider has not produced an AgentEvent inside one keep-alive window;
     * C2 an AgentEvent arrives; C3 the client receiver closes. Effects: E1 emit
     * recurring SSE comments without inventing a UIMessage event; E2 preserve
     * ordinary event projection; E3 the existing send-failure path interrupts
     * the detached run. Rules: K1=C1=>E1; K2=C2=>E2; K3=C3=>E3. This test owns
     * the timer classification; streaming integration tests own E2 and the
     * runtime cancellation suite owns E3. */
    #[tokio::test]
    async fn idle_stream_ticks_are_transport_keep_alives_not_agent_events() {
        let (_sender, mut receiver) = mpsc::unbounded_channel();
        let mut keep_alive = tokio::time::interval(std::time::Duration::from_millis(5));
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        assert!(matches!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                next_stream_item(&mut receiver, &mut keep_alive),
            )
            .await
            .expect("initial keep-alive"),
            StreamItem::KeepAlive
        ));
        assert!(matches!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                next_stream_item(&mut receiver, &mut keep_alive),
            )
            .await
            .expect("recurring keep-alive"),
            StreamItem::KeepAlive
        ));
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(": keep-alive\n\n".to_string()).unwrap();
        drop(sender);
        let response = stream_response(receiver);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache, no-transform"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b": keep-alive\n\n");
    }

    fn pending(client_executed: bool) -> Pending {
        Pending {
            tool_use_id: "t1".into(),
            name: "probe".into(),
            input: serde_json::Value::Null,
            client_executed,
        }
    }

    // ── client-executed tool: the client runs it and returns the result ──

    #[test]
    fn client_output_delivers_a_non_error_result() {
        let r = to_resume(
            &DecisionKind::Output(serde_json::json!("42")),
            &pending(true),
        );
        assert!(
            matches!(r, RunResume::ClientResult { content, is_error: false } if content == vec![ContentBlock::text("42")])
        );
    }

    #[test]
    fn client_error_delivers_an_error_result() {
        let r = to_resume(&DecisionKind::Error("boom".into()), &pending(true));
        assert!(
            matches!(r, RunResume::ClientResult { content, is_error: true } if content == vec![ContentBlock::text("boom")])
        );
    }

    #[test]
    fn client_denied_delivers_an_error_result() {
        let r = to_resume(&DecisionKind::Denied, &pending(true));
        assert!(matches!(r, RunResume::ClientResult { is_error: true, .. }));
    }

    #[test]
    fn client_approved_delivers_an_empty_result() {
        let r = to_resume(&DecisionKind::Approved, &pending(true));
        assert!(
            matches!(r, RunResume::ClientResult { content, is_error: false } if content.is_empty())
        );
    }

    // ── built-in tool: a permission gate awaiting a decision ──

    #[test]
    fn builtin_approved_allows() {
        assert!(matches!(
            to_resume(&DecisionKind::Approved, &pending(false)),
            RunResume::Confirm { allow: true, .. }
        ));
    }

    #[test]
    fn builtin_output_is_treated_as_allow() {
        assert!(matches!(
            to_resume(
                &DecisionKind::Output(serde_json::Value::Null),
                &pending(false)
            ),
            RunResume::Confirm { allow: true, .. }
        ));
    }

    #[test]
    fn builtin_denied_rejects() {
        assert!(matches!(
            to_resume(&DecisionKind::Denied, &pending(false)),
            RunResume::Confirm {
                allow: false,
                note: None
            }
        ));
    }

    #[test]
    fn builtin_error_rejects_with_a_note() {
        let r = to_resume(&DecisionKind::Error("bad args".into()), &pending(false));
        assert!(matches!(r, RunResume::Confirm { allow: false, note: Some(n) } if n == "bad args"));
    }
}
