// @formal entrypoint session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
// @formal entrypoint session_event_batch.exact_retry_replays_and_changed_retry_conflicts
//! The axum router: four Managed Agents routes over [`ManagedState`].
//!
//! Handlers only decode DTOs, call the state, and encode responses; no runtime
//! or protocol logic lives here. Errors map to HTTP status; live stream output is
//! a projection of committed events (SSE replay).

use awaken_time::WallClockExt as _;
use std::sync::Arc;

use std::collections::HashSet;

use awaken_runtime_contract::runs::scope::WorkspaceScope;
use axum::extract::{Path, RawQuery, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::broadcast;
use tokio_stream::Stream;

use crate::common::api_version::{ApiWire, ManagedApiVersion};
use crate::common::headers::{
    DREAMING_BETA, MANAGED_BETA, MEMORY_BETA, ManagedCapability, USER_PROFILES_BETA_LATEST,
    has_capability,
};
use crate::common::scope::RequiredWorkspaceScope;
use crate::preview::ThreadPreviewProjector;
use crate::routes::{ManagedJson, ManagedQuery};
use crate::state::{
    CommittedEventDelivery, ManagedRequestState, ManagedState, ProjectionObservationDecision,
    StateError, internal_thread_id,
};
use crate::types::{
    DeletedSession, ErrorResponse, ListEventsResponse, PageCursor, PageQuery, SendEventsRequest,
    SendEventsResponse, Session, SessionCreateParams, SessionThread,
};
use crate::types::{Event, StreamFrame};
use awaken_runtime_contract::runs::{RunError, RunErrorKind};

mod content_capture;
mod event_admission;
mod execution_evidence;
pub(crate) mod pagination;
mod profiled_release;
mod profiled_run;
mod stream;
#[cfg(test)]
use stream::replay_is_terminal_after;
use stream::{LiveSseSource, SseTerminalScope, live_sse_stream, private_stream_deadline};

pub use content_capture::get_session_content_capture;
pub use event_admission::close_event_admission;
pub use execution_evidence::get_session_execution_evidence;
use pagination::{parse_session_list, session_list_page};

const SESSION_TIME_VERSION: &str = "x-awaken-session-time-version";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryTimeVersion {
    V1,
    V2,
}

fn history_time_version(headers: &HeaderMap) -> Result<HistoryTimeVersion, WireErr> {
    let mut values = headers.get_all(SESSION_TIME_VERSION).iter();
    match (values.next(), values.next()) {
        (None, None) => Ok(HistoryTimeVersion::V1),
        (Some(value), None) if value.as_bytes() == b"2" => Ok(HistoryTimeVersion::V2),
        _ => Err(error_response(StateError::Run(RunError::bad_request(
            "x-awaken-session-time-version requires exactly one value: 2",
        )))),
    }
}

fn require_v1_stream_time(headers: &HeaderMap) -> Result<(), WireErr> {
    if history_time_version(headers)? == HistoryTimeVersion::V2 {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "history-time version 2 is unavailable for SSE streams",
        ))));
    }
    Ok(())
}

fn reject_unsupported_thread_time_filter(raw: Option<&str>) -> Result<(), WireErr> {
    if raw
        .into_iter()
        .flat_map(|query| form_urlencoded::parse(query.as_bytes()))
        .any(|(key, _)| {
            ["created_at", "updated_at", "archived_at"]
                .iter()
                .any(|field| key.starts_with(field))
        })
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "Thread list does not support time predicates",
        ))));
    }
    Ok(())
}

fn thread_time_v2(
    thread: &SessionThread,
    time: crate::state::SessionThreadTimeEvidence,
    api_version: ManagedApiVersion,
) -> serde_json::Value {
    crate::types::session::history_time_v2(
        &api_version.wrap(thread),
        &[
            ("created_at", time.created_at_unix_ms),
            ("updated_at", time.updated_at_unix_ms),
            ("archived_at", time.archived_at_unix_ms),
        ],
    )
}

fn session_time_v2(
    session: &Session,
    time: crate::state::SessionRootTimeEvidence,
    api_version: ManagedApiVersion,
) -> serde_json::Value {
    crate::types::session::history_time_v2(
        &api_version.wrap(session),
        &[
            ("created_at", time.created_at_unix_ms),
            ("updated_at", time.updated_at_unix_ms),
            ("archived_at", time.archived_at_unix_ms),
        ],
    )
}
pub use profiled_run::{
    get_profiled_session_run_events, resume_profiled_session_run, submit_profiled_session_run,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionListOrder {
    Asc,
    Desc,
}

impl SessionListOrder {
    pub(crate) fn parse(value: &str) -> Result<Self, WireErr> {
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
/// the edge-stamped [`WorkspaceScope`] and an unstamped request is refused with
/// the same 404, because an ownership comparison against a compiled-in tenant is
/// not an ownership check. The single-tenant surface is stamped by the
/// composition root, never by this guard.
pub(crate) async fn session_scope_guard(
    ManagedRequestState(state): ManagedRequestState,
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
    let Some(request_scope) = workspace.and_then(WorkspaceScope::non_empty) else {
        return Err(session_not_found(id));
    };
    match state.resolve_owner(id).await {
        Ok(Some(owner)) if owner != request_scope => Err(session_not_found(id)),
        Err(error) => Err(error_response(error)),
        Ok(Some(_)) => crate::state::authorize_session(
            state,
            request_scope,
            &awaken_session_types::SessionId::from(id),
        )
        .await
        .map(|_| ())
        .map_err(|error| match error {
            awaken_session_application::SessionAccessError::NotFound => session_not_found(id),
            error => error_response(StateError::Run(error.into())),
        }),
        Ok(None) if state.private_binding.is_some() => Err(session_not_found(id)),
        Ok(None) => Ok(()),
    }
}

/// One envelope for a genuine miss, a cross-tenant address, and an unstamped
/// request, so neither existence nor the tenancy wiring is disclosed.
fn session_not_found(id: &str) -> WireErr {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "not_found_error",
            format!("session `{id}` not found"),
        )),
    )
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
    let code = match &err {
        StateError::Run(error) => Some(error.code.clone()),
        _ => None,
    };
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
            RunErrorKind::Conflict => (StatusCode::CONFLICT, "invalid_request_error", e.message),
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
    let mut response = ErrorResponse::new(kind, message);
    response.error.code = code;
    (status, Json(response))
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
    if let Err(error) = ManagedApiVersion::from_headers(req.headers()) {
        return error.into_response();
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
        && !has_capability(req.headers(), ManagedCapability::UserProfilesExternal)
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
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    headers: HeaderMap,
    ManagedJson(req): ManagedJson<SessionCreateParams>,
) -> Result<(StatusCode, HeaderMap, Json<ApiWire<Session>>), (StatusCode, Json<ErrorResponse>)> {
    req.validate_public()
        .map_err(|message| error_response(StateError::Run(RunError::bad_request(message))))?;
    // Session preparation (MCP provisioning, ADR-0043 Phase 3) can fail; map the
    // RunError to the envelope exactly like a Run failure, so a failed create
    // is loud rather than a half-provisioned session.
    let idempotency_key = parse_idempotency_key(&headers)?;
    let session = match idempotency_key.as_deref() {
        Some(key) => Box::pin(state.create_session_idempotent(req, &workspace_id, key)).await,
        None => Box::pin(state.create_session(req, &workspace_id)).await,
    }
    .map_err(error_response)?;
    let (response_headers, body) = versioned_session_response(&state, &session.id, api_version)?;
    Ok((StatusCode::OK, response_headers, body))
}

pub(crate) fn parse_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, WireErr> {
    crate::parse_idempotency_key_header(headers)
        .map_err(|message| error_response(StateError::Run(RunError::bad_request(message))))
}

async fn retrieve_session(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let version = history_time_version(&headers)?;
    if let Err(error) = state
        .refresh_committed_events_for_observation(&id, None)
        .await
        && !error.is_projection_recovery_required()
    {
        return Err(error_response(error));
    }
    // The refresh reads and projects the durable Session root before Runtime
    // pending-tool detail. If only that detail is unreconstructable, keep the
    // root-owned aggregate status readable: Events GET remains unhealthy, while
    // a terminal root still disables and a known nonterminal root can authorize
    // only the protocol's existing pure-Interrupt recovery path.
    match version {
        HistoryTimeVersion::V1 => {
            versioned_session_response(&state, &id, api_version).map(IntoResponse::into_response)
        }
        HistoryTimeVersion::V2 => {
            let (session, revision, time) = state
                .get_session_with_revision_and_time(&id)
                .map_err(error_response)?;
            let (headers, _) =
                versioned_session_response_at_revision(session.clone(), revision, api_version);
            let value = session_time_v2(&session, time, api_version);
            Ok((headers, Json(value)).into_response())
        }
    }
}

fn versioned_session_response(
    state: &ManagedState,
    session_id: &str,
    api_version: ManagedApiVersion,
) -> Result<(HeaderMap, Json<ApiWire<Session>>), WireErr> {
    let (session, revision) = state
        .get_session_with_revision(session_id)
        .map_err(error_response)?;
    Ok(versioned_session_response_at_revision(
        session,
        revision,
        api_version,
    ))
}

fn versioned_session_response_at_revision(
    session: Session,
    revision: awaken_session_types::SessionRevision,
    api_version: ManagedApiVersion,
) -> (HeaderMap, Json<ApiWire<Session>>) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", revision.0))
            .expect("numeric Session revision is a valid ETag"),
    );
    (headers, Json(api_version.wrap(session)))
}

type WireErr = (StatusCode, Json<ErrorResponse>);

fn session_projection_error(
    error: awaken_session_application::SessionProjectionRecoveryError,
) -> WireErr {
    use awaken_session_application::SessionProjectionRecoveryError as Error;

    match error {
        Error::NotFound => error_response(StateError::NotFound),
        Error::Rejected(error) => error_response(StateError::Run(error)),
        Error::Unavailable(message) => {
            error_response(StateError::Run(RunError::unavailable(message)))
        }
    }
}

/// `GET /v1/sessions` — a cursor page of the request scope's sessions (ADR-0051:
/// tenancy-fenced, so a workspace never lists another's).
async fn list_sessions(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Response, WireErr> {
    let version = history_time_version(&headers)?;
    let params = parse_session_list(raw.as_deref())?;
    match version {
        HistoryTimeVersion::V1 => {
            let data = state
                .list_sessions_scoped_durable(&scope)
                .await
                .map_err(error_response)?;
            Ok(Json(api_version.wrap(session_list_page(data, &params)?)).into_response())
        }
        HistoryTimeVersion::V2 => {
            let rows = state
                .list_sessions_scoped_durable_with_time(&scope)
                .await
                .map_err(error_response)?;
            pagination::ensure_v2_time_filter_answerable(&rows, &params)?;
            let times = rows
                .iter()
                .map(|(session, time)| (session.id.clone(), *time))
                .collect();
            let data = rows.into_iter().map(|(session, _)| session).collect();
            let page = session_list_page(data, &params)?;
            Ok(Json(page.history_time_v2(&times, api_version)).into_response())
        }
    }
}

/// `POST /v1/sessions/:id` — update `title` (null clears) + patch `metadata`.
async fn update_session(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    headers: HeaderMap,
    ManagedJson(body): ManagedJson<crate::types::SessionUpdateParams>,
) -> Result<(HeaderMap, Json<ApiWire<Session>>), WireErr> {
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
            awaken_agent_contract::stable_fingerprint(&(&title, &metadata, &tools, &mcp_servers))
        }
        Some(_) => awaken_agent_contract::stable_fingerprint(&(
            &title,
            &metadata,
            &tools,
            &mcp_servers,
            &budget,
        )),
    };
    let idempotency_key = parse_idempotency_key(&headers)?;
    let if_match =
        super::parse_if_match(&headers, "Session")?.map(awaken_session_types::SessionRevision);
    let title = title.map(|value| match value {
        Some(value) => awaken_session_contract::SessionFieldUpdate::Replace(value),
        None => awaken_session_contract::SessionFieldUpdate::Clear,
    });
    let metadata = metadata.map(|value| match value {
        Some(value) => awaken_session_application::SessionMetadataUpdate::Patch(value),
        None => awaken_session_application::SessionMetadataUpdate::Clear,
    });
    let budget = budget.map(|value| match value {
        Some(value) => awaken_session_contract::SessionFieldUpdate::Replace(value),
        None => awaken_session_contract::SessionFieldUpdate::Clear,
    });
    let (session, response_revision) = state
        .update_session(
            &id,
            awaken_session_application::SessionUpdateCommand {
                title,
                metadata,
                budget,
                tools: tools
                    .map(|tools| crate::project::session_tool_configuration(&tools))
                    .transpose()
                    .map_err(|message| {
                        error_response(StateError::Run(RunError::bad_request(message)))
                    })?,
                mcp_update: mcp_servers.map(|servers| {
                    awaken_session_application::SessionMcpUpdate::PublicReplacement(
                        servers
                            .into_iter()
                            .map(|server| {
                                crate::state::agent_mcp_candidate(
                                    server,
                                    crate::session_mcp::McpAttachmentOrigin::Session,
                                )
                            })
                            .collect(),
                    )
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
        response_revision,
        api_version,
    ))
}

/// `DELETE /v1/sessions/:id`.
async fn delete_session(
    ManagedRequestState(state): ManagedRequestState,
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
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
) -> Result<Json<ApiWire<Session>>, WireErr> {
    state
        .archive_session(&id)
        .await
        .map(|value| Json(api_version.wrap(value)))
        .map_err(error_response)
}

// -- Threads --

async fn list_threads(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Response, WireErr> {
    let version = history_time_version(&headers)?;
    if version == HistoryTimeVersion::V2 {
        reject_unsupported_thread_time_filter(raw.as_deref())?;
    }
    // Thread DTOs are a disposable view over the same committed transcript and
    // lifecycle feed as Thread Events. Refresh through that one projector so a
    // direct list cannot retain idle while the Runtime has already committed
    // Running (or skip directly from idle to terminal).
    state
        .refresh_committed_events_for_observation(&id, None)
        .await
        .map_err(error_response)?;
    match version {
        HistoryTimeVersion::V1 => state
            .list_threads(&id)
            .map(PageCursor::single)
            .map(|value| Json(api_version.wrap(value)))
            .map(IntoResponse::into_response)
            .map_err(error_response),
        HistoryTimeVersion::V2 => {
            let rows = state.list_threads_with_time(&id).map_err(error_response)?;
            let data = rows
                .iter()
                .map(|(thread, time)| thread_time_v2(thread, *time, api_version))
                .collect();
            Ok(Json(PageCursor::single(data)).into_response())
        }
    }
}

async fn get_thread(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path((id, tid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, WireErr> {
    let version = history_time_version(&headers)?;
    state
        .refresh_committed_events_for_observation(&id, None)
        .await
        .map_err(error_response)?;
    match version {
        HistoryTimeVersion::V1 => state
            .get_thread(&id, &tid)
            .map(|value| Json(api_version.wrap(value)))
            .map(IntoResponse::into_response)
            .map_err(error_response),
        HistoryTimeVersion::V2 => {
            let (thread, time) = state
                .get_thread_with_time(&id, &tid)
                .map_err(error_response)?;
            Ok(Json(thread_time_v2(&thread, time, api_version)).into_response())
        }
    }
}

async fn archive_thread(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path((id, tid)): Path<(String, String)>,
) -> Result<Json<ApiWire<SessionThread>>, WireErr> {
    state.ensure_session(&id).await.map_err(error_response)?;
    state
        .archive_thread(&id, &tid)
        .await
        .map(|value| Json(api_version.wrap(value)))
        .map_err(error_response)
}

/// Thread events are a typed projection of the Session's one committed event log.
async fn list_thread_events(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path((id, tid)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
    ManagedQuery(query): ManagedQuery<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<ApiWire<ListEventsResponse>>, WireErr> {
    let version = history_time_version(&headers)?;
    let filter = (version == HistoryTimeVersion::V2)
        .then(|| parse_event_list_filter(raw.as_deref()))
        .transpose()?;
    state
        .refresh_committed_events_for_observation(&id, query.page.as_deref())
        .await
        .map_err(error_response)?;
    match filter {
        None => state.list_thread_events(&id, &tid, query.page.as_deref(), query.limit),
        Some(filter) => state.list_thread_events_filtered_with_time(
            &id,
            &tid,
            query.page.as_deref(),
            query.limit,
            true,
            filter.has_time_predicate(),
            |event| filter.includes(event),
            |event| filter.includes_without_time(event),
        ),
    }
    .map(|value| Json(api_version.wrap(value)))
    .map_err(error_response)
}

/// Parse the SDK's `event_deltas[]` live-preview opt-in. Repeated `event_deltas[]`
/// (or `event_deltas`) values select which buffered events to preview; only
/// `agent.message` and `agent.thinking` are accepted (any other value is a 400,
/// matching the official wire). The returned selection keeps message and thinking
/// independent so opting into one cannot leak the other's preview frames.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PreviewSelection {
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

pub(crate) fn parse_event_deltas(raw: Option<&str>) -> Result<PreviewSelection, WireErr> {
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

pub(crate) fn parse_event_list_order(raw: Option<&str>) -> Result<SessionListOrder, WireErr> {
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

#[derive(Debug, Default)]
pub(crate) struct EventListFilter {
    created_at_gt: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_at_gte: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_at_lt: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_at_lte: Option<chrono::DateTime<chrono::FixedOffset>>,
    types: HashSet<String>,
}

impl EventListFilter {
    fn has_time_predicate(&self) -> bool {
        self.created_at_gt.is_some()
            || self.created_at_gte.is_some()
            || self.created_at_lt.is_some()
            || self.created_at_lte.is_some()
    }

    fn includes_without_time(&self, event: &Event) -> bool {
        self.types.is_empty() || self.types.contains(event.type_str())
    }

    fn includes(&self, event: &Event) -> bool {
        if !self.includes_without_time(event) {
            return false;
        }
        if !self.has_time_predicate() {
            return true;
        }
        let Some(processed_at) = event
            .processed_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        else {
            return false;
        };
        self.created_at_gt
            .as_ref()
            .is_none_or(|bound| processed_at > *bound)
            && self
                .created_at_gte
                .as_ref()
                .is_none_or(|bound| processed_at >= *bound)
            && self
                .created_at_lt
                .as_ref()
                .is_none_or(|bound| processed_at < *bound)
            && self
                .created_at_lte
                .as_ref()
                .is_none_or(|bound| processed_at <= *bound)
    }
}

pub(crate) fn parse_event_list_filter(raw: Option<&str>) -> Result<EventListFilter, WireErr> {
    let mut filter = EventListFilter::default();
    let mut seen_bounds = HashSet::new();
    let mut type_count = 0usize;
    for (key, value) in raw
        .into_iter()
        .flat_map(|query| form_urlencoded::parse(query.as_bytes()))
    {
        match key.as_ref() {
            "created_at[gt]" | "created_at[gte]" | "created_at[lt]" | "created_at[lte]" => {
                if !seen_bounds.insert(key.to_string()) {
                    return Err(error_response(StateError::Run(RunError::bad_request(
                        format!("{key} may be specified once"),
                    ))));
                }
                let Some(value) = crate::types::page::non_empty_query_value(&value) else {
                    continue;
                };
                let parsed = chrono::DateTime::parse_from_rfc3339(value).map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(format!(
                        "{key} must be an RFC 3339 timestamp"
                    ))))
                })?;
                match key.as_ref() {
                    "created_at[gt]" => filter.created_at_gt = Some(parsed),
                    "created_at[gte]" => filter.created_at_gte = Some(parsed),
                    "created_at[lt]" => filter.created_at_lt = Some(parsed),
                    "created_at[lte]" => filter.created_at_lte = Some(parsed),
                    _ => unreachable!("matched timestamp key"),
                }
            }
            "types" | "types[]" => {
                let Some(value) = crate::types::page::non_empty_query_value(&value) else {
                    continue;
                };
                type_count += 1;
                if type_count > 100 {
                    return Err(error_response(StateError::Run(RunError::bad_request(
                        "types allows at most 100 values",
                    ))));
                }
                filter.types.insert(value.to_owned());
            }
            _ => {}
        }
    }
    Ok(filter)
}

async fn stream_thread_events(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path((id, tid)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, StateError>>>, WireErr> {
    require_v1_stream_time(&headers)?;
    let deadline = private_stream_deadline(state.as_ref());
    // The official Thread EventStreamParams carries the same preview selector as
    // the Session stream. Primary and ordinary child Threads consume the same
    // Runtime-owned live observer; Advisor consultations intentionally expose
    // only their documented lifecycle/cross-post wire.
    let previews = parse_event_deltas(raw.as_deref())?;
    let observation = state
        .refresh_committed_events_for_observation(&id, None)
        .await
        .map_err(error_response)?;
    let thread = state.get_thread(&id, &tid).map_err(error_response)?;
    let (snapshot, rx, revision) = state
        .stream_projection_subscribe(&id)
        .map_err(error_response)?;
    let project_thread = internal_thread_id(&id, &tid);
    let primary = project_thread == id;
    let thread_preview = if previews.any() && !thread.agent.is_advisor() {
        state
            .roles
            .observation
            .subscribe_session_thread_live(
                &awaken_session_types::SessionId::from(&id),
                &project_thread,
            )
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
        LiveSseSource {
            state: Arc::clone(&state),
            session_id: id.clone(),
            owner_scope: state
                .resolve_owner(&id)
                .await
                .map_err(error_response)?
                .ok_or_else(|| error_response(StateError::NotFound))?,
            revision,
            current: observation == ProjectionObservationDecision::UseRefreshed,
            deadline,
            api_version,
        },
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

/// Read-only projection of the canonical Session Resource root and policy.
pub async fn get_resource_manifest(
    ManagedRequestState(state): ManagedRequestState,
    Path(id): Path<String>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
) -> Result<Json<crate::types::resource::SessionResourceObservation>, WireErr> {
    ensure_session_scope(state.as_ref(), &id, Some(&WorkspaceScope(workspace))).await?;
    crate::state::observe_resource_manifest(state.as_ref(), &id)
        .await
        .map(Json)
        .map_err(error_response)
}

/// Lower the Awaken Resource Manifest wire request into the canonical Session
/// application command. The explicitly namespaced protocol crate owns the HTTP
/// method/path and injects this one handler rather than duplicating the lowering.
pub async fn replace_resource_manifest(
    ManagedRequestState(state): ManagedRequestState,
    Path(id): Path<String>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    Json(body): Json<crate::types::resource::ResourceManifestReplaceParams>,
) -> Result<
    (
        HeaderMap,
        Json<crate::types::resource::SessionResourceManifest>,
    ),
    (StatusCode, Json<ErrorResponse>),
> {
    ensure_session_scope(state.as_ref(), &id, Some(&WorkspaceScope(workspace))).await?;
    let idempotency_key = parse_idempotency_key(&headers)?;
    let if_match = super::parse_if_match(&headers, "Resource manifest")?;
    let fingerprints = body
        .resources
        .iter()
        .map(crate::types::resource::ResourceInput::idempotency_fingerprint)
        .collect::<Vec<_>>();
    let request_fingerprint = awaken_agent_contract::stable_fingerprint(&fingerprints);
    // The state owner composes Repository lowering, root CAS, realization, and
    // durable retirement. Keep that complete application future behind the one
    // HTTP-adapter boundary instead of embedding it in Axum's request future.
    let manifest = Box::pin(state.replace_resource_manifest(
        &id,
        body,
        idempotency_key,
        if_match,
        request_fingerprint,
    ))
    .await
    .map_err(error_response)?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", manifest.desired_revision))
            .expect("numeric Resource manifest revision is a valid ETag"),
    );
    Ok((response_headers, Json(manifest)))
}

fn historical_profiled_repository_binding_id(
    session_id: &str,
    repository_id: &awaken_resource_contract::RepositoryId,
) -> awaken_resource_contract::BindingId {
    awaken_resource_contract::BindingId::new(format!(
        "profiled:{session_id}:repository:{}",
        repository_id.as_str()
    ))
}

async fn profiled_initial_preparation(
    state: &ManagedState,
    id: &awaken_session_types::SessionId,
    requested: bool,
) -> Result<Option<awaken_session_contract::SessionPreparationObservation>, WireErr> {
    if !requested {
        return Ok(None);
    }
    let root = state
        .roles
        .profiled
        .request_run_free_initial_preparation(id)
        .await
        .map_err(|error| error_response(StateError::Run(error.into())))?;
    Ok(Some(
        awaken_session_contract::SessionPreparationObservation::from_session(&root),
    ))
}

/// Lower Awaken's strongly typed extension request into the sole profiled
/// Session composer. The handler owns no Session state or realization path.
pub async fn create_profiled_session(
    ManagedRequestState(state): ManagedRequestState,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
    Json(body): Json<crate::awaken_extensions::ProfiledSessionCreate>,
) -> Result<
    (
        StatusCode,
        Json<crate::awaken_extensions::ProfiledSessionCreated>,
    ),
    (StatusCode, Json<ErrorResponse>),
> {
    if state.private_binding.is_some() {
        return Err(error_response(StateError::Run(RunError::unavailable(
            "private profiled Session creation is unavailable",
        ))));
    }
    if body.session_id.trim().is_empty()
        || body.agent_id.trim().is_empty()
        || body.source_revision == Some(0)
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "profiled Session identity or revision is invalid",
        ))));
    }
    let request_initial_preparation = body.request_initial_preparation;
    let request_fingerprint = awaken_agent_contract::stable_fingerprint(&body);
    let idempotency = awaken_session_application::SessionCreateReceipt::Profiled
        .record(&body.session_id, request_fingerprint);
    if let Some(existing) = state
        .replay_accepted_session_with_receipt(&body.session_id, &owner_scope, &idempotency)
        .await
        .map_err(error_response)?
    {
        let initial_preparation =
            profiled_initial_preparation(&state, &existing.session_id, request_initial_preparation)
                .await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(crate::awaken_extensions::ProfiledSessionCreated {
                id: existing.session_id.into_string(),
                metadata: existing.metadata,
                initial_preparation,
            }),
        ));
    }
    let mutation_policy = body.mode.mutation_policy();
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
        .map(|(index, repository)| {
            let repository_id = awaken_resource_contract::RepositoryId::from(format!(
                "profiled:{}:repository:{index}",
                body.session_id
            ));
            let binding_id = repository.binding_id.map_or_else(
                || historical_profiled_repository_binding_id(&body.session_id, &repository_id),
                awaken_resource_contract::BindingId::new,
            );
            awaken_session_application::ProfiledSessionRepositoryInput {
                binding_id,
                repository: awaken_session_application::SessionRepositoryResourceInput {
                    id: repository_id.to_string(),
                    workspace_id: owner_scope.clone(),
                    name: format!("Profiled Session repository {index}"),
                    description: "Product-authored profiled Session input".into(),
                    remote_url: repository.remote_url,
                    credential_material: None,
                    credential: repository.credential,
                    mount_path: repository.mount_path,
                    initial_branch: repository.initial_branch,
                    initial_commit: repository.initial_commit,
                },
            }
        })
        .collect();
    let session = state
        .roles
        .profiled
        .accept_profiled_session(awaken_session_application::CreateProfiledSessionCommand {
            owner_scope,
            session_id: body.session_id,
            mutation_policy,
            application_origin: body.application_origin,
            agent_id: body.agent_id,
            source_revision: body.source_revision,
            environment_id: body.environment_id,
            model: None,
            mounts: body.mounts,
            env: body.env,
            prompts: body.prompts,
            resource_inputs: body.resource_inputs,
            mcp_candidates,
            repositories,
            network_restriction: body.network_restriction,
            title: body.title,
            metadata: body.metadata,
            tools: body.tools,
            idempotency: Some(idempotency),
        })
        .await
        .map_err(|error| error_response(ManagedState::map_creation_error(error)))?;
    let initial_preparation =
        profiled_initial_preparation(&state, &session.session_id, request_initial_preparation)
            .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(crate::awaken_extensions::ProfiledSessionCreated {
            id: session.session_id.into_string(),
            metadata: session.metadata,
            initial_preparation,
        }),
    ))
}

fn profiled_release_error_response(
    error: awaken_session_application::SessionArchiveWithRepositoryPublicationError,
) -> WireErr {
    use awaken_session_application::SessionArchiveWithRepositoryPublicationError as Error;

    match error {
        Error::NotFound => error_response(StateError::NotFound),
        Error::Conflict => error_response(StateError::Conflict),
        Error::Pending(message) | Error::Unavailable(message) => {
            error_response(StateError::Run(RunError::unavailable(message)))
        }
        Error::Rejected(error) => error_response(StateError::Run(error)),
    }
}

/// Project the private product release onto the sole durable Session cleanup
/// operation. The adapter selects an existing frozen binding and returns the
/// canonical provisioning receipt; it owns neither a publication queue nor a
/// Git transport implementation.
pub async fn release_profiled_session(
    ManagedRequestState(state): ManagedRequestState,
    Path(id): Path<String>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
    Json(body): Json<crate::awaken_extensions::ProfiledSessionRelease>,
) -> Result<Json<crate::awaken_extensions::ProfiledSessionReleased>, WireErr> {
    let request_scope = WorkspaceScope(owner_scope.clone());
    ensure_session_scope(state.as_ref(), &id, Some(&request_scope)).await?;

    let Some(publication) = body.repository_publication else {
        state
            .release_profiled_session_without_publication(&id)
            .await
            .map_err(error_response)?;
        return Ok(Json(crate::awaken_extensions::ProfiledSessionReleased {
            id,
            repository_publication: None,
        }));
    };
    if publication.binding_id.trim().is_empty()
        || publication.binding_id.trim() != publication.binding_id
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "Repository publication binding_id must be non-empty without surrounding whitespace",
        ))));
    }

    let observed_at = state.roles.clock.now_ms();
    let binding_id = publication.binding_id.clone();
    let outcome = Box::pin(state.roles.termination.archive_with_repository_publication(
        profiled_release::command(&owner_scope, &id, publication, observed_at),
    ))
    .await
    .map_err(profiled_release_error_response)?;

    Ok(Json(crate::awaken_extensions::ProfiledSessionReleased {
        id,
        repository_publication: Some(
            crate::awaken_extensions::ProfiledSessionRepositoryPublished {
                binding_id,
                receipt: outcome.publication_receipt.effect_receipt,
            },
        ),
    }))
}

async fn create_resource(
    ManagedRequestState(state): ManagedRequestState,
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
    ManagedRequestState(state): ManagedRequestState,
    Path(id): Path<String>,
) -> Result<
    (
        HeaderMap,
        Json<PageCursor<crate::types::resource::SessionResource>>,
    ),
    WireErr,
> {
    let (revision, resources) = state.list_resources(&id).await.map_err(error_response)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{revision}\""))
            .expect("numeric Resource manifest revision is a valid ETag"),
    );
    Ok((headers, Json(PageCursor::single(resources))))
}

async fn get_resource(
    ManagedRequestState(state): ManagedRequestState,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Json<crate::types::resource::SessionResource>, WireErr> {
    state
        .get_resource(&id, &rid)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn update_resource(
    ManagedRequestState(state): ManagedRequestState,
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
    ManagedRequestState(state): ManagedRequestState,
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

fn event_data_subject(headers: &HeaderMap) -> Result<Option<String>, WireErr> {
    headers
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
        .transpose()
}

async fn send_events(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    headers: HeaderMap,
    ManagedJson(req): ManagedJson<SendEventsRequest>,
) -> Result<Json<ApiWire<SendEventsResponse>>, (StatusCode, Json<ErrorResponse>)> {
    // Anthropic's Messages SDK projects request-grain `user_profile_id` to this
    // header. Reuse the context carrier for Managed events without adding an
    // out-of-contract field to the strict event envelope.
    let data_subject_id = event_data_subject(&headers)?;
    let idempotency_key = parse_idempotency_key(&headers)?;
    state
        .send_events_attributed(&id, req, data_subject_id, idempotency_key)
        .await
        .map(|value| Json(api_version.wrap(value)))
        .map_err(error_response)
}

async fn list_events(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
    ManagedQuery(query): ManagedQuery<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<ApiWire<ListEventsResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let version = history_time_version(&headers)?;
    let order = parse_event_list_order(raw.as_deref())?;
    let filter = parse_event_list_filter(raw.as_deref())?;
    state
        .refresh_committed_events_for_observation(&id, query.page.as_deref())
        .await
        .map_err(error_response)?;
    state
        .list_events_filtered_with_time(
            &id,
            query.page.as_deref(),
            query.limit,
            order == SessionListOrder::Desc,
            version == HistoryTimeVersion::V2,
            filter.has_time_predicate(),
            |event| filter.includes(event),
            |event| filter.includes_without_time(event),
        )
        .map(|value| Json(api_version.wrap(value)))
        .map_err(error_response)
}

async fn stream_events(
    ManagedRequestState(state): ManagedRequestState,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, StateError>>>, WireErr> {
    require_v1_stream_time(&headers)?;
    let deadline = private_stream_deadline(state.as_ref());
    // Opt in to live previews (`event_start`/`event_delta`) via `event_deltas[]`.
    let previews = parse_event_deltas(raw.as_deref())?;
    let observation = state
        .refresh_committed_events_for_observation(&id, None)
        .await
        .map_err(error_response)?;
    let (snapshot, rx, revision) = state
        .stream_projection_subscribe(&id)
        .map_err(error_response)?;
    let thread_preview = if previews.any() {
        state
            .roles
            .observation
            .subscribe_session_thread_live(&awaken_session_types::SessionId::from(&id), &id)
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
        LiveSseSource {
            state: Arc::clone(&state),
            session_id: id.clone(),
            owner_scope: state
                .resolve_owner(&id)
                .await
                .map_err(error_response)?
                .ok_or_else(|| error_response(StateError::NotFound))?,
            revision,
            current: observation == ProjectionObservationDecision::UseRefreshed,
            deadline,
            api_version,
        },
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
#[path = "sessions/repository_errors.rs"]
mod repository_errors;

#[cfg(test)]
mod managed_json_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::{
        HistoryTimeVersion, SseTerminalScope, enforce_managed_beta, error_response,
        history_time_version, parse_event_deltas, reject_unsupported_thread_time_filter,
        replay_is_terminal_after, require_v1_stream_time,
    };
    use crate::routes::extractors::managed_json_message;
    use crate::state::StateError;
    use awaken_runtime_contract::runs::RunError;

    #[test]
    fn history_time_header_selects_only_one_exact_version_and_rejects_sse_v2() {
        // Causes: H1 absent, H2 one exact `2`, H3 another value, H4 duplicate
        // fields, H5 comma-coalesced duplicates. Effects: H1 selects v1; H2
        // selects v2 for history but returns 400 on SSE; H3-H5 return 400 on
        // both surfaces. No malformed selector may silently become v1.
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(
            history_time_version(&headers).unwrap(),
            HistoryTimeVersion::V1
        );
        assert!(require_v1_stream_time(&headers).is_ok());
        headers.append(super::SESSION_TIME_VERSION, "2".parse().unwrap());
        assert_eq!(
            history_time_version(&headers).unwrap(),
            HistoryTimeVersion::V2
        );
        assert_eq!(
            require_v1_stream_time(&headers).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        for malformed in ["1", "2, 2", " 2", "3"] {
            let mut headers = axum::http::HeaderMap::new();
            headers.append(super::SESSION_TIME_VERSION, malformed.parse().unwrap());
            assert_eq!(
                history_time_version(&headers).unwrap_err().0,
                StatusCode::BAD_REQUEST
            );
        }
        headers.append(super::SESSION_TIME_VERSION, "2".parse().unwrap());
        assert_eq!(
            history_time_version(&headers).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        // T1 v2 Thread list with no time predicate is allowed; T2 any time
        // predicate has no Thread-list contract and returns 400. Session and
        // Event lists instead own supported time predicates and 503 unknowns.
        assert!(reject_unsupported_thread_time_filter(None).is_ok(), "T1");
        for query in [
            "created_at%5Bgt%5D=2026-01-01T00%3A00%3A00Z",
            "updated_at%5Blte%5D=2026-01-01T00%3A00%3A00Z",
            "archived_at%5Bgte%5D=2026-01-01T00%3A00%3A00Z",
        ] {
            assert_eq!(
                reject_unsupported_thread_time_filter(Some(query))
                    .unwrap_err()
                    .0,
                StatusCode::BAD_REQUEST,
                "T2"
            );
        }
    }

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

    #[test]
    fn profiled_create_preserves_typed_repository_statuses() {
        // Profiled-create status cause/effect table. C1 occupied identity; C2
        // tombstoned identity; C3 same key/different request (including a race
        // loser); C4 dependency outage; C5 corrupt durable receipt/aggregate.
        // Effects are stable Anthropic envelopes with 409 for C1-C3, 503 for C4,
        // and 500 for C5. The adapter matches typed application errors only;
        // message text and concurrency timing cannot change the status.
        use awaken_session_application::SessionCreationError;

        for (rule, error) in [
            ("C1", SessionCreationError::Conflict),
            ("C2", SessionCreationError::Tombstoned),
            ("C3", SessionCreationError::IdempotencyMismatch),
        ] {
            let (status, body) =
                error_response(crate::state::ManagedState::map_creation_error(error));
            assert_eq!(status, StatusCode::CONFLICT, "{rule}");
            assert_eq!(body.0.error.kind, "invalid_request_error", "{rule}");
        }
        let (status, body) = error_response(crate::state::ManagedState::map_creation_error(
            SessionCreationError::Unavailable("offline".into()),
        ));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "C4");
        assert_eq!(body.0.error.kind, "api_error", "C4");
        let (status, body) = error_response(crate::state::ManagedState::map_creation_error(
            SessionCreationError::Internal("dangling receipt".into()),
        ));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "C5");
        assert_eq!(body.0.error.kind, "api_error", "C5");
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
