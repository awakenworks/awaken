//! The axum router: four Managed Agents routes over [`ManagedState`].
//!
//! Handlers only decode DTOs, call the state, and encode responses; no runtime
//! or protocol logic lives here. Errors map to HTTP status; live stream output is
//! a projection of committed events (SSE replay).

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio_stream::Stream;

use crate::dto::{
    CreateSessionRequest, ErrorResponse, ListEventsResponse, SendEventsRequest, SendEventsResponse,
    Session,
};
use crate::state::{ManagedState, RunErrorKind, StateError};

/// Build the Managed Agents router. Mount it at the server root; the paths are the
/// public `/v1/sessions...` surface the SDK expects.
pub fn router(state: Arc<ManagedState>) -> Router {
    Router::new()
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/:id", get(retrieve_session))
        .route(
            "/v1/sessions/:id/events",
            post(send_events).get(list_events),
        )
        .route("/v1/sessions/:id/events/stream", get(stream_events))
        .with_state(state)
}

/// Map a domain error to `(status, Anthropic error envelope)`. The `error.type`
/// is the status-keyed discriminator the SDK expects; the message is preserved so
/// a caller sees *why* (e.g. a mismatched resume id), not a bare status code.
fn error_response(err: StateError) -> (StatusCode, Json<ErrorResponse>) {
    let (status, kind, message) = match err {
        StateError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found_error",
            "session not found".to_string(),
        ),
        StateError::Run(e) => match e.kind {
            RunErrorKind::BadRequest => {
                (StatusCode::BAD_REQUEST, "invalid_request_error", e.message)
            }
            RunErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "api_error", e.message),
        },
    };
    (status, Json(ErrorResponse::new(kind, message)))
}

async fn create_session(
    State(state): State<Arc<ManagedState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Json<Session> {
    Json(state.create_session(req))
}

async fn retrieve_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<Session>, (StatusCode, Json<ErrorResponse>)> {
    state.get_session(&id).map(Json).map_err(error_response)
}

async fn send_events(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    Json(req): Json<SendEventsRequest>,
) -> Result<Json<SendEventsResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .send_events(&id, req)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn list_events(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<ListEventsResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.list_events(&id).map(Json).map_err(error_response)
}

async fn stream_events(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, (StatusCode, Json<ErrorResponse>)>
{
    let events = state.stream_events(&id).map_err(error_response)?;
    let frames = events.into_iter().map(|event| {
        // The SDK's Stream dispatches on the SSE `event:` name, so set it to the
        // event type; the JSON body carries the same `type` plus the fields.
        let name = event.type_str();
        let data = serde_json::to_string(&event).expect("event serializes");
        Ok(SseEvent::default().event(name).data(data))
    });
    Ok(Sse::new(tokio_stream::iter(frames)).keep_alive(KeepAlive::default()))
}
