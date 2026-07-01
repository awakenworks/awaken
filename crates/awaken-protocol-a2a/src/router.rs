//! The axum router: A2A `message:send` + agent-card routes over an [`A2aRuntime`].
//!
//! Handlers decode the request, drive one turn (or resume a parked run on the same
//! context) through the port, and project the committed step into an A2A `Task`.
//! Errors are an HTTP status + A2A JSON error envelope — A2A `message:send` is
//! request/response, so failures are not in-stream events.

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::encoder::encode_task;
use crate::port::{A2aRuntime, DriverError, Pending, Resume, StepOutcome};
use crate::request::process;
use crate::types::{
    AgentCapabilities, AgentCard, AgentSkill, ErrorResponse, SendMessageRequest,
    SendMessageResponse,
};

type Runtime = Arc<dyn A2aRuntime>;

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
        .route("/v1/a2a/message:send", post(message_send))
        .route(
            "/v1/a2a/agents/:agent_id/message:send",
            post(message_send_scoped),
        )
        .route("/v1/a2a/agent-card", get(card))
        .with_state(runtime)
}

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

/// The core `message:send` handler. A message on a thread with a parked run
/// resumes it (delivering the text as the tool answer); otherwise it is a new
/// turn. Either way the committed step is projected into a `Task`.
async fn send(rt: Runtime, req: SendMessageRequest, path_agent: Option<String>) -> Response {
    let processed = process(req, path_agent);
    let thread = processed.thread_id.clone();

    let outcome = match rt.pending(&thread).await {
        // A parked run on this context → the message is the awaited input.
        Some(pending) => {
            let resume = to_resume(&processed.text, &pending);
            rt.resume(&thread, &pending.tool_use_id, resume).await
        }
        // No parked run → a fresh turn.
        None => {
            rt.run_turn(&thread, processed.agent_id.clone(), vec![processed.message])
                .await
        }
    };

    match outcome {
        Ok(step) => task_response(&rt, &thread, step).await,
        Err(err) => error_response(err),
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

/// Project the step into a `Task` over the thread's committed history.
async fn task_response(rt: &Runtime, thread: &str, step: StepOutcome) -> Response {
    let history = rt.history(thread).await;
    let task = encode_task(thread, &history, &step);
    (StatusCode::OK, Json(SendMessageResponse { task })).into_response()
}

/// Map a driver error to `(status, A2A error envelope)`.
fn error_response(err: DriverError) -> Response {
    let (status, code, message) = match err {
        DriverError::BadRequest(m) => (StatusCode::BAD_REQUEST, -32600, m),
        DriverError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, m),
    };
    (status, Json(ErrorResponse::new(code, message))).into_response()
}

async fn card(State(rt): State<Runtime>) -> Json<AgentCard> {
    Json(agent_card(&rt.model()))
}

/// The public agent discovery card. Streaming and push are not implemented in this
/// slice, so they are advertised false.
pub fn agent_card(model: &str) -> AgentCard {
    AgentCard {
        name: "assistant".to_string(),
        description: format!("Awaken agent over model `{model}`"),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: "1.0".to_string(),
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
