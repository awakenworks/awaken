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
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio_stream::Stream;

use crate::state::{LiveInboxError, ManagedState, RunErrorKind, StateError};
use crate::types::{
    ErrorResponse, ListEventsResponse, SendEventsRequest, SendEventsResponse, Session,
    SessionCreateParams,
};

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
    let guard_state = state.clone();
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
        .with_state(state.clone())
        // The live-inbox is a separate Awaken protocol, not part of the
        // managed-compatible surface; it merely rides the same host + state port.
        .merge(crate::ext::live_inbox::live_inbox_router(state))
        // The tenant ownership guard (ADR-0051): a request whose resolved scope
        // does not own the addressed `/v1/sessions/{id}` is answered 404, before
        // any handler reads the session. Applied last so it wraps every id-scoped
        // route including the live-inbox surface.
        .layer(axum::middleware::from_fn_with_state(
            guard_state,
            session_scope_guard,
        ))
}

/// The tenant ownership guard for the `/v1/sessions/{id}` surface (ADR-0051): if
/// the session is owned by a scope other than the one this request resolved to,
/// answer **404** (not 403 — never disclose that the id exists in another tenant).
/// The collection routes (`POST`/`GET /v1/sessions`, no id) pass through, as do
/// ids unknown to this process (rehydration path); the request scope comes from
/// the edge-stamped [`WorkspaceScope`], defaulting to the seeded scope when the
/// deployment resolved none, so a single-tenant surface never 404s itself.
pub async fn session_scope_guard(
    State(state): State<Arc<ManagedState>>,
    request: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(id) = session_id_from_path(request.uri().path()) {
        let request_scope = request
            .extensions()
            .get::<WorkspaceScope>()
            .map(|w| w.0.clone())
            .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
        if let Some(owner) = state.resolve_owner(&id).await
            && owner != request_scope
        {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new(
                    "not_found_error",
                    format!("session `{id}` not found"),
                )),
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// The `{id}` from a `/v1/sessions/{id}[/...]` path, or `None` for the collection
/// route (`/v1/sessions`) and any non-session path. The id is the segment after
/// `sessions`; a trailing or missing segment yields `None`.
fn session_id_from_path(path: &str) -> Option<String> {
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next()? != "v1" || segments.next()? != "sessions" {
        return None;
    }
    match segments.next() {
        Some(id) if !id.is_empty() => Some(id.to_string()),
        _ => None,
    }
}

/// Map a domain error to `(status, Anthropic error envelope)`. The `error.type`
/// is the status-keyed discriminator the SDK expects; the message is preserved so
/// a caller sees *why* (e.g. a mismatched resume id), not a bare status code.
pub(crate) fn error_response(err: StateError) -> (StatusCode, Json<ErrorResponse>) {
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

/// The owning workspace a request resolved to, stamped into the request
/// extensions by the edge (the guard/ingress) from the API key. Authorization is
/// a cross-cutting aspect: the core session never stores tenancy, but the edge
/// hands the resolved workspace to `create_session` so an edge projection
/// (webhooks/usage) can stamp it. Absent when the edge resolved no workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceScope(pub String);

async fn create_session(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    ManagedJson(req): ManagedJson<SessionCreateParams>,
) -> Result<Json<Session>, (StatusCode, Json<ErrorResponse>)> {
    // Session preparation (MCP provisioning, ADR-0043 Phase 3) can fail; map the
    // RunError to the envelope exactly like a turn's failure, so a failed create
    // is loud rather than a half-provisioned session.
    state
        .create_session(req, workspace.map(|w| w.0.0.clone()))
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

/// `GET /v1/sessions` — one full page of the request scope's sessions (ADR-0051:
/// tenancy-fenced, so a workspace never lists another's).
async fn list_sessions(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
) -> Json<serde_json::Value> {
    let scope = workspace
        .map(|w| w.0.0)
        .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
    let data = state
        .list_sessions_scoped(&scope)
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
