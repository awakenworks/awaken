// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Authorizes Managed Session reads and maps committed diagnostics into the public API.
//! Scoped history over the configured Session diagnostic authority.

use awaken_agent_contract::RunId;
use awaken_session_application::replay::{
    RunEvidence, SessionReplayError, TimelineCursor, TimelineEntry, TimelinePage, TimelineQuery,
    TimelineSnapshot,
};
use awaken_session_types::SessionId;
use axum::Json;
use axum::extract::{Path, Query, rejection::QueryRejection};
use axum::http::StatusCode;

use super::{WireErr, error_response, session_not_found};
use crate::common::scope::RequiredWorkspaceScope;
use crate::state::{ManagedRequestState, StateError};
use awaken_runtime_contract::runs::RunError;

#[derive(serde::Deserialize)]
pub struct TimelineRequest {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    thread_id: Option<String>,
    run_id: Option<RunId>,
    operation_id: Option<String>,
    source: Option<awaken_session_application::replay::EvidenceSource>,
    limit: Option<usize>,
    cursor: Option<TimelineCursor>,
}

#[derive(serde::Deserialize)]
pub struct RunEvidenceRequest {
    snapshot: TimelineSnapshot,
}

pub async fn session_timeline(
    ManagedRequestState(state): ManagedRequestState,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    query: Result<Query<TimelineRequest>, QueryRejection>,
) -> Result<Json<TimelinePage>, WireErr> {
    let access = crate::state::authorize_session(&state, &workspace, &SessionId::from(id.as_str()))
        .await
        .map_err(|error| match error {
            awaken_session_application::SessionAccessError::NotFound => session_not_found(&id),
            error => error_response(StateError::Run(error.into())),
        })?;
    let Query(query) =
        query.map_err(|_| diagnostic_error(&id, SessionReplayError::InvalidQuery))?;
    let filters = TimelineQuery {
        from_ms: query.from_ms,
        to_ms: query.to_ms,
        thread_id: query.thread_id.map(|thread| {
            awaken_agent_contract::ThreadId(crate::state::internal_thread_id(&id, &thread))
        }),
        run_id: query.run_id,
        operation_id: query.operation_id,
        source: query.source,
        limit: query.limit.unwrap_or(50),
    };
    let mut page = state
        .roles
        .observation
        .timeline(&access, &filters, query.cursor.as_ref())
        .await
        .map_err(|error| diagnostic_error(&id, error))?;
    for entry in &mut page.entries {
        public_entry(&id, entry);
    }
    Ok(Json(page))
}

pub async fn run_evidence_at(
    ManagedRequestState(state): ManagedRequestState,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, run)): Path<(String, String)>,
    query: Result<Query<RunEvidenceRequest>, QueryRejection>,
) -> Result<Json<RunEvidence>, WireErr> {
    let access = crate::state::authorize_session(&state, &workspace, &SessionId::from(id.as_str()))
        .await
        .map_err(|error| match error {
            awaken_session_application::SessionAccessError::NotFound => session_not_found(&id),
            error => error_response(StateError::Run(error.into())),
        })?;
    let Query(query) =
        query.map_err(|_| diagnostic_error(&id, SessionReplayError::InvalidQuery))?;
    let mut evidence = state
        .roles
        .observation
        .run_evidence_at(&access, &RunId(run), &query.snapshot)
        .await
        .map_err(|error| diagnostic_error(&id, error))?;
    evidence.thread_id.0 = crate::state::public_thread_id(&id, &evidence.thread_id.0);
    for entry in [
        &mut evidence.admission,
        &mut evidence.runtime,
        &mut evidence.dispatch,
    ]
    .into_iter()
    .flatten()
    {
        public_entry(&id, entry);
    }
    for edge in &mut evidence.messages {
        edge.source_thread.0 = crate::state::public_thread_id(&id, &edge.source_thread.0);
        edge.target_thread.0 = crate::state::public_thread_id(&id, &edge.target_thread.0);
    }
    Ok(Json(evidence))
}

fn public_entry(id: &str, entry: &mut TimelineEntry) {
    if let Some(thread) = &mut entry.thread_id {
        thread.0 = crate::state::public_thread_id(id, &thread.0);
    }
}

fn diagnostic_error(id: &str, error: SessionReplayError) -> WireErr {
    match error {
        SessionReplayError::NotFound => session_not_found(id),
        SessionReplayError::InvalidQuery => {
            error_response(StateError::Run(RunError::bad_request(error.to_string())))
        }
        SessionReplayError::HistoryUnavailable(_) => (
            StatusCode::CONFLICT,
            Json(crate::types::ErrorResponse::new(
                "history_unavailable",
                "Session diagnostic history unavailable",
            )),
        ),
        SessionReplayError::Unavailable(_) => error_response(StateError::Run(
            RunError::unavailable("Session diagnostic authority unavailable"),
        )),
    }
}
