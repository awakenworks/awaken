//! The axum router: A2A `message:send` + agent-card routes over a `ProtocolRuntime`.
//!
//! Handlers decode the request, drive one turn (or resume an awaiting run on the same
//! context) through the port, and project the committed step into an A2A `Task`.
//! Errors are an HTTP status + A2A JSON error envelope — A2A `message:send` is
//! request/response, so failures are not in-stream events.

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};

use crate::encoder::encode_task;
use crate::request::process;
use crate::types::{
    AgentCapabilities, AgentCard, AgentSkill, ErrorResponse, SendMessageRequest,
    SendMessageResponse, Task, TaskState,
};

type Runtime = Arc<dyn ProtocolRuntime>;

/// A JSON body extractor for the A2A routes. On a decode failure it returns the
/// A2A error envelope (`{ "error": { code, message } }`) with a 400, not axum's
/// plain-text rejection — so an A2A client parses the failure like any other.
struct A2aJson<T>(T);

impl<S, T> FromRequest<S> for A2aJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<ErrorResponse>);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(-32600, rejection.body_text())),
            )),
        }
    }
}

/// Build the A2A router. Mount it alongside other protocol routers; the paths are
/// the `/v1/a2a...` surface an A2A `HTTP+JSON` client posts to.
pub fn router(runtime: Runtime) -> Router {
    Router::new()
        // The JSON-RPC binding (the canonical A2A transport the official SDKs
        // default to): a single endpoint dispatching by `method`.
        .route(JSONRPC_PATH, post(jsonrpc))
        // The HTTP+JSON binding (a message posted straight to a method path).
        .route(crate::client::MESSAGE_SEND_PATH, post(message_send))
        .route(
            "/v1/a2a/agents/{agent_id}/message:send",
            post(message_send_scoped),
        )
        .route(crate::client::AGENT_CARD_PATH, get(card))
        .with_state(runtime)
}

/// The JSON-RPC service endpoint (advertised as the card's `url`).
pub const JSONRPC_PATH: &str = "/v1/a2a";

async fn message_send(
    State(rt): State<Runtime>,
    A2aJson(req): A2aJson<SendMessageRequest>,
) -> Response {
    send(rt, req, None).await
}

async fn message_send_scoped(
    State(rt): State<Runtime>,
    Path(agent_id): Path<String>,
    A2aJson(req): A2aJson<SendMessageRequest>,
) -> Response {
    send(rt, req, Some(agent_id)).await
}

/// The core send logic (shared by the HTTP+JSON and JSON-RPC bindings). A message
/// on a thread with an awaiting run resumes it (delivering the text as the tool
/// answer); otherwise it is a fresh turn. Either way the committed step is
/// projected into a `Task`.
async fn run_send(
    rt: &Runtime,
    req: SendMessageRequest,
    path_agent: Option<String>,
) -> Result<Task, DriverError> {
    let processed = process(req, path_agent);
    let thread = processed.thread_id.clone();

    let step = match rt.pending(&thread).await {
        // A awaiting run on this context → the message is the awaited input.
        Some(pending) => {
            let resume = to_resume(&processed.text, &pending);
            rt.resume(&thread, &pending.tool_use_id, resume).await?
        }
        // No awaiting run → a fresh turn.
        None => {
            rt.run(&thread, processed.agent_id.clone(), vec![processed.message])
                .await?
        }
    };

    let history = rt.history(&thread).await;
    Ok(encode_task(&thread, &history, &step))
}

/// The HTTP+JSON `message:send` handler.
async fn send(rt: Runtime, req: SendMessageRequest, path_agent: Option<String>) -> Response {
    match run_send(&rt, req, path_agent).await {
        Ok(task) => (StatusCode::OK, Json(SendMessageResponse { task })).into_response(),
        Err(err) => error_response(err),
    }
}

/// Project the current state of the task on the context recovered from `id`
/// (`task-{thread}`): an awaiting run reads back as `input-required`, otherwise the
/// committed history is a `completed` task.
async fn get_task(rt: &Runtime, id: &str) -> Task {
    let thread = id.strip_prefix("task-").unwrap_or(id).to_string();
    let pending = rt.pending(&thread).await;
    let history = rt.history(&thread).await;
    let outcome = StepOutcome {
        new_messages: Vec::new(),
        terminal: if pending.is_some() {
            Terminal::Awaiting { pending }
        } else {
            Terminal::Finished
        },
    };
    encode_task(&thread, &history, &outcome)
}

/// Cancel the task on the context recovered from `id` (`task-{thread}`). A2A has
/// no in-band "deny" for a built-in tool approval; canceling the task is the
/// protocol-native way to reject it: an awaiting run is denied (unblocked with
/// `allow: false`) and the task reads back `canceled`. A task with nothing awaiting
/// is returned in its current state (not falsely canceled).
async fn cancel_task(rt: &Runtime, id: &str) -> Task {
    let thread = id.strip_prefix("task-").unwrap_or(id).to_string();
    let was_awaiting = if let Some(pending) = rt.pending(&thread).await {
        let _ = rt
            .resume(
                &thread,
                &pending.tool_use_id,
                Resume::Confirm {
                    allow: false,
                    note: Some("task canceled by the client".to_string()),
                },
            )
            .await;
        true
    } else {
        false
    };
    let history = rt.history(&thread).await;
    let mut task = encode_task(
        &thread,
        &history,
        &StepOutcome {
            new_messages: Vec::new(),
            terminal: Terminal::Finished,
        },
    );
    if was_awaiting {
        task.status.state = TaskState::Canceled;
    }
    task
}

/// A minimal JSON-RPC 2.0 request envelope (the fields the A2A binding uses).
#[derive(Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// The JSON-RPC endpoint: dispatch by `method`. `message/send` drives a turn;
/// `tasks/get` reads a task's state; `tasks/cancel` cancels/denies an awaiting task;
/// other methods return a JSON-RPC "method not found".
async fn jsonrpc(State(rt): State<Runtime>, A2aJson(req): A2aJson<JsonRpcRequest>) -> Response {
    let id = req.id;
    match req.method.as_str() {
        "message/send" => match serde_json::from_value::<SendMessageRequest>(req.params) {
            Ok(send_req) => match run_send(&rt, send_req, None).await {
                // A2A `message/send` returns the Task (or Message) directly as
                // the JSON-RPC `result`.
                Ok(task) => rpc_ok(id, task),
                Err(err) => {
                    let (code, message) = rpc_fault(err);
                    rpc_error(id, code, message)
                }
            },
            Err(err) => rpc_error(id, -32602, format!("invalid params: {err}")),
        },
        "tasks/get" => match req.params.get("id").and_then(|v| v.as_str()) {
            Some(task_id) => rpc_ok(id, get_task(&rt, task_id).await),
            None => rpc_error(id, -32602, "invalid params: missing task `id`".to_string()),
        },
        "tasks/cancel" => match req.params.get("id").and_then(|v| v.as_str()) {
            Some(task_id) => rpc_ok(id, cancel_task(&rt, task_id).await),
            None => rpc_error(id, -32602, "invalid params: missing task `id`".to_string()),
        },
        other => rpc_error(id, -32601, format!("method not found: {other}")),
    }
}

/// A JSON-RPC success: `{ jsonrpc, id, result }` on a 200.
fn rpc_ok(id: Value, task: Task) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": task })).into_response()
}

/// A JSON-RPC error member on a 200 (the transport succeeded; the call did not).
fn rpc_error(id: Value, code: i32, message: String) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}

/// Map a driver error to a JSON-RPC (code, message).
fn rpc_fault(err: DriverError) -> (i32, String) {
    match err {
        DriverError::BadRequest(m) => (-32600, m),
        DriverError::Internal(m) => (-32603, m),
    }
}

/// Map the inbound text to a neutral resume, matching the pending tool's binding:
/// a client-executed tool receives the text as its result; a built-in tool awaiting
/// approval reads any answer as an allow.
fn to_resume(text: &str, pending: &Pending) -> Resume {
    if pending.client_executed {
        Resume::ClientResult {
            content: text.to_string(),
            is_error: false,
        }
    } else {
        Resume::Confirm {
            allow: true,
            note: None,
        }
    }
}

/// Map a driver error to `(status, A2A error envelope)`.
fn error_response(err: DriverError) -> Response {
    let (status, code, message) = match err {
        DriverError::BadRequest(m) => (StatusCode::BAD_REQUEST, -32600, m),
        DriverError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, m),
    };
    (status, Json(ErrorResponse::new(code, message))).into_response()
}

async fn card(State(rt): State<Runtime>, headers: HeaderMap) -> Json<AgentCard> {
    // The card must advertise an absolute service endpoint. Derive it from the
    // request's Host so an SDK client fetching the card learns where to post.
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let mut card = agent_card(&rt.model());
    card.url = Some(format!("http://{host}{JSONRPC_PATH}"));
    Json(card)
}

/// The public agent discovery card. Streaming and push are not implemented in this
/// slice, so they are advertised false. `url` is filled in by the handler from the
/// request Host; the JSON-RPC transport is advertised as the canonical binding.
pub fn agent_card(model: &str) -> AgentCard {
    AgentCard {
        name: "assistant".to_string(),
        description: format!("Awaken agent over model `{model}`"),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: "1.0".to_string(),
        url: None,
        preferred_transport: Some("JSONRPC".to_string()),
        capabilities: AgentCapabilities {
            streaming: false,
            push_notifications: false,
        },
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: vec![AgentSkill {
            id: "chat".to_string(),
            name: "Chat".to_string(),
            tags: vec!["chat".to_string()],
        }],
        // No schemes by default: auth is the composition root's choice. A host
        // that puts auth in front of the router declares it here on the card.
        security_schemes: Default::default(),
        security: Vec::new(),
        supports_authenticated_extended_card: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_maps_faults_to_status() {
        // A caller fault is a 400; a runtime fault is a 500 — both carry the A2A
        // JSON error envelope (its shape is covered by the `types` tests).
        assert_eq!(
            error_response(DriverError::BadRequest("bad".into())).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            error_response(DriverError::Internal("boom".into())).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn agent_card_names_the_model_and_pins_the_protocol() {
        let card = agent_card("echo-model");
        assert_eq!(card.protocol_version, "1.0");
        assert!(!card.capabilities.streaming);
        assert!(card.description.contains("echo-model"));
    }

    fn pending(client_executed: bool) -> Pending {
        Pending {
            tool_use_id: "c1".into(),
            name: "t".into(),
            input: serde_json::Value::Null,
            client_executed,
        }
    }

    #[test]
    fn to_resume_delivers_the_text_to_a_client_executed_tool() {
        let r = to_resume("the answer", &pending(true));
        assert!(
            matches!(r, Resume::ClientResult { content, is_error: false } if content == "the answer")
        );
    }

    #[test]
    fn to_resume_reads_any_answer_as_an_allow_for_a_builtin_tool() {
        // A2A carries no in-band deny for a built-in approval (that is `tasks/cancel`).
        let r = to_resume("whatever", &pending(false));
        assert!(matches!(
            r,
            Resume::Confirm {
                allow: true,
                note: None
            }
        ));
    }
}
