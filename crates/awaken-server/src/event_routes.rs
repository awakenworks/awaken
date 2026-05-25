//! Canonical event-list and cursor-resume routes.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use awaken_contract::contract::event_store::{
    CanonicalEvent, EventCursor, EventScope, EventStoreError, EventVisibility, SubscribeStart,
};

use crate::app::EventModuleState;
use crate::http_sse::sse_response;
use crate::routes::ApiError;

const DEFAULT_EVENT_LIMIT: usize = 50;
const MAX_EVENT_LIMIT: usize = 200;
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

pub(crate) fn event_routes() -> Router<EventModuleState> {
    Router::new()
        .route("/v1/threads/:thread_id/events", get(list_thread_events))
        .route(
            "/v1/threads/:thread_id/events/stream",
            get(stream_thread_events),
        )
        .route("/v1/runs/:run_id/events", get(list_run_events))
        .route("/v1/runs/:run_id/events/stream", get(stream_run_events))
}

#[derive(Debug, Deserialize)]
struct EventListParams {
    cursor: Option<String>,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

const fn default_event_limit() -> usize {
    DEFAULT_EVENT_LIMIT
}

#[tracing::instrument(skip(state), fields(thread_id = %thread_id))]
async fn list_thread_events(
    State(state): State<EventModuleState>,
    Path(thread_id): Path<String>,
    Query(params): Query<EventListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list_events_for_scope(state, EventScope::thread(thread_id), params).await
}

#[tracing::instrument(skip(state), fields(run_id = %run_id))]
async fn list_run_events(
    State(state): State<EventModuleState>,
    Path(run_id): Path<String>,
    Query(params): Query<EventListParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list_events_for_scope(state, EventScope::run(run_id), params).await
}

async fn list_events_for_scope(
    state: EventModuleState,
    scope: EventScope,
    params: EventListParams,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = Arc::clone(&state.event_store);
    let cursor = parse_cursor(params.cursor.as_deref())?;
    let page = store
        .list(
            scope.clone(),
            cursor,
            params.limit.clamp(1, MAX_EVENT_LIMIT),
        )
        .await
        .map_err(map_event_store_error)?;

    let items = page
        .events
        .iter()
        .map(CanonicalEventHttp::from)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "items": items,
        "next_cursor": page.next_cursor.map(|cursor| cursor.as_str().to_string()),
        "has_more": page.has_more,
    })))
}

#[tracing::instrument(skip(state, headers), fields(thread_id = %thread_id))]
async fn stream_thread_events(
    State(state): State<EventModuleState>,
    Path(thread_id): Path<String>,
    Query(params): Query<EventListParams>,
    headers: HeaderMap,
) -> Response {
    stream_events_for_scope(state, EventScope::thread(thread_id), params, headers).await
}

#[tracing::instrument(skip(state, headers), fields(run_id = %run_id))]
async fn stream_run_events(
    State(state): State<EventModuleState>,
    Path(run_id): Path<String>,
    Query(params): Query<EventListParams>,
    headers: HeaderMap,
) -> Response {
    stream_events_for_scope(state, EventScope::run(run_id), params, headers).await
}

async fn stream_events_for_scope(
    state: EventModuleState,
    scope: EventScope,
    params: EventListParams,
    headers: HeaderMap,
) -> Response {
    let store = Arc::clone(&state.event_store);
    let cursor = match stream_cursor(&params, &headers) {
        Ok(cursor) => cursor,
        Err(error) => return error.into_response(),
    };
    let start = cursor.map_or(SubscribeStart::FromNow, SubscribeStart::FromCursor);
    let handle = match store.subscribe(scope.clone(), start).await {
        Ok(handle) => handle,
        Err(error) => return map_event_store_error(error).into_response(),
    };

    let stream = async_stream::stream! {
        let mut events = handle.stream;
        while let Some(item) = events.next().await {
            match item.and_then(|event| format_canonical_event_sse(&event, &scope)) {
                Ok(frame) => yield Ok::<Bytes, Infallible>(frame),
                Err(error) => {
                    yield Ok::<Bytes, Infallible>(format_stream_error(&error));
                    break;
                }
            }
        }
    };
    sse_response(stream)
}

fn stream_cursor(
    params: &EventListParams,
    headers: &HeaderMap,
) -> Result<Option<EventCursor>, ApiError> {
    if params.cursor.is_some() {
        return parse_cursor(params.cursor.as_deref());
    }
    let header_cursor = headers
        .get(LAST_EVENT_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    parse_cursor(header_cursor)
}

fn parse_cursor(raw: Option<&str>) -> Result<Option<EventCursor>, ApiError> {
    raw.map(|value| {
        EventCursor::new(value.to_string()).map_err(|error| ApiError::BadRequest(error.to_string()))
    })
    .transpose()
}

fn map_event_store_error(error: EventStoreError) -> ApiError {
    match error {
        EventStoreError::Validation(message) => ApiError::BadRequest(message),
        EventStoreError::IdempotencyConflict(message)
        | EventStoreError::ExpectedCursorConflict(message) => ApiError::Conflict(message),
        EventStoreError::CursorExpired(message) => ApiError::Gone(message),
        EventStoreError::Integrity(message)
        | EventStoreError::Io(message)
        | EventStoreError::Serialization(message) => ApiError::Internal(message),
    }
}

fn format_canonical_event_sse(
    event: &CanonicalEvent,
    scope: &EventScope,
) -> Result<Bytes, EventStoreError> {
    let cursor = event.cursors_by_scope.get(scope).ok_or_else(|| {
        EventStoreError::Integrity("event missing cursor for requested scope".to_string())
    })?;
    if cursor.as_str().contains(['\r', '\n']) {
        return Err(EventStoreError::Integrity(
            "event cursor contains a newline".to_string(),
        ));
    }
    let payload = serde_json::to_string(&CanonicalEventHttp::from(event))
        .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
    Ok(Bytes::from(format!(
        "id: {}\ndata: {}\n\n",
        cursor.as_str(),
        payload
    )))
}

fn format_stream_error(error: &EventStoreError) -> Bytes {
    let payload = serde_json::to_string(&json!({ "error": error.to_string() }))
        .unwrap_or_else(|_| r#"{"error":"event stream error"}"#.to_string());
    Bytes::from(format!("event: error\ndata: {payload}\n\n"))
}

#[derive(Debug, Serialize)]
struct CanonicalEventHttp {
    event_id: String,
    scopes: Vec<EventScope>,
    cursors_by_scope: Vec<ScopedCursorHttp>,
    event_kind: String,
    #[serde(default)]
    payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    causation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correlation_id: Option<String>,
    origin: String,
    visibility: EventVisibility,
    schema_version: u32,
    created_at: u64,
}

#[derive(Debug, Serialize)]
struct ScopedCursorHttp {
    scope: EventScope,
    cursor: String,
}

impl From<&CanonicalEvent> for CanonicalEventHttp {
    fn from(event: &CanonicalEvent) -> Self {
        Self {
            event_id: event.event_id.as_str().to_string(),
            scopes: event.scopes.clone(),
            cursors_by_scope: event
                .cursors_by_scope
                .iter()
                .map(|(scope, cursor)| ScopedCursorHttp {
                    scope: scope.clone(),
                    cursor: cursor.as_str().to_string(),
                })
                .collect(),
            event_kind: event.event_kind.as_str().to_string(),
            payload: event.payload.clone(),
            thread_id: event.thread_id.clone(),
            run_id: event.run_id.clone(),
            causation_id: event.causation_id.clone(),
            correlation_id: event.correlation_id.clone(),
            origin: event.origin.clone(),
            visibility: event.visibility,
            schema_version: event.schema_version,
            created_at: event.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event_store::{
        AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventWriter,
    };
    use awaken_runtime::AgentRuntime;
    use awaken_stores::{InMemoryEventStore, InMemoryMailboxStore, InMemoryStore};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::app::{ServerConfig, ServerState};
    use crate::mailbox::{Mailbox, MailboxConfig};
    use crate::routes::build_router;

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

    fn make_state(event_store: Option<Arc<InMemoryEventStore>>) -> ServerState {
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
        let mut state = ServerState::new(
            runtime,
            mailbox,
            store,
            Arc::new(StubResolver),
            ServerConfig::default(),
        );
        if let Some(store) = event_store {
            state.events = Some(crate::app::EventModuleState { event_store: store });
        }
        state
    }

    async fn append_event(store: &InMemoryEventStore, thread_id: &str, kind: &str) -> EventCursor {
        let scope = EventScope::thread(thread_id);
        append_scoped_event(store, vec![scope.clone()], kind)
            .await
            .cursors_by_scope
            .get(&scope)
            .cloned()
            .unwrap()
    }

    async fn append_scoped_event(
        store: &InMemoryEventStore,
        scopes: Vec<EventScope>,
        kind: &str,
    ) -> CanonicalEvent {
        let draft = CanonicalEventDraft::new(
            scopes,
            CanonicalEventKind::new(kind).unwrap(),
            json!({ "kind": kind }),
            "test",
        )
        .unwrap();
        store
            .append(draft, AppendOptions::default())
            .await
            .unwrap()
            .event
    }

    async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn list_thread_events_uses_event_store_cursors() {
        let event_store = Arc::new(InMemoryEventStore::new());
        let first_cursor = append_event(&event_store, "thread-events", "RunStarted").await;
        append_event(&event_store, "thread-events", "RunFinished").await;
        let state = make_state(Some(event_store));
        let app = build_router(&state);

        let (status, body) = get(app.clone(), "/v1/threads/thread-events/events?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["has_more"].as_bool(), Some(true));
        assert_eq!(body["next_cursor"].as_str(), Some(first_cursor.as_str()));

        let uri = format!(
            "/v1/threads/thread-events/events?cursor={}",
            first_cursor.as_str()
        );
        let (status, body) = get(app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["event_kind"], "RunFinished");
    }

    #[tokio::test]
    async fn list_run_events_use_scoped_event_store_cursors() {
        let event_store = Arc::new(InMemoryEventStore::new());
        let event = append_scoped_event(
            &event_store,
            vec![EventScope::thread("child-thread"), EventScope::run("run-1")],
            "RunStarted",
        )
        .await;
        let run_cursor = event
            .cursors_by_scope
            .get(&EventScope::run("run-1"))
            .unwrap();
        let state = make_state(Some(event_store));
        let app = build_router(&state);

        let (status, body) = get(app, "/v1/runs/run-1/events").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["items"][0]["cursors_by_scope"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cursor| cursor["cursor"].as_str() == Some(run_cursor.as_str()))
        );
    }

    #[tokio::test]
    async fn list_thread_events_route_absent_without_event_module() {
        let state = make_state(None);
        let app = build_router(&state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/threads/thread-events/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_thread_events_returns_gone_for_expired_cursor() {
        let event_store = Arc::new(InMemoryEventStore::new());
        let state = make_state(Some(event_store));
        let app = build_router(&state);

        let (status, body) = get(app, "/v1/threads/thread-events/events?cursor=unknown").await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body["error"], "unknown");
    }

    #[tokio::test]
    async fn stream_thread_events_replays_after_last_event_id() {
        let event_store = Arc::new(InMemoryEventStore::new());
        let first_cursor = append_event(&event_store, "thread-events", "RunStarted").await;
        let second_cursor = append_event(&event_store, "thread-events", "RunFinished").await;
        let state = make_state(Some(event_store));
        let app = build_router(&state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/threads/thread-events/events/stream")
                    .header(LAST_EVENT_ID_HEADER, first_cursor.as_str())
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
        let mut body = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = String::from_utf8(frame.to_vec()).unwrap();
        assert!(frame.starts_with(&format!("id: {}\n", second_cursor.as_str())));
        assert!(frame.contains("\"event_kind\":\"RunFinished\""));
    }
}
