//! The axum router: AI SDK v6 UI Message Stream routes over a `ProtocolRuntime`.
//!
//! Handlers decode the request, drive one turn or resume through the port, and
//! project the committed step into a UI Message Stream SSE response. No runtime or
//! protocol logic lives here beyond routing and framing.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};

use crate::encoder::{encode_history, encode_step};
use crate::request::{DecisionKind, process_request, result_text};
use crate::types::{AiSdkChatRequest, HistoryResponse, UIStreamEvent};

type Runtime = Arc<dyn ProtocolRuntime>;

/// The AI SDK v6 header `DefaultChatTransport` uses to identify the stream format.
const AI_SDK_STREAM_HEADER: &str = "x-vercel-ai-ui-message-stream";

/// A JSON body extractor for the AI SDK routes. On a decode failure (malformed
/// JSON, wrong field type, bad content-type) it returns the UI Message Stream
/// error frame (`error` + `finish("error")`) rather than axum's plain-text 400,
/// so `useChat` sees the failure as a stream error like any driver error.
struct AiSdkJson<T>(T);

#[async_trait::async_trait]
impl<S, T> FromRequest<S> for AiSdkJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(sse_error(DriverError::BadRequest(rejection.body_text()))),
        }
    }
}

/// Build the AI SDK router. Mount it alongside other protocol routers; the paths
/// are the `/v1/ai-sdk/...` surface the `useChat` transport expects.
pub fn router(runtime: Runtime) -> Router {
    Router::new()
        .route("/v1/ai-sdk/chat", post(chat))
        .route("/v1/ai-sdk/threads/:thread_id/runs", post(chat_threaded))
        .route("/v1/ai-sdk/agents/:agent_id/runs", post(chat_agent_scoped))
        .route(
            "/v1/ai-sdk/threads/:thread_id/messages",
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
    AiSdkJson(mut payload): AiSdkJson<AiSdkChatRequest>,
) -> Response {
    payload.thread_id = Some(thread_id);
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

    let outcome = if processed.messages.is_empty() {
        // Resume: answer the pending tool with the matching client decision.
        match resume_step(&rt, &thread, &processed.decisions).await {
            Ok(outcome) => outcome,
            Err(response) => return response,
        }
    } else {
        match rt
            .run_turn(&thread, processed.agent_id.clone(), processed.messages)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => return sse_error(err),
        }
    };

    sse_response(encode_step(&outcome))
}

/// Translate the request's decisions into a resume against the parked tool and
/// run it. Fails closed when there is no parked tool or no matching decision.
async fn resume_step(
    rt: &Runtime,
    thread: &str,
    decisions: &[crate::request::Decision],
) -> Result<StepOutcome, Response> {
    let Some(pending) = rt.pending(thread).await else {
        return Err(sse_error(DriverError::BadRequest(
            "no parked run to resume".into(),
        )));
    };
    let decision = decisions
        .iter()
        .find(|d| d.tool_call_id == pending.tool_use_id)
        .or_else(|| decisions.first())
        .ok_or_else(|| {
            sse_error(DriverError::BadRequest(
                "no tool decision for the parked tool".into(),
            ))
        })?;
    let resume = to_resume(&decision.kind, &pending);
    rt.resume(thread, &pending.tool_use_id, resume)
        .await
        .map_err(sse_error)
}

/// Map a client decision to a neutral resume, choosing the variant that matches
/// the pending tool's binding (client-executed vs built-in awaiting approval).
fn to_resume(kind: &DecisionKind, pending: &Pending) -> Resume {
    if pending.client_executed {
        match kind {
            DecisionKind::Output(v) => Resume::ClientResult {
                content: result_text(v),
                is_error: false,
            },
            DecisionKind::Error(e) => Resume::ClientResult {
                content: e.clone(),
                is_error: true,
            },
            DecisionKind::Denied => Resume::ClientResult {
                content: "client denied the tool".into(),
                is_error: true,
            },
            DecisionKind::Approved => Resume::ClientResult {
                content: String::new(),
                is_error: false,
            },
        }
    } else {
        match kind {
            DecisionKind::Approved | DecisionKind::Output(_) => Resume::Confirm {
                allow: true,
                note: None,
            },
            DecisionKind::Denied => Resume::Confirm {
                allow: false,
                note: None,
            },
            DecisionKind::Error(e) => Resume::Confirm {
                allow: false,
                note: Some(e.clone()),
            },
        }
    }
}

async fn thread_messages(
    State(rt): State<Runtime>,
    Path(thread_id): Path<String>,
) -> Json<HistoryResponse> {
    let messages = encode_history(&rt.history(&thread_id).await);
    Json(HistoryResponse { messages })
}

/// Frame UI stream events as a Server-Sent Events body (`data: <json>\n\n`),
/// terminated by `[DONE]`, with the AI SDK transport header.
fn sse_response(events: Vec<UIStreamEvent>) -> Response {
    let mut body = String::new();
    for event in &events {
        let json = serde_json::to_string(event).expect("ui stream event serializes");
        body.push_str("data: ");
        body.push_str(&json);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            ),
            (
                header::HeaderName::from_static(AI_SDK_STREAM_HEADER),
                HeaderValue::from_static("v1"),
            ),
        ],
        body,
    )
        .into_response()
}

/// A single-frame SSE error stream (AI SDK errors use `errorText`).
fn sse_error(err: DriverError) -> Response {
    let message = match err {
        DriverError::BadRequest(m) | DriverError::Internal(m) => m,
    };
    sse_response(vec![
        UIStreamEvent::error(message),
        UIStreamEvent::finish("error"),
    ])
}
