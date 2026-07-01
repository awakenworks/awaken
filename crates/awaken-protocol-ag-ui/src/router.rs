//! The axum router: AG-UI routes over an [`AgUiRuntime`]. Handlers decode the
//! `RunAgentInput`, drive one turn or resume through the port, and project the
//! committed step into an AG-UI SSE event stream.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use crate::encoder::encode_step;
use crate::port::{AgUiRuntime, DriverError, Pending, Resume, StepOutcome};
use crate::request::{ToolResultInput, process};
use crate::types::{AgUiEvent, RunAgentInput};

type Runtime = Arc<dyn AgUiRuntime>;

/// A JSON body extractor for the AG-UI routes. On a decode failure (malformed
/// JSON, wrong field type, bad content-type) it returns a bare `RUN_ERROR` event
/// stream rather than axum's plain-text 400, so an AG-UI client sees the failure
/// as a run error. The body never parsed, so there is no run to bracket with
/// `RUN_STARTED`.
struct AgUiJson<T>(T);

#[async_trait::async_trait]
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
        .route("/v1/ag-ui/agents/:agent_id", post(run_agent_scoped))
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

    let outcome = if processed.messages.is_empty() {
        match resume_step(&rt, &thread, &processed.tool_results).await {
            Ok(outcome) => outcome,
            Err(err) => return sse_error(&thread, &run_id, err),
        }
    } else {
        match rt
            .run_turn(&thread, processed.agent_id.clone(), processed.messages)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => return sse_error(&thread, &run_id, err),
        }
    };

    sse_response(encode_step(&outcome, &thread, &run_id))
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

/// Frame AG-UI events as a Server-Sent Events body (`data: <json>\n\n`).
fn sse_response(events: Vec<AgUiEvent>) -> Response {
    let mut body = String::new();
    for event in &events {
        let json = serde_json::to_string(event).expect("ag-ui event serializes");
        body.push_str("data: ");
        body.push_str(&json);
        body.push_str("\n\n");
    }
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )],
        body,
    )
        .into_response()
}

/// A run bracketed by `RUN_STARTED` / `RUN_ERROR` for a driver failure.
fn sse_error(thread: &str, run_id: &str, err: DriverError) -> Response {
    let message = match err {
        DriverError::BadRequest(m) | DriverError::Internal(m) => m,
    };
    sse_response(vec![
        AgUiEvent::RunStarted {
            thread_id: thread.to_string(),
            run_id: run_id.to_string(),
        },
        AgUiEvent::error(message),
    ])
}
