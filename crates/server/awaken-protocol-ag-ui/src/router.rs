//! The axum router: AG-UI routes over a `ProtocolRuntime`. Handlers decode the
//! `RunAgentInput`, drive one turn or resume through the port, and project the
//! committed step into an AG-UI SSE event stream.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::stream::event::Kind;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use awaken_protocol_transport::{
    ChannelStreamSink, DriverError, Pending, ProtocolRuntime, Resume, StepOutcome,
};

use crate::encoder::{encode_close, encode_step};
use crate::live::AgUiLiveTranscoder;
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
        .with_state(runtime)
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
        let (live_tx, mut live_rx) = mpsc::unbounded_channel::<Kind>();
        let sink: Arc<dyn StreamSink> = Arc::new(ChannelStreamSink::new(live_tx));
        let mut transcoder = AgUiLiveTranscoder::new(thread.clone(), run_id.clone());
        let close_thread = thread.clone();
        let turn =
            tokio::spawn(
                async move { rt.run_turn_streaming(&thread, agent, messages, sink).await },
            );
        while let Some(kind) = live_rx.recv().await {
            for event in transcoder.transcode(&kind) {
                if out_tx.send(sse_line(&event)).is_err() {
                    return;
                }
            }
        }
        let started = transcoder.has_streamed();
        let close = match turn.await {
            Ok(Ok(outcome)) if started => encode_close(&outcome, &close_thread, &run_id),
            Ok(Ok(outcome)) => encode_step(&outcome, &close_thread, &run_id),
            Ok(Err(err)) => error_events(&close_thread, &run_id, err, started),
            Err(_) => error_events(
                &close_thread,
                &run_id,
                DriverError::Internal("turn task cancelled".into()),
                started,
            ),
        };
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
    let resume = to_resume(&result.content, &pending);
    rt.resume(thread, &pending.tool_use_id, resume).await
}

/// Map an AG-UI tool result to a neutral resume, matching the pending tool's
/// binding (client-executed delivers the content; built-in reads it as approval).
fn to_resume(content: &str, pending: &Pending) -> Resume {
    if pending.client_executed {
        Resume::ClientResult {
            content: content.to_string(),
            is_error: false,
        }
    } else {
        Resume::Confirm {
            allow: true,
            note: None,
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
