//! The axum router: four Managed Agents routes over [`ManagedState`].
//!
//! Handlers only decode DTOs, call the state, and encode responses; no runtime
//! or protocol logic lives here. Errors map to HTTP status; live stream output is
//! a projection of committed events (SSE replay).

use std::convert::Infallible;
use std::sync::Arc;

use std::collections::HashSet;

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::broadcast;
use tokio_stream::Stream;

use crate::state::{LiveInboxError, ManagedState, RunError, RunErrorKind, StateError};
use crate::types::{
    DeletedSession, ErrorResponse, ListEventsResponse, Page, PageQuery, SendEventsRequest,
    SendEventsResponse, Session, SessionCreateParams, SessionThread, paginate,
};
use crate::types::{Event, StreamFrame};

/// A JSON body extractor scoped to the Managed Agents routes. On a decode failure
/// (malformed JSON, missing/mistyped field, wrong content-type, or an unknown
/// tagged-union variant) it returns the Anthropic error envelope
/// (`invalid_request_error`, HTTP 400) instead of axum's default plain-text/422
/// rejection, so the SDK parses the failure like any other API error. Shared with
/// the vault routes so the whole managed surface answers bad bodies identically.
pub(crate) struct ManagedJson<T>(pub(crate) T);

fn managed_json_message(detail: String) -> String {
    // Cause graph / decision table: a decode error under `resources[i]` is a
    // resource-union admission failure, so prefix the stable public category and
    // retain serde's exact path/detail; every other Managed body keeps its
    // existing diagnostic. This avoids coupling SDK users to Rust type wording.
    if detail.contains("resources[") {
        format!("invalid resource: {detail}")
    } else {
        detail
    }
}

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
                    managed_json_message(rejection.body_text()),
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
            "/v1/sessions/{id}",
            get(retrieve_session)
                .post(update_session)
                .delete(delete_session),
        )
        .route("/v1/sessions/{id}/archive", post(archive_session))
        .route(
            "/v1/sessions/{id}/events",
            post(send_events).get(list_events),
        )
        .route("/v1/sessions/{id}/events/stream", get(stream_events))
        .route("/v1/sessions/{id}/threads", get(list_threads))
        .route("/v1/sessions/{id}/threads/{tid}", get(get_thread))
        .route(
            "/v1/sessions/{id}/threads/{tid}/archive",
            post(archive_thread),
        )
        .route(
            "/v1/sessions/{id}/threads/{tid}/events",
            get(list_thread_events),
        )
        .route(
            "/v1/sessions/{id}/threads/{tid}/stream",
            get(stream_thread_events),
        )
        .route(
            "/v1/sessions/{id}/resources",
            post(create_resource).get(list_resources),
        )
        .route(
            "/v1/sessions/{id}/resources/{rid}",
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
        err @ (StateError::Archived | StateError::Conflict | StateError::IdempotencyMismatch) => (
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
///
/// Re-exported from [`awaken_tenancy`] — its orthogonal home (tenancy is an edge
/// aspect, ADR-0051, decoupled from the session-runtime contract) — so existing
/// `crate::…::WorkspaceScope` paths keep working.
pub use awaken_tenancy::WorkspaceScope;

/// Axum middleware enforcing the `anthropic-beta: managed-agents-2026-04-01` opt-in
/// on every ordinary Managed Agents endpoint. Applied by each executable
/// composition root, NOT baked into [`router`], so router-level tests remain focused
/// on domain behavior. Endpoint families with their own beta (Memory, User Profiles,
/// Files) are deliberately left to their family-specific gate.
pub async fn enforce_managed_beta(
    req: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();
    let is_family = |family: &str| path == family || path.starts_with(&format!("{family}/"));
    let is_managed = [
        "/v1/sessions",
        "/v1/agents",
        "/v1/environments",
        "/v1/deployments",
        "/v1/deployment_runs",
        "/v1/vaults",
        "/v1/skills",
    ]
    .into_iter()
    .any(is_family);
    if is_managed {
        let opted_in = req
            .headers()
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|b| b.trim() == awaken_managed_bridge::MANAGED_BETA);
        if !opted_in {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    format!(
                        "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                        beta = awaken_managed_bridge::MANAGED_BETA,
                    ),
                )),
            )
                .into_response();
        }
    }
    next.run(req).await
}

async fn create_session(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    ManagedJson(req): ManagedJson<SessionCreateParams>,
) -> Result<(HeaderMap, Json<Session>), (StatusCode, Json<ErrorResponse>)> {
    // Session preparation (MCP provisioning, ADR-0043 Phase 3) can fail; map the
    // RunError to the envelope exactly like a turn's failure, so a failed create
    // is loud rather than a half-provisioned session.
    let initial_events = req.initial_events.clone();
    let mut session = state
        .create_session(req, workspace.map(|w| w.0.0.clone()))
        .await
        .map_err(error_response)?;
    if !initial_events.is_empty() {
        state
            .start_initial_events(&session.id, initial_events)
            .map_err(error_response)?;
        session.status = "running";
    }
    versioned_session_response(&state, session, None).await
}

async fn retrieve_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<(HeaderMap, Json<Session>), (StatusCode, Json<ErrorResponse>)> {
    state.ensure_session(&id).await.map_err(error_response)?;
    let session = state.get_session(&id).map_err(error_response)?;
    versioned_session_response(&state, session, None).await
}

async fn versioned_session_response(
    state: &ManagedState,
    session: Session,
    operation_id: Option<String>,
) -> Result<(HeaderMap, Json<Session>), WireErr> {
    let revision = state
        .session_revision(&session.id)
        .await
        .map_err(error_response)?;
    Ok(versioned_session_response_at_revision(
        session,
        revision,
        operation_id,
    ))
}

fn versioned_session_response_at_revision(
    session: Session,
    revision: awaken_session_contract::SessionRevision,
    operation_id: Option<String>,
) -> (HeaderMap, Json<Session>) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", revision.0))
            .expect("numeric Session revision is a valid ETag"),
    );
    if let Some(operation_id) = operation_id {
        headers.insert(
            "x-awaken-operation-id",
            HeaderValue::from_str(&operation_id).expect("fingerprint is a valid header value"),
        );
    }
    (headers, Json(session))
}

type WireErr = (StatusCode, Json<ErrorResponse>);

/// `GET /v1/sessions` — a cursor page of the request scope's sessions (ADR-0051:
/// tenancy-fenced, so a workspace never lists another's).
async fn list_sessions(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    Query(page): Query<PageQuery>,
) -> Json<Page<Session>> {
    let scope = workspace
        .map(|w| w.0.0)
        .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
    let data = state.list_sessions_scoped(&scope);
    Json(paginate(data, &page, |s| s.id.as_str()))
}

/// `POST /v1/sessions/:id` — update `title` (null clears) + patch `metadata`.
async fn update_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ManagedJson(body): ManagedJson<crate::types::SessionUpdateParams>,
) -> Result<(HeaderMap, Json<Session>), WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    if body.vault_ids.is_some() {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "vault_ids is reserved and not yet supported on Session update",
        ))));
    }
    let (tools, mcp_servers) = body
        .agent
        .map(|agent| (agent.tools, agent.mcp_servers))
        .unwrap_or_default();
    let title = body.title;
    let metadata = body.metadata;
    let idempotency_key = headers
        .get("idempotency-key")
        .map(|value| {
            value.to_str().map(str::to_string).map_err(|_| {
                error_response(StateError::Run(RunError::bad_request(
                    "Idempotency-Key must be visible ASCII",
                )))
            })
        })
        .transpose()?;
    if idempotency_key
        .as_ref()
        .is_some_and(|key| key.trim().is_empty() || key.len() > 255)
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "Idempotency-Key must contain 1 to 255 characters",
        ))));
    }
    let if_match = headers
        .get(header::IF_MATCH)
        .map(|value| {
            let raw = value.to_str().map_err(|_| {
                error_response(StateError::Run(RunError::bad_request(
                    "If-Match must be a quoted Session revision",
                )))
            })?;
            if raw == "*" {
                return Ok(None);
            }
            let revision = raw
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    error_response(StateError::Run(RunError::bad_request(
                        "If-Match must be `*` or a quoted Session revision",
                    )))
                })?;
            Ok(Some(awaken_session_contract::SessionRevision(revision)))
        })
        .transpose()?
        .flatten();
    let operation_id = idempotency_key
        .as_deref()
        .map(|key| ManagedState::update_operation_id(&id, key));
    let (session, command_revision) = state
        .update_session(
            &id,
            crate::state::SessionUpdateCommand {
                title,
                metadata,
                tools,
                mcp_servers,
                idempotency_key,
                if_match,
            },
        )
        .await
        .map_err(error_response)?;
    Ok(versioned_session_response_at_revision(
        session,
        command_revision,
        operation_id,
    ))
}

/// `DELETE /v1/sessions/:id`.
async fn delete_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedSession>, WireErr> {
    state.delete_session(&id).await.map_err(error_response)?;
    Ok(Json(DeletedSession {
        id,
        kind: "session_deleted",
    }))
}

/// `POST /v1/sessions/:id/archive`.
async fn archive_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<Session>, WireErr> {
    state
        .archive_session(&id)
        .await
        .map(Json)
        .map_err(error_response)
}

// -- Threads --

async fn list_threads(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<Page<SessionThread>>, WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    state
        .list_threads(&id)
        .map(Page::single)
        .map(Json)
        .map_err(error_response)
}

async fn get_thread(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Json<SessionThread>, WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    state
        .get_thread(&id, &tid)
        .map(Json)
        .map_err(error_response)
}

async fn archive_thread(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Json<SessionThread>, WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    state
        .archive_thread(&id, &tid)
        .await
        .map(Json)
        .map_err(error_response)
}

/// Thread events are a typed projection of the Session's one committed event log.
async fn list_thread_events(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
) -> Result<Json<ListEventsResponse>, WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    state
        .list_thread_events(&id, &tid, query.page.as_deref(), query.limit)
        .map(Json)
        .map_err(error_response)
}

/// Parse the SDK's `event_deltas[]` live-preview opt-in. Repeated `event_deltas[]`
/// (or `event_deltas`) values select which buffered events to preview; only
/// `agent.message` and `agent.thinking` are accepted (any other value is a 400,
/// matching the official wire). Returns whether any preview was requested — awaken
/// previews `agent.message` text; `agent.thinking` is accepted but never emitted
/// (awaken's live stream carries no thinking channel).
fn parse_event_deltas(raw: Option<&str>) -> Result<bool, WireErr> {
    let mut requested = false;
    let mut count = 0usize;
    if let Some(q) = raw {
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            if k == "event_deltas[]" || k == "event_deltas" {
                count += 1;
                if count > 100 {
                    return Err(error_response(
                        RunError::bad_request("event_deltas allows at most 100 values").into(),
                    ));
                }
                match v.as_ref() {
                    "agent.message" | "agent.thinking" => requested = true,
                    other => {
                        return Err(error_response(
                            RunError::bad_request(format!(
                                "event_deltas: unsupported value `{other}` \
                                 (only agent.message, agent.thinking)"
                            ))
                            .into(),
                        ));
                    }
                }
            }
        }
    }
    Ok(requested)
}

/// The terminal committed events that close a turn's SSE stream.
fn is_terminal(frame: &StreamFrame) -> bool {
    matches!(
        frame,
        StreamFrame::Committed(e)
            if matches!(
                e.type_str(),
                "session.status_idle"
                    | "session.status_terminated"
                    | "session.deleted"
                    | "session.thread_status_idle"
                    | "session.thread_status_terminated"
            )
    )
}

fn sse_frame(frame: &StreamFrame) -> SseEvent {
    // The SDK dispatches on the SSE `event:` name; the JSON body carries the same
    // `type` plus the fields (committed event, or a stream-only preview).
    SseEvent::default()
        .event(frame.type_str())
        .data(frame.data())
}

/// The live SSE body: the committed snapshot (backfill, deduped against the live
/// tail by id), then live broadcast frames until a terminal committed event or the
/// session's sender drops. Preview frames are forwarded only if `previews` is set.
fn live_sse_stream<F>(
    snapshot: Vec<Event>,
    mut rx: broadcast::Receiver<StreamFrame>,
    previews: bool,
    project: F,
) -> impl Stream<Item = Result<SseEvent, Infallible>>
where
    F: Fn(StreamFrame) -> Option<StreamFrame> + Send + Sync + 'static,
{
    async_stream::stream! {
        let mut seen: HashSet<String> = HashSet::new();
        let mut backfill_terminal = false;
        for event in snapshot {
            let Some(StreamFrame::Committed(event)) = project(StreamFrame::Committed(event)) else {
                continue;
            };
            seen.insert(event.id.clone());
            let frame = StreamFrame::Committed(event);
            backfill_terminal = is_terminal(&frame);
            yield Ok(sse_frame(&frame));
        }
        // A snapshot that already reached idle/terminated is a completed turn
        // (send-then-stream): deliver the backfill and end, preserving
        // request/response semantics. Otherwise tail the live broadcast.
        if !backfill_terminal {
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        let Some(frame) = project(frame) else {
                            continue;
                        };
                        let StreamFrame::Committed(event) = frame else {
                            if previews {
                                yield Ok(sse_frame(&frame));
                            }
                            continue;
                        };
                        // Dedupe the snapshot/live overlap by id; only end on a
                        // committed terminal.
                        if !seen.insert(event.id.clone()) {
                            continue;
                        }
                        let frame = StreamFrame::Committed(event);
                        let terminal = is_terminal(&frame);
                        yield Ok(sse_frame(&frame));
                        if terminal {
                            break;
                        }
                    }
                    // Best-effort: a lagging subscriber skips the dropped frames
                    // (the buffered agent.message still arrives); a closed sender ends.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn stream_thread_events(
    State(state): State<Arc<ManagedState>>,
    Path((id, tid)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, WireErr> {
    // The official Thread EventStreamParams carries the same preview selector as
    // the Session stream. Primary-thread previews are the Session previews; child
    // execution currently has no independent preview producer.
    let previews = parse_event_deltas(raw.as_deref())?;
    state.ensure_session(&id).await.map_err(error_response)?;
    state.get_thread(&id, &tid).map_err(error_response)?;
    let (snapshot, rx) = state.stream_subscribe(&id).map_err(error_response)?;
    let primary = tid == format!("{id}:primary");
    let project_session = id.clone();
    let project_thread = tid.clone();
    Ok(Sse::new(live_sse_stream(
        snapshot,
        rx,
        previews,
        move |frame| match frame {
            StreamFrame::Committed(event) => {
                ManagedState::project_event_for_thread(&project_session, &project_thread, event)
                    .map(StreamFrame::Committed)
            }
            preview @ StreamFrame::Preview(_) if primary => Some(preview),
            StreamFrame::Preview(_) => None,
        },
    ))
    .keep_alive(KeepAlive::default()))
}

// -- Resources --

async fn create_resource(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<crate::types::resource::ResourceAddParams>,
) -> Result<Json<crate::types::resource::SessionResource>, WireErr> {
    state
        .create_resource(&id, body)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn list_resources(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<Page<crate::types::resource::SessionResource>>, WireErr> {
    state
        .list_resources(&id)
        .map(Page::single)
        .map(Json)
        .map_err(error_response)
}

async fn get_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<crate::types::resource::SessionResource>, WireErr> {
    state
        .get_resource(&id, &rid)
        .map(Json)
        .map_err(error_response)
}

async fn update_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
    ManagedJson(body): ManagedJson<crate::types::resource::ResourceUpdateParams>,
) -> Result<Json<crate::types::resource::SessionResource>, WireErr> {
    state
        .update_resource(&id, &rid, body)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn delete_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<crate::types::resource::DeletedSessionResource>, WireErr> {
    state
        .delete_resource(&id, &rid)
        .await
        .map_err(error_response)?;
    Ok(Json(crate::types::resource::DeletedSessionResource {
        id: rid,
        kind: "session_resource_deleted",
    }))
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
    Query(query): Query<PageQuery>,
) -> Result<Json<ListEventsResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .list_events(&id, query.page.as_deref(), query.limit)
        .map(Json)
        .map_err(error_response)
}

async fn stream_events(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, WireErr> {
    // Opt in to live previews (`event_start`/`event_delta`) via `event_deltas[]`.
    let previews = parse_event_deltas(raw.as_deref())?;
    let (snapshot, rx) = state.stream_subscribe(&id).map_err(error_response)?;
    Ok(Sse::new(live_sse_stream(snapshot, rx, previews, Some)).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod managed_json_tests {
    use super::managed_json_message;

    #[test]
    fn resource_decode_errors_have_a_stable_category_and_keep_the_path() {
        let detail = "resources[0].type: unknown variant `future_resource`".to_string();
        let message = managed_json_message(detail.clone());
        assert!(message.starts_with("invalid resource:"));
        assert!(message.ends_with(&detail));
        assert_eq!(
            managed_json_message("model: missing field".into()),
            "model: missing field"
        );
    }
}
