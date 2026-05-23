//! ProtocolReplayLog-backed AI SDK stream replay.

use std::convert::Infallible;
use std::sync::Arc;

use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayCursor, ProtocolReplayError, ProtocolReplayLog, ProtocolReplayRecord,
    ProtocolStreamKey,
};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use serde::Deserialize;

use crate::app::ProtocolRoutesState;
use crate::protocol_projector::{AI_SDK_PROTOCOL, AI_SDK_PROTOCOL_VERSION};
use crate::routes::ApiError;

const LAST_EVENT_ID_HEADER: &str = "last-event-id";
const DEFAULT_REPLAY_LIMIT: usize = 100;
const MAX_REPLAY_LIMIT: usize = 500;

#[derive(Debug, Deserialize, Default)]
struct ReplayQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub fn ai_sdk_replay_routes() -> Router<ProtocolRoutesState> {
    Router::new()
        .route("/v1/ai-sdk/threads/:thread_id/replay", get(replay_stream))
        .route("/v1/ai-sdk/chat/:thread_id/replay", get(replay_stream))
}

async fn replay_stream(
    State(st): State<ProtocolRoutesState>,
    Path(thread_id): Path<String>,
    Query(query): Query<ReplayQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let log = configured_replay_log(&st)?;
    let cursor = cursor_from_query_or_header(query.cursor.as_deref(), &headers)?;
    let frames = replay_frames(log, &thread_id, cursor, replay_limit(query.limit)).await?;
    Ok(response_from_frames(frames))
}

pub(crate) async fn resume_from_replay_log(
    st: &ProtocolRoutesState,
    thread_id: &str,
    headers: &HeaderMap,
) -> Result<Option<Response>, ApiError> {
    let Some(log) =
        crate::protocol_replay_state::protocol_replay_log_for_buffers(&st.protocol.replay_buffers)
    else {
        return Ok(None);
    };
    let cursor = cursor_from_resume_header(headers)?;
    let frames = replay_frames(log, thread_id, cursor.clone(), DEFAULT_REPLAY_LIMIT).await?;
    if frames.is_empty() && cursor.is_none() {
        return Ok(None);
    }
    Ok(Some(response_from_frames(frames)))
}

fn configured_replay_log(st: &ProtocolRoutesState) -> Result<Arc<dyn ProtocolReplayLog>, ApiError> {
    crate::protocol_replay_state::protocol_replay_log_for_buffers(&st.protocol.replay_buffers)
        .ok_or_else(|| {
            ApiError::ServiceUnavailable("protocol replay log is not configured".to_string())
        })
}

async fn replay_frames(
    log: Arc<dyn ProtocolReplayLog>,
    thread_id: &str,
    cursor: Option<ProtocolReplayCursor>,
    limit: usize,
) -> Result<Vec<Bytes>, ApiError> {
    let stream = ProtocolStreamKey::new(
        format!("thread:{thread_id}"),
        AI_SDK_PROTOCOL,
        AI_SDK_PROTOCOL_VERSION,
    )
    .map_err(map_protocol_replay_error)?;
    let page = log
        .list_replay(stream, cursor, limit)
        .await
        .map_err(map_protocol_replay_error)?;
    page.records.iter().map(record_frame).collect()
}

fn response_from_frames(frames: Vec<Bytes>) -> Response {
    let stream = futures::stream::iter(frames.into_iter().map(Ok::<Bytes, Infallible>));
    super::http::ai_sdk_sse_response(stream)
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

fn cursor_from_resume_header(
    headers: &HeaderMap,
) -> Result<Option<ProtocolReplayCursor>, ApiError> {
    headers
        .get(LAST_EVENT_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|cursor| !cursor.bytes().all(|byte| byte.is_ascii_digit()))
        .map(replay_cursor)
        .transpose()
}

pub(crate) fn resume_header_has_protocol_cursor(headers: &HeaderMap) -> bool {
    headers
        .get(LAST_EVENT_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cursor| !cursor.bytes().all(|byte| byte.is_ascii_digit()))
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
        // ADR-0034 D8: a missing replay row inside retention is an integrity
        // violation that must be alertable, not a generic 500.
        ProtocolReplayError::Integrity(message) => ApiError::DataIntegrity(message),
        ProtocolReplayError::Io(message) => ApiError::Internal(message),
        ProtocolReplayError::Serialization(message) => ApiError::Internal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::protocol_replay_log::{
        ProtocolReplayDraft, ProtocolReplayWriter,
    };
    use awaken_runtime::AgentRuntime;
    use awaken_stores::{InMemoryMailboxStore, InMemoryProtocolReplayLog, InMemoryStore};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::app::{ServerConfig, ServerState};
    use crate::mailbox::{Mailbox, MailboxConfig};
    use crate::protocol_projector::AI_SDK_PROJECTOR_VERSION;
    use crate::protocol_replay_state::with_protocol_replay_log;
    use crate::routes::build_router;
    use crate::transport::replay_buffer::EventReplayBuffer;

    struct StubResolver;

    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn make_state(replay_log: Option<Arc<InMemoryProtocolReplayLog>>) -> ServerState {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(InMemoryStore::new());
        let mailbox_store = Arc::new(InMemoryMailboxStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            mailbox_store,
            store.clone(),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        let state = ServerState::new(
            runtime,
            mailbox,
            store,
            Arc::new(StubResolver),
            ServerConfig::default(),
        );
        match replay_log {
            Some(log) => with_protocol_replay_log(state, log),
            None => state,
        }
    }

    async fn append_replay(
        log: &InMemoryProtocolReplayLog,
        thread_id: &str,
        wire_event_id: &str,
        payload: &str,
    ) -> ProtocolReplayCursor {
        let mut draft = ProtocolReplayDraft::new(
            format!("thread:{thread_id}"),
            AI_SDK_PROTOCOL,
            AI_SDK_PROTOCOL_VERSION,
            AI_SDK_PROJECTOR_VERSION,
            wire_event_id,
            "test",
            payload.as_bytes().to_vec(),
        )
        .unwrap();
        draft.wire_payload_json = Some(serde_json::from_str(payload).unwrap());
        log.append_replay(draft)
            .await
            .unwrap()
            .record
            .protocol_replay_cursor
    }

    async fn get(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn replay_route_streams_protocol_replay_rows_with_cursor_ids() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let first = append_replay(
            &replay_log,
            "thread-replay",
            "wire-1",
            r#"{"type":"start","messageId":"run-1"}"#,
        )
        .await;
        append_replay(
            &replay_log,
            "thread-replay",
            "wire-2",
            r#"{"type":"finish","finishReason":"stop"}"#,
        )
        .await;
        let state = make_state(Some(replay_log));
        let app = build_router(&state);

        let (status, body) = get(app, "/v1/ai-sdk/threads/thread-replay/replay").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&format!("id: {}", first.as_str())));
        assert!(body.contains(r#"data: {"type":"start","messageId":"run-1"}"#));
        assert!(body.contains(r#"data: {"type":"finish","finishReason":"stop"}"#));
    }

    #[tokio::test]
    async fn replay_route_resumes_after_protocol_cursor() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let first = append_replay(
            &replay_log,
            "thread-replay",
            "wire-1",
            r#"{"type":"start","messageId":"run-1"}"#,
        )
        .await;
        append_replay(
            &replay_log,
            "thread-replay",
            "wire-2",
            r#"{"type":"finish","finishReason":"stop"}"#,
        )
        .await;
        let state = make_state(Some(replay_log));
        let app = build_router(&state);
        let uri = format!(
            "/v1/ai-sdk/threads/thread-replay/replay?cursor={}",
            first.as_str()
        );

        let (status, body) = get(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains(r#"messageId":"run-1"#));
        assert!(body.contains(r#""finishReason":"stop""#));
    }

    #[tokio::test]
    async fn replay_route_requires_configured_protocol_log() {
        let state = make_state(None);
        let app = build_router(&state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/ai-sdk/threads/thread-replay/replay")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn ai_sdk_stream_falls_back_to_protocol_replay_when_no_live_buffer() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let first = append_replay(
            &replay_log,
            "thread-replay",
            "wire-1",
            r#"{"type":"start","messageId":"run-1"}"#,
        )
        .await;
        let state = make_state(Some(replay_log));
        let app = build_router(&state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/ai-sdk/threads/thread-replay/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(&format!("id: {}", first.as_str())));
    }

    #[tokio::test]
    async fn ai_sdk_stream_does_not_mix_protocol_cursor_with_live_buffer_ids() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let first = append_replay(
            &replay_log,
            "thread-replay",
            "wire-1",
            r#"{"type":"start","messageId":"run-1"}"#,
        )
        .await;
        append_replay(
            &replay_log,
            "thread-replay",
            "wire-2",
            r#"{"type":"finish","finishReason":"stop"}"#,
        )
        .await;
        let state = make_state(Some(replay_log));
        let live_buffer = Arc::new(EventReplayBuffer::new(10));
        live_buffer.push_json(r#"{"type":"live-buffer"}"#);
        state.insert_replay_buffer("thread-replay".to_string(), live_buffer);
        let app = build_router(&state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/ai-sdk/threads/thread-replay/stream")
                    .header("last-event-id", first.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(r#""finishReason":"stop""#));
        assert!(!body.contains("live-buffer"));
    }
}
