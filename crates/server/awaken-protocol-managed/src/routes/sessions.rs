//! The axum router: four Managed Agents routes over [`ManagedState`].
//!
//! Handlers only decode DTOs, call the state, and encode responses; no runtime
//! or protocol logic lives here. Errors map to HTTP status; live stream output is
//! a projection of committed events (SSE replay).

use std::convert::Infallible;
use std::sync::Arc;

use std::collections::HashSet;

use awaken_tenancy::WorkspaceScope;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::broadcast;
use tokio_stream::Stream;

use crate::common::headers::{
    ANTHROPIC_API_VERSION, DREAMING_BETA, MANAGED_BETA, MEMORY_BETA, ManagedCapability,
    SKILLS_BETA, USER_PROFILES_BETA_LATEST, has_capability,
};
use crate::preview::ThreadPreviewProjector;
use crate::state::{ManagedState, RunError, RunErrorKind, StateError, internal_thread_id};
use crate::types::{
    DeletedSession, ErrorResponse, ListEventsResponse, PageCursor, PageQuery, SendEventsRequest,
    SendEventsResponse, Session, SessionCreateParams, SessionThread,
};
use crate::types::{Event, StreamFrame};

mod pagination;

use pagination::{SessionListPage, parse_session_list, session_list_page};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionListOrder {
    Asc,
    Desc,
}

impl SessionListOrder {
    fn parse(value: &str) -> Result<Self, WireErr> {
        match value {
            "asc" => Ok(Self::Asc),
            "desc" => Ok(Self::Desc),
            _ => Err(error_response(StateError::Run(RunError::bad_request(
                "order must be `asc` or `desc`",
            )))),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

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
    if let Some(id) = session_id_from_path(request.uri().path())
        && let Err(error) = ensure_session_scope(
            state.as_ref(),
            &id,
            request.extensions().get::<WorkspaceScope>(),
        )
        .await
    {
        return error.into_response();
    }
    next.run(request).await
}

async fn ensure_session_scope(
    state: &ManagedState,
    id: &str,
    workspace: Option<&WorkspaceScope>,
) -> Result<(), WireErr> {
    let request_scope = workspace
        .map(|workspace| workspace.0.clone())
        .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
    match state.resolve_owner(id).await {
        Ok(Some(owner)) if owner != request_scope => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found_error",
                format!("session `{id}` not found"),
            )),
        )),
        Err(error) => Err(error_response(error)),
        Ok(Some(_)) | Ok(None) => Ok(()),
    }
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
        err @ (StateError::Archived
        | StateError::Conflict
        | StateError::IdempotencyMismatch
        | StateError::TerminalCreateConflict) => (
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
            RunErrorKind::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "api_error", e.message),
        },
        // No compatible route emits these errors. Keep the generic envelope for
        // internal exhaustive mapping; awaken-protocol-awaken owns the extension's
        // public status decision table.
        err @ StateError::LiveInbox(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            err.to_string(),
        ),
    };
    (status, Json(ErrorResponse::new(kind, message)))
}

fn path_is_family(path: &str, family: &str) -> bool {
    let direct = path == family || path.starts_with(&format!("{family}/"));
    if direct {
        return true;
    }
    let Some(scoped) = path.strip_prefix("/v1/workspaces/") else {
        return false;
    };
    let Some((workspace, tail)) = scoped.split_once('/') else {
        return false;
    };
    if workspace.is_empty() {
        return false;
    }
    let Some(relative_family) = family.strip_prefix("/v1/") else {
        return false;
    };
    tail == relative_family || tail.starts_with(&format!("{relative_family}/"))
}

/// Axum middleware enforcing the `anthropic-beta: managed-agents-2026-04-01` opt-in
/// on every ordinary Managed Agents endpoint. Applied by each executable
/// process startup, NOT baked into [`router`], so router-level tests remain focused
/// on domain behavior. Memory accepts its current endpoint beta or the legacy
/// Managed selector. Dreams and Skills use their generated SDK endpoint betas;
/// User Profiles and Files retain their family gates.
pub async fn enforce_managed_beta(
    req: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Both supported SDK anchors currently send the same API date. Validate an
    // explicit date, but retain missing-header acceptance for Awaken's legacy
    // raw clients; a package version is never inferred from telemetry headers.
    if req
        .headers()
        .get("anthropic-version")
        .is_some_and(|value| value.to_str().ok() != Some(ANTHROPIC_API_VERSION))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "unsupported `anthropic-version`; this endpoint supports {ANTHROPIC_API_VERSION}"
                ),
            )),
        )
            .into_response();
    }
    let path = req.uri().path();
    // Workspace-addressed routes are rewritten by an inner composition layer.
    // Admission runs outside that layer, so classify both public spellings here
    // or `/v1/workspaces/{id}/...` could bypass an endpoint beta gate.
    let is_family = |family: &str| path_is_family(path, family);
    if is_family("/v1/memory_stores") {
        let has_memory = has_capability(req.headers(), ManagedCapability::Memory);
        let has_managed = has_capability(req.headers(), ManagedCapability::ManagedAgents);
        if has_memory == has_managed {
            let message = if has_memory {
                format!(
                    "the {MEMORY_BETA} beta replaces {managed} on memory store endpoints; do not send both",
                    managed = MANAGED_BETA,
                )
            } else {
                format!(
                    "a Memory beta is required: send `anthropic-beta: {MEMORY_BETA}`; legacy clients may send `{managed}` alone",
                    managed = MANAGED_BETA,
                )
            };
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("invalid_request_error", message)),
            )
                .into_response();
        }
    }
    let query_requests_beta = req.uri().query().is_some_and(|query| {
        form_urlencoded::parse(query.as_bytes())
            .any(|(name, value)| name == "beta" && value == "true")
    });
    if is_family("/v1/skills")
        && query_requests_beta
        && !has_capability(req.headers(), ManagedCapability::Skills)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!("`beta=true` requires the `anthropic-beta: {SKILLS_BETA}` header"),
            )),
        )
            .into_response();
    }
    if is_family("/v1/dreams") && !has_capability(req.headers(), ManagedCapability::Dreams) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = DREAMING_BETA,
                ),
            )),
        )
            .into_response();
    }
    if is_family("/v1/user_profiles")
        && !has_capability(req.headers(), ManagedCapability::UserProfilesLegacy)
        && !has_capability(req.headers(), ManagedCapability::UserProfilesCurrent)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "a User Profiles beta is required: send `anthropic-beta: {legacy}` or `anthropic-beta: {latest}`",
                    legacy = crate::USER_PROFILES_BETA,
                    latest = USER_PROFILES_BETA_LATEST,
                ),
            )),
        )
            .into_response();
    }
    if is_family("/v1/tunnels") && !has_capability(req.headers(), ManagedCapability::TunnelsCurrent)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = crate::TUNNELS_BETA,
                ),
            )),
        )
            .into_response();
    }
    if is_family("/v1/organizations/tunnels")
        && !has_capability(req.headers(), ManagedCapability::TunnelsLegacy)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = crate::LEGACY_TUNNELS_BETA,
                ),
            )),
        )
            .into_response();
    }
    let is_managed = [
        "/v1/sessions",
        "/v1/agents",
        "/v1/environments",
        "/v1/deployments",
        "/v1/deployment_runs",
        "/v1/vaults",
    ]
    .into_iter()
    .any(is_family);
    if is_managed && !has_capability(req.headers(), ManagedCapability::ManagedAgents) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = MANAGED_BETA,
                ),
            )),
        )
            .into_response();
    }
    next.run(req).await
}

async fn create_session(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    headers: HeaderMap,
    ManagedJson(req): ManagedJson<SessionCreateParams>,
) -> Result<(HeaderMap, Json<Session>), (StatusCode, Json<ErrorResponse>)> {
    req.validate_public()
        .map_err(|message| error_response(StateError::Run(RunError::bad_request(message))))?;
    // Session preparation (MCP provisioning, ADR-0043 Phase 3) can fail; map the
    // RunError to the envelope exactly like a Run failure, so a failed create
    // is loud rather than a half-provisioned session.
    let workspace_id = workspace.map(|w| w.0.0.clone());
    let idempotency_key = parse_idempotency_key(&headers)?;
    let session = match idempotency_key.as_deref() {
        Some(key) => {
            state
                .create_session_idempotent(req, workspace_id.clone(), key)
                .await
        }
        None => state.create_session(req, workspace_id.clone()).await,
    }
    .map_err(error_response)?;
    versioned_session_response(&state, session).await
}

pub(crate) fn parse_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, WireErr> {
    crate::parse_idempotency_key_header(headers)
        .map_err(|message| error_response(StateError::Run(RunError::bad_request(message))))
}

fn parse_if_match(
    headers: &HeaderMap,
) -> Result<Option<awaken_session_contract::SessionRevision>, WireErr> {
    headers
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
        .transpose()
        .map(Option::flatten)
}

async fn retrieve_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<(HeaderMap, Json<Session>), (StatusCode, Json<ErrorResponse>)> {
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    let session = state.get_session(&id).map_err(error_response)?;
    versioned_session_response(&state, session).await
}

async fn versioned_session_response(
    state: &ManagedState,
    session: Session,
) -> Result<(HeaderMap, Json<Session>), WireErr> {
    let revision = state
        .session_revision(&session.id)
        .await
        .map_err(error_response)?;
    Ok(versioned_session_response_at_revision(session, revision))
}

fn versioned_session_response_at_revision(
    session: Session,
    revision: awaken_session_contract::SessionRevision,
) -> (HeaderMap, Json<Session>) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", revision.0))
            .expect("numeric Session revision is a valid ETag"),
    );
    (headers, Json(session))
}

type WireErr = (StatusCode, Json<ErrorResponse>);

/// `GET /v1/sessions` — a cursor page of the request scope's sessions (ADR-0051:
/// tenancy-fenced, so a workspace never lists another's).
async fn list_sessions(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<SessionListPage>, WireErr> {
    let scope = workspace
        .map(|w| w.0.0)
        .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
    let data = state
        .list_sessions_scoped_durable(&scope)
        .await
        .map_err(error_response)?;
    let params = parse_session_list(raw.as_deref())?;
    session_list_page(data, &params).map(Json)
}

/// `POST /v1/sessions/:id` — update `title` (null clears) + patch `metadata`.
async fn update_session(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ManagedJson(body): ManagedJson<crate::types::SessionUpdateParams>,
) -> Result<(HeaderMap, Json<Session>), WireErr> {
    body.validate()
        .map_err(|message| error_response(StateError::Run(RunError::bad_request(message))))?;
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
    let budget = match body.budget {
        None => None,
        Some(None) => Some(None),
        Some(Some(limit)) => Some(Some(limit.max_list_cost_minor().map_err(|message| {
            error_response(StateError::Run(RunError::bad_request(message)))
        })?)),
    };
    // Managed wire equivalence is stable across the application-layer move so
    // durable receipts written by a previous process version remain replayable.
    let request_fingerprint = match budget {
        None => {
            awaken_session_contract::stable_fingerprint(&(&title, &metadata, &tools, &mcp_servers))
        }
        Some(_) => awaken_session_contract::stable_fingerprint(&(
            &title,
            &metadata,
            &tools,
            &mcp_servers,
            &budget,
        )),
    };
    let idempotency_key = parse_idempotency_key(&headers)?;
    let if_match = parse_if_match(&headers)?;
    let title = title.map(|value| match value {
        Some(value) => awaken_session_application::SessionFieldUpdate::Replace(value),
        None => awaken_session_application::SessionFieldUpdate::Clear,
    });
    let metadata = metadata.map(|value| match value {
        Some(value) => awaken_session_application::SessionMetadataUpdate::Patch(value),
        None => awaken_session_application::SessionMetadataUpdate::Clear,
    });
    let budget = budget.map(|value| match value {
        Some(value) => awaken_session_application::SessionFieldUpdate::Replace(value),
        None => awaken_session_application::SessionFieldUpdate::Clear,
    });
    let (session, command_revision) = state
        .update_session(
            &id,
            awaken_session_application::SessionUpdateCommand {
                title,
                metadata,
                budget,
                tools: tools.map(|tools| crate::project::session_tool_configuration(&tools)),
                mcp_candidates: mcp_servers.map(|servers| {
                    servers
                        .into_iter()
                        .map(|server| {
                            crate::state::agent_mcp_candidate(
                                server,
                                awaken_session_contract::McpAttachmentOrigin::Session,
                            )
                        })
                        .collect()
                }),
                idempotency_key,
                request_fingerprint,
                if_match,
            },
        )
        .await
        .map_err(error_response)?;
    Ok(versioned_session_response_at_revision(
        session,
        command_revision,
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
) -> Result<Json<PageCursor<SessionThread>>, WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    state
        .list_threads(&id)
        .map(PageCursor::single)
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
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    state
        .list_thread_events(&id, &tid, query.page.as_deref(), query.limit)
        .map(Json)
        .map_err(error_response)
}

/// Parse the SDK's `event_deltas[]` live-preview opt-in. Repeated `event_deltas[]`
/// (or `event_deltas`) values select which buffered events to preview; only
/// `agent.message` and `agent.thinking` are accepted (any other value is a 400,
/// matching the official wire). The returned selection keeps message and thinking
/// independent so opting into one cannot leak the other's preview frames.
#[derive(Debug, Clone, Copy, Default)]
struct PreviewSelection {
    message: bool,
    thinking: bool,
}

impl PreviewSelection {
    const fn any(self) -> bool {
        self.message || self.thinking
    }

    fn accepts(self, event_type: &str) -> bool {
        match event_type {
            "agent.message" => self.message,
            "agent.thinking" => self.thinking,
            _ => false,
        }
    }
}

fn parse_event_deltas(raw: Option<&str>) -> Result<PreviewSelection, WireErr> {
    let mut requested = PreviewSelection::default();
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
                    "agent.message" => requested.message = true,
                    "agent.thinking" => requested.thinking = true,
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

fn parse_event_list_order(raw: Option<&str>) -> Result<SessionListOrder, WireErr> {
    let mut order = None;
    if let Some(query) = raw {
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            if key != "order" {
                continue;
            }
            if order.is_some() {
                return Err(error_response(StateError::Run(RunError::bad_request(
                    "order may be specified once",
                ))));
            }
            order = Some(SessionListOrder::parse(&value)?);
        }
    }
    Ok(order.unwrap_or(SessionListOrder::Asc))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SseTerminalScope {
    /// Session and primary-Thread streams close only at the aggregate boundary.
    Session,
    /// A child-Thread stream closes at that child's idle/terminated boundary.
    ChildThread,
}

/// The terminal committed events that close this exact SSE projection. A child
/// idle is observable on the primary stream but is never the Session terminal.
fn is_terminal(frame: &StreamFrame, scope: SseTerminalScope) -> bool {
    matches!(
        frame,
        StreamFrame::Committed(e)
            if matches!(
                e.type_str(),
                "session.status_idle"
                    | "session.status_terminated"
                    | "session.deleted"
            )
                || (scope == SseTerminalScope::ChildThread
                    && matches!(
                        e.type_str(),
                        "session.thread_status_idle" | "session.thread_status_terminated"
                    ))
    )
}

/// Track whether the newest Run represented in a replay snapshot has reached a
/// terminal event. Usage and telemetry are committed after `status_idle`, so
/// simply asking whether the final snapshot event is terminal leaves a
/// send-then-stream client tailing forever. Conversely, an older idle must not
/// close a stream after a newer input or running marker has started another Run.
fn replay_is_terminal_after(current: bool, event_type: &str, scope: SseTerminalScope) -> bool {
    match event_type {
        "session.status_idle" | "session.status_terminated" | "session.deleted" => true,
        "session.thread_status_idle" | "session.thread_status_terminated"
            if scope == SseTerminalScope::ChildThread =>
        {
            true
        }
        "session.thread_status_running" | "session.thread_status_rescheduled"
            if scope == SseTerminalScope::ChildThread =>
        {
            false
        }
        "session.thread_status_idle"
        | "session.thread_status_terminated"
        | "session.thread_status_running"
        | "session.thread_status_rescheduled" => current,
        "user.message"
        | "user.tool_confirmation"
        | "user.custom_tool_result"
        | "user.tool_result"
        | "user.define_outcome"
        | "user.interrupt"
        | "session.status_running"
        | "session.status_rescheduled" => false,
        _ => current,
    }
}

fn sse_frame(frame: &StreamFrame) -> SseEvent {
    // The SDK dispatches on the SSE `event:` name; the JSON body carries the same
    // `type` plus the fields (committed event, or a stream-only preview).
    SseEvent::default()
        .event(frame.type_str())
        .data(frame.data())
}

fn accept_preview_frame(
    frame: &crate::types::PreviewFrame,
    selection: PreviewSelection,
    accepted_ids: &mut HashSet<String>,
) -> bool {
    match frame {
        crate::types::PreviewFrame::EventStart { event } => {
            selection.accepts(&event.event_type) && accepted_ids.insert(event.id.clone())
        }
        crate::types::PreviewFrame::EventDelta { event_id, .. } => accepted_ids.contains(event_id),
    }
}

/// The live SSE body: the committed snapshot (backfill, deduped against the live
/// tail by id), then the Runtime-owned Thread preview subscription and committed
/// broadcast race until a terminal committed event or the Session sender drops.
/// Preview frames are connection-local and forwarded only when their exact event
/// type was selected; they never re-enter the committed broadcast.
fn live_sse_stream<F>(
    snapshot: Vec<Event>,
    mut rx: broadcast::Receiver<Event>,
    previews: PreviewSelection,
    terminal_scope: SseTerminalScope,
    project: F,
    thread_preview: Option<(
        Box<dyn awaken_session_contract::SessionThreadLiveSubscription>,
        ThreadPreviewProjector,
    )>,
) -> impl Stream<Item = Result<SseEvent, Infallible>>
where
    F: Fn(Event) -> Option<Event> + Send + Sync + 'static,
{
    async_stream::stream! {
        let mut seen: HashSet<String> = HashSet::new();
        let mut preview_event_ids: HashSet<String> = HashSet::new();
        let mut thread_preview = thread_preview;
        let mut backfill_terminal = false;
        for event in snapshot {
            let Some(event) = project(event) else {
                continue;
            };
            seen.insert(event.id.clone());
            let frame = StreamFrame::Committed(event);
            backfill_terminal = replay_is_terminal_after(
                backfill_terminal,
                frame.type_str(),
                terminal_scope,
            );
            yield Ok(sse_frame(&frame));
        }
        // A snapshot that already reached idle/terminated is a completed Run
        // (send-then-stream): deliver the backfill and end, preserving
        // request/response semantics. Otherwise tail the live broadcast.
        if !backfill_terminal {
            loop {
                tokio::select! {
                    biased;
                    live = async {
                        match thread_preview.as_mut() {
                            Some((subscription, _)) => subscription.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        match live {
                            Ok(Some(event)) => {
                                let frames = thread_preview
                                    .as_mut()
                                    .expect("enabled Thread subscription still exists")
                                    .1
                                    .project(event);
                                for frame in frames {
                                    if accept_preview_frame(
                                        &frame,
                                        previews,
                                        &mut preview_event_ids,
                                    ) {
                                        yield Ok(sse_frame(&StreamFrame::Preview(frame)));
                                    }
                                }
                            }
                            Ok(None) | Err(_) => thread_preview = None,
                        }
                    }
                    received = rx.recv() => match received {
                        Ok(event) => {
                            let Some(event) = project(event) else {
                                continue;
                            };
                            // Dedupe the snapshot/live overlap by id; only end on a
                            // committed terminal.
                            if !seen.insert(event.id.clone()) {
                                continue;
                            }
                            if let Some((_, projector)) = thread_preview.as_mut() {
                                if event.type_str() == "agent.message" {
                                    for preview in projector.take_for_committed(&event.id) {
                                        if accept_preview_frame(
                                            &preview,
                                            previews,
                                            &mut preview_event_ids,
                                        ) {
                                            yield Ok(sse_frame(&StreamFrame::Preview(preview)));
                                        }
                                    }
                                } else if matches!(
                                    event.type_str(),
                                    "agent.thread_message_sent"
                                        | "session.thread_status_idle"
                                        | "session.thread_status_terminated"
                                ) {
                                    projector.discard_uncommitted();
                                }
                            }
                            let frame = StreamFrame::Committed(event);
                            let terminal = is_terminal(&frame, terminal_scope);
                            yield Ok(sse_frame(&frame));
                            if terminal {
                                break;
                            }
                        }
                        // Best-effort: a lagging subscriber skips the dropped frames
                        // (the buffered agent.message still arrives); a closed sender ends.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
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
    // the Session stream. Primary and ordinary child Threads consume the same
    // Runtime-owned live observer; Advisor consultations intentionally expose
    // only their documented lifecycle/cross-post wire.
    let previews = parse_event_deltas(raw.as_deref())?;
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    let thread = state.get_thread(&id, &tid).map_err(error_response)?;
    let (snapshot, rx) = state.stream_subscribe(&id).map_err(error_response)?;
    let project_thread = internal_thread_id(&id, &tid);
    let primary = project_thread == id;
    let thread_preview = if previews.any() && !thread.agent.is_advisor() {
        state
            .session_application()
            .subscribe_session_thread_live(&id, &project_thread)
            .await
            .map_err(StateError::Run)
            .map_err(error_response)?
            .map(|subscription| {
                (
                    subscription,
                    ThreadPreviewProjector::new(id.clone(), project_thread.clone()),
                )
            })
    } else {
        None
    };
    let project_session = id.clone();
    let project_state = Arc::clone(&state);
    Ok(Sse::new(live_sse_stream(
        snapshot,
        rx,
        previews,
        if primary {
            SseTerminalScope::Session
        } else {
            SseTerminalScope::ChildThread
        },
        move |event| {
            project_state.project_committed_event_for_thread(
                &project_session,
                &project_thread,
                event,
            )
        },
        thread_preview,
    ))
    .keep_alive(KeepAlive::default()))
}

// -- Resources --

/// Lower the Awaken Resource Manifest wire request into the canonical Session
/// application command. The explicitly namespaced protocol crate owns the HTTP
/// method/path and injects this one handler rather than duplicating the lowering.
pub async fn replace_resource_manifest(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    headers: HeaderMap,
    Json(body): Json<crate::types::resource::ResourceManifestReplaceParams>,
) -> Result<
    (
        HeaderMap,
        Json<crate::types::resource::SessionResourceManifest>,
    ),
    (StatusCode, Json<ErrorResponse>),
> {
    ensure_session_scope(
        state.as_ref(),
        &id,
        workspace.as_ref().map(|scope| &scope.0),
    )
    .await?;
    let idempotency_key = parse_idempotency_key(&headers)?;
    let if_match = parse_if_match(&headers)?;
    let fingerprints = body
        .resources
        .iter()
        .map(crate::types::resource::ResourceInput::idempotency_fingerprint)
        .collect::<Vec<_>>();
    let request_fingerprint = awaken_session_contract::stable_fingerprint(&fingerprints);
    let (manifest, command_revision) = state
        .replace_resource_manifest(&id, body, idempotency_key, if_match, request_fingerprint)
        .await
        .map_err(error_response)?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", command_revision.0))
            .expect("numeric Session revision is a valid ETag"),
    );
    Ok((response_headers, Json(manifest)))
}

const PROFILED_SESSION_REQUEST_FINGERPRINT: &str = "awaken.profiled_session_request_fingerprint";

/// Lower Awaken's strongly typed extension request into the sole profiled
/// Session composer. The handler owns no Session state or realization path.
pub async fn create_profiled_session(
    State(state): State<Arc<ManagedState>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    Json(mut body): Json<awaken_protocol_awaken::ProfiledSessionCreate>,
) -> Result<Json<awaken_protocol_awaken::ProfiledSessionCreated>, (StatusCode, Json<ErrorResponse>)>
{
    let owner_scope = workspace
        .and_then(|scope| scope.0.non_empty().map(str::to_owned))
        .ok_or_else(|| {
            error_response(StateError::Run(RunError::bad_request(
                "profiled Session creation requires a Workspace scope",
            )))
        })?;
    if body.session_id.trim().is_empty()
        || body.agent_id.trim().is_empty()
        || body.source_revision == Some(0)
        || body
            .metadata
            .contains_key(PROFILED_SESSION_REQUEST_FINGERPRINT)
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "profiled Session identity, revision, or reserved metadata is invalid",
        ))));
    }
    let request_fingerprint = awaken_session_contract::stable_fingerprint(&body);
    if let Some(existing) = state
        .replay_session_with_metadata(
            &body.session_id,
            &owner_scope,
            &[(PROFILED_SESSION_REQUEST_FINGERPRINT, &request_fingerprint)],
        )
        .await
        .map_err(error_response)?
    {
        return Ok(Json(awaken_protocol_awaken::ProfiledSessionCreated {
            id: existing.id,
            metadata: existing.metadata,
        }));
    }
    body.metadata.insert(
        PROFILED_SESSION_REQUEST_FINGERPRINT.into(),
        request_fingerprint,
    );
    let mcp_candidates = body
        .mcp_attachments
        .into_iter()
        .map(
            |candidate| awaken_session_application::McpAttachmentCandidate {
                name: candidate.name,
                target: awaken_session_application::McpAttachmentCandidateTarget::Normalized(
                    candidate.target,
                ),
                prompts_as_skills: candidate.prompts_as_skills,
                published_credential: candidate
                    .published_credential
                    .map(|credential| (credential.id, credential.revision)),
                origin: candidate.origin,
            },
        )
        .collect();
    let repositories = body
        .repositories
        .into_iter()
        .enumerate()
        .map(
            |(index, repository)| awaken_session_application::SessionRepositoryResourceInput {
                id: format!("profiled:{}:repository:{index}", body.session_id),
                workspace_id: owner_scope.clone(),
                name: format!("Profiled Session repository {index}"),
                description: "Product-authored profiled Session input".into(),
                remote_url: repository.remote_url,
                authorization_token: None,
                credential: repository.credential,
                mount_path: repository.mount_path,
                initial_branch: repository.initial_branch,
                initial_commit: repository.initial_commit,
            },
        )
        .collect();
    let session = state
        .session_application()
        .create_profiled_session(awaken_session_application::CreateProfiledSessionCommand {
            owner_scope,
            session_id: body.session_id,
            agent_id: body.agent_id,
            source_revision: body.source_revision,
            environment_id: body.environment_id,
            model: None,
            mounts: body.mounts,
            env: body.env,
            prompts: body.prompts,
            mcp_candidates,
            repositories,
            network_restriction: body.network_restriction,
            title: body.title,
            metadata: body.metadata,
            tools: body.tools,
        })
        .await
        .map_err(|error| error_response(StateError::Run(error)))?;
    state
        .ensure_session(&session.session_id)
        .await
        .map_err(error_response)?;
    Ok(Json(awaken_protocol_awaken::ProfiledSessionCreated {
        id: session.session_id,
        metadata: session.metadata,
    }))
}

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
) -> Result<Json<PageCursor<crate::types::resource::SessionResource>>, WireErr> {
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    state
        .list_resources(&id)
        .map(PageCursor::single)
        .map(Json)
        .map_err(error_response)
}

async fn get_resource(
    State(state): State<Arc<ManagedState>>,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<crate::types::resource::SessionResource>, WireErr> {
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
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
    headers: HeaderMap,
    ManagedJson(req): ManagedJson<SendEventsRequest>,
) -> Result<Json<SendEventsResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Anthropic's Messages SDK projects request-grain `user_profile_id` to this
    // header. Reuse the context carrier for Managed events without adding an
    // out-of-contract field to the strict event envelope.
    let data_subject_id = headers
        .get("anthropic-user-profile-id")
        .map(|value| {
            value
                .to_str()
                .ok()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    error_response(StateError::Run(RunError::bad_request(
                        "anthropic-user-profile-id must be non-empty visible ASCII",
                    )))
                })
        })
        .transpose()?;
    let idempotency_key = parse_idempotency_key(&headers)?;
    state
        .send_events_attributed(&id, req, data_subject_id, idempotency_key)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn list_events(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
    Query(query): Query<PageQuery>,
) -> Result<Json<ListEventsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let order = parse_event_list_order(raw.as_deref())?;
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    state
        .list_events(
            &id,
            query.page.as_deref(),
            query.limit,
            order == SessionListOrder::Desc,
        )
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
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    let (snapshot, rx) = state.stream_subscribe(&id).map_err(error_response)?;
    let thread_preview = if previews.any() {
        state
            .session_application()
            .subscribe_session_thread_live(&id, &id)
            .await
            .map_err(StateError::Run)
            .map_err(error_response)?
            .map(|subscription| {
                (
                    subscription,
                    ThreadPreviewProjector::new(id.clone(), id.clone()),
                )
            })
    } else {
        None
    };
    let project_session = id.clone();
    let project_state = Arc::clone(&state);
    Ok(Sse::new(live_sse_stream(
        snapshot,
        rx,
        previews,
        SseTerminalScope::Session,
        move |event| {
            project_state.project_committed_event_for_thread(
                &project_session,
                &project_session,
                event,
            )
        },
        thread_preview,
    ))
    .keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod managed_json_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::{
        SseTerminalScope, enforce_managed_beta, error_response, managed_json_message,
        parse_event_deltas, replay_is_terminal_after,
    };
    use crate::state::{RunError, StateError};

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

    #[test]
    fn temporary_session_dependency_failure_is_retryable() {
        // Cause/effect decision table: R1 image/readiness dependency failure is
        // Unavailable; R2 the Managed adapter returns 503 + api_error so SDK
        // callers can retry the unchanged Session create command.
        let (status, body) = error_response(StateError::Run(RunError::unavailable(
            "Environment image is not ready",
        )));
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE, "R1");
        assert_eq!(body.0.error.kind, "api_error", "R2");
    }

    /// Preview selector cause/effect table: C1 message requested, C2 thinking
    /// requested, C3 neither. E1 accepts only message, E2 accepts only thinking,
    /// E3 disables previews. Rules S1=C1=>E1, S2=C2=>E2,
    /// S3=C1+C2=>E1+E2, S4=C3=>E3. Invalid/count boundaries are covered by the
    /// HTTP streaming suite.
    #[test]
    fn preview_selection_keeps_message_and_thinking_independent() {
        // Causes: the fixtures below establish `preview selection` with the concrete inputs, state,
        // dependencies, and failure triggers used by this case.
        // Effects: the observable result `keeps message and thinking independent` and every
        // asserted state transition or side effect must hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let message = parse_event_deltas(Some("event_deltas[]=agent.message")).unwrap();
        assert!(message.accepts("agent.message"), "S1/E1");
        assert!(!message.accepts("agent.thinking"), "S1/E1");

        let thinking = parse_event_deltas(Some("event_deltas[]=agent.thinking")).unwrap();
        assert!(!thinking.accepts("agent.message"), "S2/E2");
        assert!(thinking.accepts("agent.thinking"), "S2/E2");

        let both = parse_event_deltas(Some(
            "event_deltas[]=agent.message&event_deltas[]=agent.thinking",
        ))
        .unwrap();
        assert!(
            both.accepts("agent.message") && both.accepts("agent.thinking"),
            "S3"
        );
        assert!(!parse_event_deltas(None).unwrap().any(), "S4/E3");
    }

    #[tokio::test]
    async fn current_and_legacy_tunnel_betas_are_route_scoped() {
        let app = Router::new()
            .route("/v1/tunnels", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/v1/organizations/tunnels",
                get(|| async { StatusCode::NO_CONTENT }),
            )
            .layer(axum::middleware::from_fn(enforce_managed_beta));
        let status = |path: &'static str, beta: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::get(path)
                        .header("anthropic-beta", beta)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };
        assert_eq!(
            status("/v1/tunnels", crate::TUNNELS_BETA).await,
            StatusCode::NO_CONTENT,
            "current/current"
        );
        assert_eq!(
            status("/v1/tunnels", crate::LEGACY_TUNNELS_BETA).await,
            StatusCode::BAD_REQUEST,
            "legacy beta cannot select the current route"
        );
        assert_eq!(
            status("/v1/organizations/tunnels", crate::LEGACY_TUNNELS_BETA,).await,
            StatusCode::NO_CONTENT,
            "legacy/legacy"
        );
        assert_eq!(
            status("/v1/organizations/tunnels", crate::TUNNELS_BETA).await,
            StatusCode::BAD_REQUEST,
            "current beta cannot silently change legacy auth semantics"
        );
    }

    #[tokio::test]
    async fn current_and_legacy_memory_betas_select_one_canonical_contract() {
        // SDK 0.105 sends the Managed beta and SDK 0.117 sends the Memory beta.
        // Either one alone selects the same handler; neither or both is
        // ambiguous and fails before the handler can observe the request.
        let app = Router::new()
            .route(
                "/v1/memory_stores",
                get(|| async { StatusCode::NO_CONTENT }),
            )
            .layer(axum::middleware::from_fn(enforce_managed_beta));
        let status = |beta: Option<&'static str>| {
            let app = app.clone();
            async move {
                let mut request = Request::get("/v1/memory_stores");
                if let Some(beta) = beta {
                    request = request.header("anthropic-beta", beta);
                }
                app.oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        assert_eq!(
            status(Some(crate::MANAGED_BETA)).await,
            StatusCode::NO_CONTENT,
            "SDK 0.105 legacy selector"
        );
        assert_eq!(
            status(Some(super::MEMORY_BETA)).await,
            StatusCode::NO_CONTENT,
            "SDK 0.117 current selector"
        );
        assert_eq!(status(None).await, StatusCode::BAD_REQUEST, "missing");
        assert_eq!(
            status(Some("future-memory-beta")).await,
            StatusCode::BAD_REQUEST,
            "unknown only"
        );
        assert_eq!(
            status(Some("managed-agents-2026-04-01,agent-memory-2026-07-22")).await,
            StatusCode::BAD_REQUEST,
            "the official headers remain mutually exclusive"
        );
    }

    #[tokio::test]
    async fn workspace_addressing_cannot_bypass_endpoint_beta_admission() {
        // Same endpoint and header causes must have the same effect before and
        // after the public Workspace path prefix. This is the complete flat ×
        // scoped decision table for ordinary Managed and Memory selectors.
        let app = Router::new()
            .route("/v1/sessions", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/v1/workspaces/default/sessions",
                get(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/memory_stores",
                get(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/workspaces/default/memory_stores",
                get(|| async { StatusCode::NO_CONTENT }),
            )
            .layer(axum::middleware::from_fn(enforce_managed_beta));
        let status = |path: &'static str, beta: Option<&'static str>| {
            let app = app.clone();
            async move {
                let mut request = Request::get(path);
                if let Some(beta) = beta {
                    request = request.header("anthropic-beta", beta);
                }
                app.oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        for path in ["/v1/sessions", "/v1/workspaces/default/sessions"] {
            assert_eq!(status(path, None).await, StatusCode::BAD_REQUEST);
            assert_eq!(
                status(path, Some(crate::MANAGED_BETA)).await,
                StatusCode::NO_CONTENT
            );
        }
        for path in ["/v1/memory_stores", "/v1/workspaces/default/memory_stores"] {
            assert_eq!(status(path, None).await, StatusCode::BAD_REQUEST);
            assert_eq!(
                status(path, Some(super::MEMORY_BETA)).await,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                status(path, Some(crate::MANAGED_BETA)).await,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                status(
                    path,
                    Some("managed-agents-2026-04-01,agent-memory-2026-07-22")
                )
                .await,
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn replay_terminal_state_survives_trailing_usage_and_telemetry() {
        // Causes: the fixtures below establish `replay terminal state survives trailing usage and
        // telemetry` with the concrete inputs, state, dependencies, and failure triggers used by
        // this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Coverage rationale: `replay terminal state survives trailing usage and telemetry` is one
        // independent branch selecting `all output, state, side-effect, error, and terminal
        // assertions below hold together`; a multi-row decision table is not applicable, and
        // sibling tests own alternate causes.
        let mut terminal = false;
        for event_type in [
            "user.message",
            "session.status_running",
            "span.model_request_start",
            "span.model_request_end",
            "agent.message",
            "session.status_idle",
            "session.usage",
        ] {
            terminal = replay_is_terminal_after(terminal, event_type, SseTerminalScope::Session);
        }
        assert!(
            terminal,
            "trailing observational events cannot reopen a Run"
        );
    }

    #[test]
    fn replay_terminal_state_is_reset_by_every_resumption_input() {
        // Causes: the fixtures below establish `replay terminal state` with the concrete inputs,
        // state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `is reset by every resumption input` and every asserted
        // state transition or side effect must hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Coverage rationale: `replay terminal state` is one independent branch selecting `is reset
        // by every resumption input`; a multi-row decision table is not applicable, and sibling
        // tests own alternate causes.
        for event_type in [
            "user.message",
            "user.tool_confirmation",
            "user.custom_tool_result",
            "user.tool_result",
            "user.define_outcome",
            "user.interrupt",
            "session.status_running",
            "session.status_rescheduled",
        ] {
            assert!(
                !replay_is_terminal_after(true, event_type, SseTerminalScope::Session),
                "{event_type} must reopen the replay tail"
            );
        }
    }

    #[test]
    fn child_terminal_events_close_only_the_matching_child_stream() {
        // Causes: the fixtures below establish `child terminal events close only the matching child
        // stream` with the concrete inputs, state, dependencies, and failure triggers used by this
        // case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 a child emits idle/terminated; C2 the projected
        // stream is aggregate Session/primary; C3 it is that child Thread.
        // Effects: E1 C1+C2 leaves aggregate tailing; E2 C1+C3 closes the child.
        // Decision table: T1=C1+C2=>E1; T2=C1+C3=>E2. This prevents one child
        // from truncating a primary stream while another activity remains active.
        for event_type in [
            "session.thread_status_idle",
            "session.thread_status_terminated",
        ] {
            assert!(
                !replay_is_terminal_after(false, event_type, SseTerminalScope::Session,),
                "T1/{event_type}"
            );
            assert!(
                replay_is_terminal_after(false, event_type, SseTerminalScope::ChildThread,),
                "T2/{event_type}"
            );
        }
        for event_type in [
            "session.thread_status_running",
            "session.thread_status_rescheduled",
        ] {
            assert!(
                replay_is_terminal_after(true, event_type, SseTerminalScope::Session),
                "T1 aggregate state ignores {event_type}"
            );
            assert!(
                !replay_is_terminal_after(true, event_type, SseTerminalScope::ChildThread,),
                "T2 child state reopens on {event_type}"
            );
        }
        assert!(
            replay_is_terminal_after(false, "session.status_idle", SseTerminalScope::Session,),
            "the aggregate terminal still closes primary"
        );
    }
}
