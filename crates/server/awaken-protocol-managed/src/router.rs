//! The axum router: four Managed Agents routes over [`ManagedState`].
//!
//! Handlers only decode DTOs, call the state, and encode responses; no runtime
//! or protocol logic lives here. Errors map to HTTP status; live stream output is
//! a projection of committed events (SSE replay).

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio_stream::Stream;

use awaken_agent_contract::agent::content::ContentBlock;

use crate::dto::{
    CreateSessionRequest, ErrorResponse, ListEventsResponse, SendEventsRequest, SendEventsResponse,
    Session,
};
use crate::state::{LiveInboxError, LiveInboxSnapshot, ManagedState, RunErrorKind, StateError};

/// A JSON body extractor scoped to the Managed Agents routes. On a decode failure
/// (malformed JSON, missing/mistyped field, wrong content-type, or an unknown
/// tagged-union variant) it returns the Anthropic error envelope
/// (`invalid_request_error`, HTTP 400) instead of axum's default plain-text/422
/// rejection, so the SDK parses the failure like any other API error. Shared with
/// the vault routes so the whole managed surface answers bad bodies identically.
pub(crate) struct ManagedJson<T>(pub(crate) T);

#[async_trait::async_trait]
impl<S, T> FromRequest<S> for ManagedJson<T>
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
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    rejection.body_text(),
                )),
            )),
        }
    }
}

/// Build the Managed Agents router. Mount it at the server root; the paths are the
/// public `/v1/sessions...` surface the SDK expects.
pub fn router(state: Arc<ManagedState>) -> Router {
    Router::new()
        .route("/v1/sessions", post(create_session).get(list_sessions))
        .route(
            "/v1/sessions/:id",
            get(retrieve_session)
                .post(update_session)
                .delete(delete_session),
        )
        .route("/v1/sessions/:id/archive", post(archive_session))
        .route(
            "/v1/sessions/:id/events",
            post(send_events).get(list_events),
        )
        .route("/v1/sessions/:id/events/stream", get(stream_events))
        .route("/v1/sessions/:id/threads", get(list_threads))
        .route("/v1/sessions/:id/threads/:tid", get(get_thread))
        .route(
            "/v1/sessions/:id/threads/:tid/archive",
            post(archive_thread),
        )
        .route(
            "/v1/sessions/:id/threads/:tid/events",
            get(list_thread_events),
        )
        .route(
            "/v1/sessions/:id/threads/:tid/stream",
            get(stream_thread_events),
        )
        .route(
            "/v1/sessions/:id/resources",
            post(create_resource).get(list_resources),
        )
        .route(
            "/v1/sessions/:id/resources/:rid",
            get(get_resource)
                .post(update_resource)
                .delete(delete_resource),
        )
        .route(
            "/v1/sessions/:id/live-inbox",
            get(live_inbox_snapshot).post(live_inbox_queue),
        )
        .route("/v1/sessions/:id/live-inbox/order", put(live_inbox_reorder))
        .route(
            "/v1/sessions/:id/live-inbox/:msg",
            put(live_inbox_replace).delete(live_inbox_remove),
        )
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
        // Writing to an archived (terminated, read-only) session conflicts with the
        // session's terminal state — 409 in the shared error envelope.
        err @ StateError::Archived => (
            StatusCode::CONFLICT,
            "invalid_request_error",
            err.to_string(),
        ),
        // A create naming a nonexistent vault fails closed; the message names
        // the offending vault id (the Display impl carries it).
        err @ StateError::VaultNotFound(_) => {
            (StatusCode::NOT_FOUND, "not_found_error", err.to_string())
        }
        StateError::Run(e) => match e.kind {
            RunErrorKind::BadRequest => {
                (StatusCode::BAD_REQUEST, "invalid_request_error", e.message)
            }
            RunErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "api_error", e.message),
        },
        // The live-inbox edit contract maps 1:1 onto HTTP: an id that no longer
        // exists is 404; a stale reorder is 409 (re-GET and retry); an inactive
        // queue is 410 (the attempt is gone — send a normal event instead).
        err @ StateError::LiveInbox(LiveInboxError::UnknownMessage) => {
            (StatusCode::NOT_FOUND, "not_found_error", err.to_string())
        }
        err @ StateError::LiveInbox(LiveInboxError::StaleOrder) => (
            StatusCode::CONFLICT,
            "invalid_request_error",
            err.to_string(),
        ),
        err @ StateError::LiveInbox(LiveInboxError::Inactive) => {
            (StatusCode::GONE, "invalid_request_error", err.to_string())
        }
    };
    (status, Json(ErrorResponse::new(kind, message)))
}

/// Body of `POST /v1/sessions/:id/live-inbox` (queue) and
/// `PUT /v1/sessions/:id/live-inbox/:msg` (replace): the message's block list.
#[derive(serde::Deserialize)]
struct LiveInboxMessageBody {
    content: Vec<ContentBlock>,
}

/// Body of `PUT /v1/sessions/:id/live-inbox/order`: the full permutation of
/// currently queued ids, in the desired consumption order.
#[derive(serde::Deserialize)]
struct LiveInboxOrderBody {
    order: Vec<u64>,
}

/// Response of a successful queue: the id to edit or withdraw the message by.
#[derive(serde::Serialize)]
struct LiveInboxQueuedResponse {
    id: u64,
}

async fn live_inbox_snapshot(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<LiveInboxSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_snapshot(&id)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn live_inbox_queue(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<LiveInboxMessageBody>,
) -> Result<Json<LiveInboxQueuedResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_queue(&id, body.content)
        .await
        .map(|id| Json(LiveInboxQueuedResponse { id }))
        .map_err(error_response)
}

async fn live_inbox_remove(
    State(state): State<Arc<ManagedState>>,
    Path((id, msg)): Path<(String, u64)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_remove(&id, msg)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(error_response)
}

async fn live_inbox_replace(
    State(state): State<Arc<ManagedState>>,
    Path((id, msg)): Path<(String, u64)>,
    ManagedJson(body): ManagedJson<LiveInboxMessageBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_replace(&id, msg, body.content)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(error_response)
}

async fn live_inbox_reorder(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<LiveInboxOrderBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_reorder(&id, body.order)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(error_response)
}

/// The consumption-side project a request arrived through, stamped into the
/// request extensions by the host's `/projects/{id}` ingress middleware
/// (ADR-0042 amendment: the URL segment is ADDRESSING only — tenancy and
/// authority still flow from the API key). Absent on the bare surface.
#[derive(Debug, Clone)]
pub struct ProjectScope(pub String);

async fn create_session(
    State(state): State<Arc<ManagedState>>,
    project: Option<axum::Extension<ProjectScope>>,
    ManagedJson(req): ManagedJson<CreateSessionRequest>,
) -> Result<Json<Session>, (StatusCode, Json<ErrorResponse>)> {
    // Session preparation (MCP provisioning, ADR-0043 Phase 3) can fail; map the
    // RunError to the envelope exactly like a turn's failure, so a failed create
    // is loud rather than a half-provisioned session.
    state
        .create_session(req, project.map(|p| p.0.0.clone()))
        .await
        .map(Json)
        .map_err(error_response)
}

async fn retrieve_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<Session>, (StatusCode, Json<ErrorResponse>)> {
    state.get_session(&id).map(Json).map_err(error_response)
}

type WireErr = (StatusCode, Json<ErrorResponse>);

fn page(data: Vec<serde_json::Value>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "data": data, "has_more": false, "next_page": null }))
}

/// `GET /v1/sessions` — one full page of sessions.
async fn list_sessions(State(state): State<Arc<ManagedState>>) -> Json<serde_json::Value> {
    let data = state
        .list_sessions()
        .into_iter()
        .map(|s| serde_json::to_value(s).expect("session serializes"))
        .collect();
    page(data)
}

/// `POST /v1/sessions/:id` — update `title` (null clears) + patch `metadata`.
async fn update_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<serde_json::Value>,
) -> Result<Json<Session>, WireErr> {
    let title = body.get("title").map(|t| t.as_str().map(str::to_string));
    let metadata = body.get("metadata").and_then(|m| m.as_object()).map(|o| {
        o.iter()
            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string)))
            .collect()
    });
    state
        .update_session(&id, title, metadata)
        .map(Json)
        .map_err(error_response)
}

/// `DELETE /v1/sessions/:id`.
async fn delete_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state.delete_session(&id).map_err(error_response)?;
    Ok(Json(
        serde_json::json!({ "id": id, "type": "session_deleted" }),
    ))
}

/// `POST /v1/sessions/:id/archive`.
async fn archive_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<Session>, WireErr> {
    state.archive_session(&id).map(Json).map_err(error_response)
}

// -- Threads --

async fn list_threads(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state.list_threads(&id).map(page).map_err(error_response)
}

async fn get_thread(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state
        .get_thread(&id, &tid)
        .map(Json)
        .map_err(error_response)
}

async fn archive_thread(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state
        .archive_thread(&id, &tid)
        .map(Json)
        .map_err(error_response)
}

/// Thread events == the session's events (the primary thread), after validating
/// the thread id belongs to the session.
async fn list_thread_events(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Json<ListEventsResponse>, WireErr> {
    state.get_thread(&id, &tid).map_err(error_response)?;
    state.list_events(&id).map(Json).map_err(error_response)
}

async fn stream_thread_events(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, WireErr> {
    state.get_thread(&id, &tid).map_err(error_response)?;
    let events = state.stream_events(&id).map_err(error_response)?;
    let frames = events.into_iter().map(|event| {
        let name = event.type_str();
        let data = serde_json::to_string(&event).expect("event serializes");
        Ok(SseEvent::default().event(name).data(data))
    });
    Ok(Sse::new(tokio_stream::iter(frames)).keep_alive(KeepAlive::default()))
}

// -- Resources --

async fn create_resource(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<serde_json::Value>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state
        .create_resource(&id, body)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn list_resources(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state.list_resources(&id).map(page).map_err(error_response)
}

async fn get_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state
        .get_resource(&id, &rid)
        .map(Json)
        .map_err(error_response)
}

async fn update_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
    ManagedJson(body): ManagedJson<serde_json::Value>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state
        .update_resource(&id, &rid, body)
        .map(Json)
        .map_err(error_response)
}

async fn delete_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, WireErr> {
    state
        .delete_resource(&id, &rid)
        .await
        .map_err(error_response)?;
    Ok(Json(
        serde_json::json!({ "id": rid, "type": "session_resource_deleted" }),
    ))
}

async fn send_events(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(req): ManagedJson<SendEventsRequest>,
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
