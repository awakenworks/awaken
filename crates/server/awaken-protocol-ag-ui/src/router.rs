//! The axum router: AG-UI routes over a `ProtocolRuntime`. Handlers decode the
//! `RunAgentInput`, drive one turn or resume through the port, and project the
//! committed step into an AG-UI SSE event stream.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::event::{AgentEvent, Fact, Transcoder};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use awaken_api_contract::CursorPage;
use awaken_protocol_transport::{
    ChannelStreamSink, CursorParams, DriverError, Pending, ProtocolRuntime, Resume, StepOutcome,
    paginate_history,
};

use crate::encoder::{AgUiEncoder, encode_close, encode_history, encode_step};
use crate::request::{ToolResultInput, process};
use crate::types::{AgUiEvent, RunAgentInput};

type Runtime = Arc<dyn ProtocolRuntime>;

/// A JSON body extractor for the AG-UI routes. On a decode failure (malformed
/// JSON, wrong field type, bad content-type) it returns a bare `RUN_ERROR` event
/// stream rather than axum's plain-text 400, so an AG-UI client sees the failure
/// as a run error. The body never parsed, so there is no run to bracket with
/// `RUN_STARTED`.
struct AgUiJson<T>(T);

impl<S, T> FromRequest<S> for AgUiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(sse_response(vec![AgUiEvent::error(rejection.body_text())])),
        }
    }
}

/// Build the AG-UI router. Mount it alongside other protocol routers; the paths
/// are the `/v1/ag-ui...` surface an AG-UI `HttpAgent` posts to.
pub fn router(runtime: Runtime) -> Router {
    Router::new()
        .route("/v1/ag-ui", post(run_agent))
        .route("/v1/ag-ui/agents/{agent_id}", post(run_agent_scoped))
        .route(
            "/v1/ag-ui/threads/{thread_id}/messages",
            get(thread_messages),
        )
        .with_state(runtime)
}

/// `GET /v1/ag-ui/threads/{id}/messages` — the server-persisted history of a
/// thread, projected to AG-UI messages. AG-UI is client-forward (the client
/// replays history in each run), so this endpoint lets a client that lost its
/// state rehydrate from committed truth. Paginated by cursor (`next_page`).
async fn thread_messages(
    State(rt): State<Runtime>,
    Path(thread_id): Path<String>,
    Query(params): Query<CursorParams>,
) -> Response {
    let history = rt.history(&thread_id).await;
    match paginate_history(&history, params.cursor.as_deref(), params.limit()) {
        // The house cursor-page envelope (`awaken-api-contract`): `{ items, cursor }`,
        // where `cursor` is the continuation (`null` on the last page).
        Ok(page) => {
            Json(CursorPage::new(encode_history(page.items), page.next_page)).into_response()
        }
        // A stale or fabricated cursor is a caller fault: a plain 400. This GET is
        // a JSON read, not a run, so it is not framed as an SSE RUN_ERROR.
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

async fn run_agent(
    State(rt): State<Runtime>,
    AgUiJson(input): AgUiJson<RunAgentInput>,
) -> Response {
    run(rt, input, None).await
}

async fn run_agent_scoped(
    State(rt): State<Runtime>,
    Path(agent_id): Path<String>,
    AgUiJson(input): AgUiJson<RunAgentInput>,
) -> Response {
    run(rt, input, Some(agent_id)).await
}

async fn run(rt: Runtime, input: RunAgentInput, agent_id: Option<String>) -> Response {
    let peek = input
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

    let processed = process(input, agent_id, &known_ids);
    let thread = processed.thread_id.clone();
    let run_id = processed.run_id.clone();

    if processed.messages.is_empty() {
        // Resume answers a parked tool decision — one committed step, framed whole.
        match resume_step(&rt, &thread, &processed.tool_results).await {
            Ok(outcome) => sse_response(encode_step(&outcome, &thread, &run_id)),
            Err(err) => sse_error(&thread, &run_id, err),
        }
    } else {
        // A fresh turn: stream the engine's live progress, then the committed tail.
        stream_turn(rt, thread, run_id, processed.agent_id, processed.messages)
    }
}

/// Drive a turn while streaming the engine's best-effort live progress
/// (`RUN_STARTED`/`TEXT_MESSAGE_*`/`TOOL_CALL_START`+`ARGS`) to a chunked SSE body
/// as it runs, then append the committed authoritative tail (`TOOL_CALL_END` +
/// `RUN_FINISHED`). A runtime that streams nothing falls back to the full
/// committed projection, so the committed step stays the source of truth.
fn stream_turn(
    rt: Runtime,
    thread: String,
    run_id: String,
    agent: Option<String>,
    messages: Vec<Message>,
) -> Response {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let (live_tx, mut live_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let sink: Arc<dyn StreamSink> = Arc::new(ChannelStreamSink::new(live_tx));
        // One transcoder instance for both tiers (ADR-0058 Axis 9): live `delta()`
        // for increments + `fact(RunStarted)` to open the stream. The authoritative
        // terminus comes from the committed tail, not the live channel (G10/G13).
        let mut transcoder = AgUiEncoder::new(thread.clone(), run_id.clone());
        let close_thread = thread.clone();
        // A handle kept out of the turn task so a client disconnect can cancel it.
        let rt_cancel = rt.clone();
        let turn =
            tokio::spawn(async move { rt.run_streaming(&thread, agent, messages, sink).await });
        while let Some(event) = live_rx.recv().await {
            let wires = match &event {
                AgentEvent::Delta(delta) => transcoder.delta(delta),
                AgentEvent::Fact(fact @ Fact::RunStarted) => transcoder.fact(fact),
                AgentEvent::Fact(_) => Vec::new(),
            };
            for wire in wires {
                if out_tx.send(sse_line(&wire)).is_err() {
                    // Client hung up: cancel the in-flight turn so it ends promptly
                    // instead of running to completion detached (a token leak).
                    let _ = rt_cancel.interrupt(&close_thread).await;
                    return;
                }
            }
        }
        let started = transcoder.has_streamed();
        let mut close = transcoder.finalize();
        close.extend(match turn.await {
            Ok(Ok(outcome)) if started => encode_close(&outcome, &close_thread, &run_id),
            Ok(Ok(outcome)) => encode_step(&outcome, &close_thread, &run_id),
            Ok(Err(err)) => error_events(&close_thread, &run_id, err, started),
            Err(_) => error_events(
                &close_thread,
                &run_id,
                DriverError::Internal("turn task cancelled".into()),
                started,
            ),
        });
        for event in close {
            if out_tx.send(sse_line(&event)).is_err() {
                return;
            }
        }
    });
    stream_response(out_rx)
}

/// Resume the parked tool with the matching tool result. Fails closed when there
/// is no parked tool or no matching result.
async fn resume_step(
    rt: &Runtime,
    thread: &str,
    tool_results: &[ToolResultInput],
) -> Result<StepOutcome, DriverError> {
    let pending = rt
        .pending(thread)
        .await
        .ok_or_else(|| DriverError::BadRequest("no parked run to resume".into()))?;
    let result = tool_results
        .iter()
        .find(|r| r.tool_call_id == pending.tool_use_id)
        .or_else(|| tool_results.first())
        .ok_or_else(|| DriverError::BadRequest("no tool result for the parked tool".into()))?;
    let resume = to_resume(&result.content, result.error.as_deref(), &pending);
    rt.resume(thread, &pending.tool_use_id, resume).await
}

/// Map an AG-UI tool result to a neutral resume, matching the pending tool's
/// binding. A client-executed tool delivers its content, flagged as an error when
/// the message's `error` (the AG-UI `ToolMessage.error` string) is present; a
/// built-in tool awaiting approval reads a present `error` as a denial (carrying it
/// as the note) and anything else as an allow.
fn to_resume(content: &str, error: Option<&str>, pending: &Pending) -> Resume {
    if pending.client_executed {
        Resume::ClientResult {
            content: content.to_string(),
            is_error: error.is_some(),
        }
    } else {
        Resume::Confirm {
            allow: error.is_none(),
            note: error.map(str::to_string),
        }
    }
}

/// The AG-UI SSE response headers.
fn sse_headers() -> [(header::HeaderName, HeaderValue); 1] {
    [(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    )]
}

/// Frame one AG-UI event as an SSE data line (`data: <json>\n\n`).
fn sse_line(event: &AgUiEvent) -> String {
    let json = serde_json::to_string(event).expect("ag-ui event serializes");
    format!("data: {json}\n\n")
}

/// A chunked SSE body fed from `out_rx`: each frame flushes as the producer
/// sends it, so an AG-UI client sees the run progress live.
fn stream_response(out_rx: mpsc::UnboundedReceiver<String>) -> Response {
    let stream = UnboundedReceiverStream::new(out_rx).map(Ok::<String, Infallible>);
    (StatusCode::OK, sse_headers(), Body::from_stream(stream)).into_response()
}

/// Frame a whole event list as one buffered SSE body (the non-streaming resume
/// and error paths).
fn sse_response(events: Vec<AgUiEvent>) -> Response {
    let mut body = String::new();
    for event in &events {
        body.push_str(&sse_line(event));
    }
    (StatusCode::OK, sse_headers(), body).into_response()
}

/// The AG-UI error tail. When the run has not yet been bracketed live, it opens
/// with `RUN_STARTED` first; when it already streamed, only `RUN_ERROR` is added.
fn error_events(thread: &str, run_id: &str, err: DriverError, started: bool) -> Vec<AgUiEvent> {
    let message = match err {
        DriverError::BadRequest(m) | DriverError::Internal(m) => m,
    };
    let mut out = Vec::new();
    if !started {
        out.push(AgUiEvent::RunStarted {
            thread_id: thread.to_string(),
            run_id: run_id.to_string(),
        });
    }
    out.push(AgUiEvent::error(message));
    out
}

/// A run bracketed by `RUN_STARTED` / `RUN_ERROR` for a non-streaming failure.
fn sse_error(thread: &str, run_id: &str, err: DriverError) -> Response {
    sse_response(error_events(thread, run_id, err, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(client_executed: bool) -> Pending {
        Pending {
            tool_use_id: "t1".into(),
            name: "probe".into(),
            input: serde_json::Value::Null,
            client_executed,
        }
    }

    #[test]
    fn client_executed_tool_delivers_the_content_as_its_result() {
        let r = to_resume("the answer", None, &pending(true));
        assert!(
            matches!(r, Resume::ClientResult { content, is_error: false } if content == "the answer")
        );
    }

    #[test]
    fn a_client_executed_tool_error_is_delivered_as_an_error_result() {
        let r = to_resume("it failed", Some("it failed"), &pending(true));
        assert!(
            matches!(r, Resume::ClientResult { content, is_error: true } if content == "it failed")
        );
    }

    #[test]
    fn a_builtin_tool_result_without_an_error_is_read_as_an_approval() {
        let r = to_resume("anything", None, &pending(false));
        assert!(matches!(
            r,
            Resume::Confirm {
                allow: true,
                note: None
            }
        ));
    }

    #[test]
    fn a_builtin_tool_result_with_an_error_is_a_denial() {
        // The AG-UI `error` message denies a built-in approval, carried as the note.
        let r = to_resume("ignored content", Some("not permitted"), &pending(false));
        assert!(matches!(
            r,
            Resume::Confirm { allow: false, note: Some(n) } if n == "not permitted"
        ));
    }
}
