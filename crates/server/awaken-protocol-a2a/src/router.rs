//! The axum router: A2A `message:send` + agent-card routes over a `ProtocolRuntime`.
//!
//! Handlers decode the request, drive one turn (or resume a parked run on the same
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

use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume};

use crate::encoder::encode_task;
use crate::request::process;
use crate::types::{
    AgentCapabilities, AgentCard, AgentSkill, ErrorResponse, SendMessageRequest,
    SendMessageResponse, Task,
};

type Runtime = Arc<dyn ProtocolRuntime>;

/// A JSON body extractor for the A2A routes. On a decode failure it returns the
/// A2A error envelope (`{ "error": { code, message } }`) with a 400, not axum's
/// plain-text rejection — so an A2A client parses the failure like any other.
struct A2aJson<T>(T);

#[async_trait::async_trait]
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
            "/v1/a2a/agents/:agent_id/message:send",
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
/// on a thread with a parked run resumes it (delivering the text as the tool
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
        // A parked run on this context → the message is the awaited input.
        Some(pending) => {
            let resume = to_resume(&processed.text, &pending);
            rt.resume(&thread, &pending.tool_use_id, resume).await?
        }
        // No parked run → a fresh turn.
        None => {
            rt.run_turn(&thread, processed.agent_id.clone(), vec![processed.message])
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

/// A minimal JSON-RPC 2.0 request envelope (the fields the A2A binding uses).
#[derive(Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// The JSON-RPC endpoint: dispatch by `method`. Only `message/send` is
/// implemented; other methods return a JSON-RPC "method not found".
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
}
