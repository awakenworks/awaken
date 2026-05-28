//! ProtocolReplayLog-backed AG-UI stream replay.

use std::sync::Arc;

use awaken_server_contract::ScopeContext;
use awaken_server_contract::contract::protocol_replay_log::{
    ProtocolReplayCursor, ProtocolReplayError, ProtocolReplayLog, ProtocolReplayRecord,
    ProtocolStreamKey, ScopedProtocolReplayLog,
};
use axum::Extension;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use serde::Deserialize;

use crate::app::ProtocolRoutesState;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::protocol_projector::{AG_UI_PROTOCOL, AG_UI_PROTOCOL_VERSION};
use crate::routes::ApiError;

const LAST_EVENT_ID_HEADER: &str = "last-event-id";
const DEFAULT_REPLAY_LIMIT: usize = 100;
const MAX_REPLAY_LIMIT: usize = 500;

#[derive(Debug, Deserialize, Default)]
struct ReplayQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub fn ag_ui_replay_routes() -> Router<ProtocolRoutesState> {
    Router::new().route("/v1/ag-ui/threads/:thread_id/replay", get(replay_stream))
}

async fn replay_stream(
    State(st): State<ProtocolRoutesState>,
    Extension(scope): Extension<ScopeContext>,
    Path(thread_id): Path<String>,
    Query(query): Query<ReplayQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let st = st.scoped(&scope);
    let log = configured_replay_log(&st)?;
    let cursor = cursor_from_query_or_header(query.cursor.as_deref(), &headers)?;
    let frames = replay_frames(log, &thread_id, cursor, replay_limit(query.limit)).await?;
    Ok(response_from_frames(frames))
}

fn configured_replay_log(st: &ProtocolRoutesState) -> Result<Arc<dyn ProtocolReplayLog>, ApiError> {
    let log =
        crate::protocol_replay_state::protocol_replay_log_for_buffers(&st.protocol.replay_buffers)
            .ok_or_else(|| {
                ApiError::ServiceUnavailable("protocol replay log is not configured".to_string())
            })?;
    Ok(st.run.scope_id.as_ref().map_or(log.clone(), |scope_id| {
        Arc::new(ScopedProtocolReplayLog::new(log, scope_id.clone())) as Arc<dyn ProtocolReplayLog>
    }))
}

async fn replay_frames(
    log: Arc<dyn ProtocolReplayLog>,
    thread_id: &str,
    cursor: Option<ProtocolReplayCursor>,
    limit: usize,
) -> Result<Vec<Bytes>, ApiError> {
    let stream = ProtocolStreamKey::new(
        format!("thread:{thread_id}"),
        AG_UI_PROTOCOL,
        AG_UI_PROTOCOL_VERSION,
    )
    .map_err(map_protocol_replay_error)?;
    let page = log
        .list_replay(stream, cursor, limit)
        .await
        .map_err(map_protocol_replay_error)?;
    page.records.iter().map(record_frame).collect()
}

fn response_from_frames(frames: Vec<Bytes>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(frames.len().max(1));
    tokio::spawn(async move {
        for frame in frames {
            if tx.send(frame).await.is_err() {
                return;
            }
        }
    });
    sse_response(sse_body_stream(rx))
}

fn record_frame(record: &ProtocolReplayRecord) -> Result<Bytes, ApiError> {
    let payload = std::str::from_utf8(&record.wire_payload_bytes)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    Ok(Bytes::from(format!(
        "id: {}\ndata: {}\n\n",
        record.protocol_replay_cursor.as_str(),
        payload
    )))
}

fn cursor_from_query_or_header(
    query_cursor: Option<&str>,
    headers: &HeaderMap,
) -> Result<Option<ProtocolReplayCursor>, ApiError> {
    match query_cursor {
        Some(cursor) => replay_cursor(cursor).map(Some),
        None => headers
            .get(LAST_EVENT_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(replay_cursor)
            .transpose(),
    }
}

fn replay_cursor(cursor: &str) -> Result<ProtocolReplayCursor, ApiError> {
    if cursor.trim().is_empty() {
        return Err(ApiError::BadRequest("cursor cannot be empty".to_string()));
    }
    ProtocolReplayCursor::new(cursor).map_err(map_protocol_replay_error)
}

fn replay_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_REPLAY_LIMIT)
        .clamp(1, MAX_REPLAY_LIMIT)
}

fn map_protocol_replay_error(error: ProtocolReplayError) -> ApiError {
    match error {
        ProtocolReplayError::Validation(message) => ApiError::BadRequest(message),
        ProtocolReplayError::Conflict(message) => ApiError::Conflict(message),
        ProtocolReplayError::CursorExpired(message) => ApiError::Gone(message),
        ProtocolReplayError::Integrity(message) => ApiError::DataIntegrity(message),
        ProtocolReplayError::Io(message) => ApiError::Internal(message),
        ProtocolReplayError::Serialization(message) => ApiError::Internal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_frame_uses_protocol_cursor_as_sse_id() {
        let draft =
            awaken_server_contract::contract::protocol_replay_log::ProtocolReplayDraft::new(
                "thread:t1",
                AG_UI_PROTOCOL,
                AG_UI_PROTOCOL_VERSION,
                crate::protocol_projector::AG_UI_PROJECTOR_VERSION,
                "wire-1",
                "run_started",
                br#"{"type":"run_started"}"#.to_vec(),
            )
            .unwrap();
        let record = ProtocolReplayRecord::from_append(
            awaken_server_contract::contract::protocol_replay_log::ProtocolReplayId::new("pr_1")
                .unwrap(),
            ProtocolReplayCursor::new("cur_1").unwrap(),
            42,
            draft,
        )
        .unwrap();

        let frame = record_frame(&record).unwrap();

        assert_eq!(
            frame,
            Bytes::from_static(b"id: cur_1\ndata: {\"type\":\"run_started\"}\n\n")
        );
    }
}
