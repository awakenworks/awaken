//! The axum router: four Managed Agents routes over [`ManagedState`].
//!
//! Handlers only decode DTOs, call the state, and encode responses; no runtime
//! or protocol logic lives here. Errors map to HTTP status; live stream output is
//! a projection of committed events (SSE replay).

use std::convert::Infallible;
use std::sync::Arc;

use std::cmp::Ordering;
use std::collections::HashSet;

use awaken_tenancy::WorkspaceScope;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_stream::Stream;

use crate::state::{ManagedState, RunError, RunErrorKind, StateError};
use crate::types::{
    DeletedSession, ErrorResponse, ListEventsResponse, Page, PageQuery, SendEventsRequest,
    SendEventsResponse, Session, SessionCreateParams, SessionThread,
};
use crate::types::{Event, StreamFrame};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionCursorDirection {
    After,
    Before,
}

#[derive(Debug, Clone)]
struct SessionCursor {
    order: SessionListOrder,
    direction: SessionCursorDirection,
    created_at: String,
    id: String,
}

impl SessionCursor {
    fn for_session(
        order: SessionListOrder,
        direction: SessionCursorDirection,
        session: &Session,
    ) -> Self {
        Self {
            order,
            direction,
            created_at: session.created_at.clone(),
            id: session.id.clone(),
        }
    }

    fn encode(&self) -> String {
        let direction = match self.direction {
            SessionCursorDirection::After => "after",
            SessionCursorDirection::Before => "before",
        };
        let plain = format!(
            "v1|{}|{direction}|{}|{}",
            self.order.as_str(),
            self.created_at,
            self.id
        );
        plain
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn decode(value: &str) -> Result<Self, WireErr> {
        if !value.len().is_multiple_of(2) || value.is_empty() {
            return Err(invalid_session_cursor());
        }
        let bytes = (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid_session_cursor())?;
        let plain = String::from_utf8(bytes).map_err(|_| invalid_session_cursor())?;
        let mut fields = plain.split('|');
        if fields.next() != Some("v1") {
            return Err(invalid_session_cursor());
        }
        let order = fields
            .next()
            .ok_or_else(invalid_session_cursor)
            .and_then(SessionListOrder::parse)?;
        let direction = match fields.next() {
            Some("after") => SessionCursorDirection::After,
            Some("before") => SessionCursorDirection::Before,
            _ => return Err(invalid_session_cursor()),
        };
        let created_at = fields
            .next()
            .ok_or_else(invalid_session_cursor)?
            .to_string();
        let id = fields
            .next()
            .ok_or_else(invalid_session_cursor)?
            .to_string();
        if fields.next().is_some()
            || id.is_empty()
            || chrono::DateTime::parse_from_rfc3339(&created_at).is_err()
        {
            return Err(invalid_session_cursor());
        }
        Ok(Self {
            order,
            direction,
            created_at,
            id,
        })
    }
}

fn invalid_session_cursor() -> WireErr {
    error_response(StateError::Run(RunError::bad_request(
        "invalid session pagination cursor",
    )))
}

#[derive(Debug)]
struct SessionListParams {
    limit: usize,
    page: Option<SessionCursor>,
    order: SessionListOrder,
    agent_id: Option<String>,
    agent_version: Option<u64>,
    created_gt: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_gte: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_lt: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_lte: Option<chrono::DateTime<chrono::FixedOffset>>,
    deployment_id: Option<String>,
    include_archived: bool,
    memory_store_id: Option<String>,
    statuses: HashSet<String>,
}

fn parse_session_list(raw: Option<&str>) -> Result<SessionListParams, WireErr> {
    let mut params = SessionListParams {
        limit: awaken_agent_contract::page::DEFAULT_PAGE_LIMIT,
        page: None,
        order: SessionListOrder::Desc,
        agent_id: None,
        agent_version: None,
        created_gt: None,
        created_gte: None,
        created_lt: None,
        created_lte: None,
        deployment_id: None,
        include_archived: false,
        memory_store_id: None,
        statuses: HashSet::new(),
    };
    let pairs = form_urlencoded::parse(raw.unwrap_or_default().as_bytes());
    let mut encoded_page = None;
    for (key, value) in pairs {
        match key.as_ref() {
            "limit" => {
                params.limit = value.parse::<usize>().map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(
                        "limit must be a positive integer",
                    )))
                })?;
                if params.limit == 0 {
                    return Err(error_response(StateError::Run(RunError::bad_request(
                        "limit must be a positive integer",
                    ))));
                }
                params.limit = params
                    .limit
                    .min(awaken_agent_contract::page::MAX_PAGE_LIMIT);
            }
            "page" => encoded_page = Some(value.into_owned()),
            "order" => params.order = SessionListOrder::parse(&value)?,
            "agent_id" => params.agent_id = Some(value.into_owned()),
            "agent_version" => {
                params.agent_version = Some(value.parse::<u64>().map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(
                        "agent_version must be a positive integer",
                    )))
                })?);
            }
            "created_at[gt]" => params.created_gt = Some(parse_list_time(&value)?),
            "created_at[gte]" => params.created_gte = Some(parse_list_time(&value)?),
            "created_at[lt]" => params.created_lt = Some(parse_list_time(&value)?),
            "created_at[lte]" => params.created_lte = Some(parse_list_time(&value)?),
            "deployment_id" => params.deployment_id = Some(value.into_owned()),
            "include_archived" => {
                params.include_archived = value.parse::<bool>().map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(
                        "include_archived must be a boolean",
                    )))
                })?;
            }
            "memory_store_id" => params.memory_store_id = Some(value.into_owned()),
            "statuses" | "statuses[]" => match value.as_ref() {
                "rescheduling" | "running" | "idle" | "terminated" => {
                    params.statuses.insert(value.into_owned());
                }
                _ => {
                    return Err(error_response(StateError::Run(RunError::bad_request(
                        "statuses contains an unsupported Session status",
                    ))));
                }
            },
            _ => {}
        }
    }
    params.page = encoded_page
        .as_deref()
        .map(SessionCursor::decode)
        .transpose()?;
    if params
        .page
        .as_ref()
        .is_some_and(|cursor| cursor.order != params.order)
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "session pagination cursor order does not match the requested order",
        ))));
    }
    Ok(params)
}

fn parse_list_time(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, WireErr> {
    chrono::DateTime::parse_from_rfc3339(value).map_err(|_| {
        error_response(StateError::Run(RunError::bad_request(
            "created_at filters must be RFC 3339 timestamps",
        )))
    })
}

#[derive(Debug, Serialize)]
struct SessionListPage {
    data: Vec<Session>,
    next_page: Option<String>,
    prev_page: Option<String>,
}

fn session_list_page(
    mut data: Vec<Session>,
    params: &SessionListParams,
) -> Result<SessionListPage, WireErr> {
    data.retain(|session| session_matches(session, params));
    data.sort_by(|left, right| session_order(left, right, params.order));
    let (start, end) = match &params.page {
        None => (0, params.limit.min(data.len())),
        Some(cursor) => match cursor.direction {
            SessionCursorDirection::After => {
                let start = data.partition_point(|session| {
                    session_to_cursor_order(session, cursor, params.order) != Ordering::Greater
                });
                (start, start.saturating_add(params.limit).min(data.len()))
            }
            SessionCursorDirection::Before => {
                let end = data.partition_point(|session| {
                    session_to_cursor_order(session, cursor, params.order) == Ordering::Less
                });
                (end.saturating_sub(params.limit), end)
            }
        },
    };
    let page = data[start..end].to_vec();
    let prev_page = (start > 0).then(|| page.first()).flatten().map(|first| {
        SessionCursor::for_session(params.order, SessionCursorDirection::Before, first).encode()
    });
    let next_page = (end < data.len())
        .then(|| page.last())
        .flatten()
        .map(|last| {
            SessionCursor::for_session(params.order, SessionCursorDirection::After, last).encode()
        });
    Ok(SessionListPage {
        data: page,
        next_page,
        prev_page,
    })
}

fn session_order(left: &Session, right: &Session, order: SessionListOrder) -> Ordering {
    let result = left
        .created_at
        .cmp(&right.created_at)
        .then_with(|| left.id.cmp(&right.id));
    match order {
        SessionListOrder::Asc => result,
        SessionListOrder::Desc => result.reverse(),
    }
}

fn session_to_cursor_order(
    session: &Session,
    cursor: &SessionCursor,
    order: SessionListOrder,
) -> Ordering {
    let result = session
        .created_at
        .cmp(&cursor.created_at)
        .then_with(|| session.id.cmp(&cursor.id));
    match order {
        SessionListOrder::Asc => result,
        SessionListOrder::Desc => result.reverse(),
    }
}

fn session_matches(session: &Session, params: &SessionListParams) -> bool {
    if !params.include_archived && session.archived_at.is_some() {
        return false;
    }
    if params
        .agent_id
        .as_ref()
        .is_some_and(|id| session.agent.id != *id)
    {
        return false;
    }
    if params.agent_id.is_some()
        && params
            .agent_version
            .is_some_and(|version| session.agent.version != version)
    {
        return false;
    }
    if params
        .deployment_id
        .as_ref()
        .is_some_and(|id| session.deployment_id.as_ref() != Some(id))
    {
        return false;
    }
    if params.memory_store_id.as_ref().is_some_and(|id| {
        !session.resources.iter().any(|resource| {
            matches!(
                resource,
                crate::types::resource::SessionResource::MemoryStore {
                    memory_store_id,
                    ..
                } if memory_store_id == id
            )
        })
    }) {
        return false;
    }
    if !params.statuses.is_empty() && !params.statuses.contains(session.status.as_str()) {
        return false;
    }
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&session.created_at) else {
        return false;
    };
    params
        .created_gt
        .as_ref()
        .is_none_or(|bound| created > *bound)
        && params
            .created_gte
            .as_ref()
            .is_none_or(|bound| created >= *bound)
        && params
            .created_lt
            .as_ref()
            .is_none_or(|bound| created < *bound)
        && params
            .created_lte
            .as_ref()
            .is_none_or(|bound| created <= *bound)
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

/// The endpoint-specific beta required by the standalone Skills resource API.
pub const SKILLS_BETA: &str = "skills-2025-10-02";
/// The endpoint-specific beta that replaces the Managed beta on Memory APIs.
pub const MEMORY_BETA: &str = "agent-memory-2026-07-22";

fn has_beta(req: &Request, expected: &str) -> bool {
    req.headers()
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|beta| beta.trim() == expected)
}

/// Axum middleware enforcing the `anthropic-beta: managed-agents-2026-04-01` opt-in
/// on every ordinary Managed Agents endpoint. Applied by each executable
/// process startup, NOT baked into [`router`], so router-level tests remain focused
/// on domain behavior. Memory and Skills are gated here with their exclusive
/// endpoint-specific betas; User Profiles and Files remain with their family gates.
pub async fn enforce_managed_beta(
    req: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();
    let is_family = |family: &str| path == family || path.starts_with(&format!("{family}/"));
    if is_family("/v1/memory_stores") {
        let has_memory = has_beta(&req, MEMORY_BETA);
        let has_managed = has_beta(&req, crate::MANAGED_BETA);
        if !has_memory || has_managed {
            let message = if has_memory && has_managed {
                format!(
                    "the {MEMORY_BETA} beta replaces {managed} on memory store endpoints; do not send both",
                    managed = crate::MANAGED_BETA,
                )
            } else {
                format!(
                    "the {MEMORY_BETA} beta is required: send the `anthropic-beta: {MEMORY_BETA}` header"
                )
            };
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("invalid_request_error", message)),
            )
                .into_response();
        }
    }
    if is_family("/v1/skills") && !has_beta(&req, SKILLS_BETA) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {SKILLS_BETA} beta is required: send the `anthropic-beta: {SKILLS_BETA}` header"
                ),
            )),
        )
            .into_response();
    }
    if is_family("/v1/dreams") && !has_beta(&req, super::dreams::DREAMING_BETA) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = super::dreams::DREAMING_BETA,
                ),
            )),
        )
            .into_response();
    }
    if is_family("/v1/user_profiles") && !has_beta(&req, crate::USER_PROFILES_BETA) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = crate::USER_PROFILES_BETA,
                ),
            )),
        )
            .into_response();
    }
    if is_family("/v1/tunnels") && !has_beta(&req, crate::TUNNELS_BETA) {
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
    let is_managed = [
        "/v1/sessions",
        "/v1/agents",
        "/v1/environments",
        "/v1/deployments",
        "/v1/deployment_runs",
        "/v1/vaults",
        "/v1/dreams",
    ]
    .into_iter()
    .any(is_family);
    if is_managed && !has_beta(&req, crate::MANAGED_BETA) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!(
                    "the {beta} beta is required: send the `anthropic-beta: {beta}` header",
                    beta = crate::MANAGED_BETA,
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
    // Session preparation (MCP provisioning, ADR-0043 Phase 3) can fail; map the
    // RunError to the envelope exactly like a turn's failure, so a failed create
    // is loud rather than a half-provisioned session.
    let workspace_id = workspace.map(|w| w.0.0.clone());
    let idempotency_key = parse_idempotency_key(&headers)?;
    let session = match idempotency_key.as_deref() {
        Some(_) if !req.initial_events.is_empty() => {
            return Err(error_response(StateError::Run(RunError::bad_request(
                "Idempotency-Key is not supported with initial_events",
            ))));
        }
        Some(key) => {
            state
                .create_session_with_initial_events_idempotent(req, workspace_id.clone(), key)
                .await
        }
        None => {
            state
                .create_session_with_initial_events(req, workspace_id.clone())
                .await
        }
    }
    .map_err(error_response)?;
    let operation_id = idempotency_key.as_deref().map(|key| {
        awaken_session_contract::stable_fingerprint(&(
            "managed-session-create-operation",
            workspace_id
                .as_deref()
                .unwrap_or(crate::state::DEFAULT_SCOPE),
            key,
        ))
    });
    versioned_session_response(&state, session, operation_id).await
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
    RawQuery(raw): RawQuery,
) -> Result<Json<SessionListPage>, WireErr> {
    let scope = workspace
        .map(|w| w.0.0)
        .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
    let data = state.list_sessions_scoped(&scope);
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
    let operation_id = idempotency_key
        .as_deref()
        .map(|key| ManagedState::update_operation_id(&id, key));
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
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
    state.get_thread(&id, &tid).map_err(error_response)?;
    let (snapshot, rx) = state.stream_subscribe(&id).map_err(error_response)?;
    let primary = tid == format!("{id}:primary");
    let project_session = id.clone();
    let project_thread = tid.clone();
    let project_state = Arc::clone(&state);
    Ok(Sse::new(live_sse_stream(
        snapshot,
        rx,
        previews,
        move |frame| match frame {
            StreamFrame::Committed(event) => {
                let owner = project_state.event_thread_owner(&project_session, &event.id);
                ManagedState::project_event_for_thread(
                    &project_session,
                    &project_thread,
                    event,
                    owner.as_deref(),
                )
                .map(StreamFrame::Committed)
            }
            preview @ StreamFrame::Preview(_) if primary => Some(preview),
            StreamFrame::Preview(_) => None,
        },
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
    let operation_id = idempotency_key.as_deref().map(|key| {
        awaken_session_application::SessionApplication::resource_manifest_operation_id(&id, key)
    });
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
    if let Some(operation_id) = operation_id {
        response_headers.insert(
            "x-awaken-operation-id",
            HeaderValue::from_str(&operation_id).expect("fingerprint is a valid header value"),
        );
    }
    Ok((response_headers, Json(manifest)))
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
) -> Result<Json<Page<crate::types::resource::SessionResource>>, WireErr> {
    state
        .refresh_committed_events(&id)
        .await
        .map_err(error_response)?;
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
    Ok(Sse::new(live_sse_stream(snapshot, rx, previews, Some)).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod managed_json_tests {
    use super::{error_response, managed_json_message};
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
}
