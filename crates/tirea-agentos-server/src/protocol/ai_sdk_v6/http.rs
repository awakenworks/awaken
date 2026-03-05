use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::OnceLock;
use tirea_agentos::orchestrator::AgentOsRunError;
use tirea_protocol_ai_sdk_v6::{
    AiSdkEncoder, AiSdkTrigger, AiSdkV6HistoryEncoder, AiSdkV6RunRequest, UIStreamEvent,
    AI_SDK_VERSION,
};

use super::runtime::apply_ai_sdk_extensions;
use tokio::sync::{broadcast, RwLock};

use crate::run_service::wrap_with_run_tracking;
use crate::service::{
    active_run_key, encode_message_page, load_message_page, prepare_http_run, remove_active_run,
    truncate_thread_at_message, try_forward_decisions_to_active_run, ApiError, AppState,
    MessageQueryParams,
};
use crate::transport::http_run::wire_http_sse_relay;
use crate::transport::http_sse::{sse_body_stream, sse_response};

const RUN_PATH: &str = "/agents/:agent_id/runs";
const RESUME_STREAM_PATH: &str = "/agents/:agent_id/runs/:chat_id/stream";
const THREAD_MESSAGES_PATH: &str = "/threads/:id/messages";

/// Build AI SDK v6 HTTP routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(RUN_PATH, post(run))
        .route(RESUME_STREAM_PATH, get(resume_stream))
        .route(THREAD_MESSAGES_PATH, get(thread_messages))
}

#[derive(Default)]
struct StreamRegistry {
    streams: RwLock<HashMap<String, broadcast::Sender<Bytes>>>,
}

impl StreamRegistry {
    async fn register(&self, key: String) -> broadcast::Sender<Bytes> {
        let (tx, _) = broadcast::channel(128);
        self.streams.write().await.insert(key, tx.clone());
        tx
    }

    async fn subscribe(&self, key: &str) -> Option<broadcast::Receiver<Bytes>> {
        self.streams
            .read()
            .await
            .get(key)
            .map(|sender| sender.subscribe())
    }

    async fn remove(&self, key: &str) {
        self.streams.write().await.remove(key);
    }
}

/// Process-wide registry for SSE stream fanout (resume_stream endpoint).
///
/// Lifecycle: entries are registered in `run()` and unconditionally removed
/// in the `on_relay_done` callback, which runs even on cancellation.
static STREAM_REGISTRY: OnceLock<StreamRegistry> = OnceLock::new();

fn stream_registry() -> &'static StreamRegistry {
    STREAM_REGISTRY.get_or_init(StreamRegistry::default)
}

fn stream_key(agent_id: &str, chat_id: &str) -> String {
    format!("{agent_id}:{chat_id}")
}

async fn thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page = load_message_page(&st.read_store, &id, &params).await?;
    let encoded = encode_message_page(page, AiSdkV6HistoryEncoder::encode_message);
    Ok(Json(encoded))
}

async fn run(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AiSdkV6RunRequest>,
) -> Result<Response, ApiError> {
    req.validate().map_err(ApiError::BadRequest)?;
    if req.trigger == Some(AiSdkTrigger::RegenerateMessage) {
        truncate_thread_at_message(&st.os, &req.thread_id, req.message_id.as_deref().unwrap())
            .await?;
    }

    let suspension_decisions = req.suspension_decisions();
    let decision_only = !req.has_user_input() && !suspension_decisions.is_empty();
    if decision_only {
        if let Some(run_id) = req
            .run_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            let key = active_run_key("ai_sdk", &agent_id, &req.thread_id, run_id);
            if try_forward_decisions_to_active_run(&key, suspension_decisions).await {
                return Ok((
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({
                        "status": "decision_forwarded",
                        "threadId": req.thread_id,
                    })),
                )
                    .into_response());
            }
        }
    }

    let mut resolved = st.os.resolve(&agent_id).map_err(AgentOsRunError::from)?;
    apply_ai_sdk_extensions(&mut resolved, &req);
    let run_request = req.into_runtime_run_request(agent_id.clone());

    let prepared = prepare_http_run(&st.os, resolved, run_request, "ai_sdk", &agent_id).await?;
    let token_for_callback = prepared.cancellation_token.clone();
    let stream_key = stream_key(&agent_id, &prepared.thread_id);
    let active_key = prepared.active_key.clone();
    let fanout = stream_registry().register(stream_key.clone()).await;

    let encoder = wrap_with_run_tracking(
        AiSdkEncoder::new(),
        prepared.run_id.clone(),
        prepared.thread_id.clone(),
        "ai_sdk",
    );
    let sse_rx = wire_http_sse_relay(
        prepared.starter,
        encoder,
        prepared.ingress_rx,
        prepared.thread_id,
        Some(fanout.clone()),
        true,
        "ai-sdk",
        move |sse_tx| async move {
            let trailer = Bytes::from("data: [DONE]\n\n");
            let _ = fanout.send(trailer.clone());
            if sse_tx.send(trailer).await.is_err() {
                token_for_callback.cancel();
            }
            stream_registry().remove(&stream_key).await;
            remove_active_run(&active_key).await;
        },
        |msg| {
            let json = serde_json::to_string(&UIStreamEvent::error(&msg)).unwrap_or_default();
            Bytes::from(format!("data: {json}\n\n"))
        },
    );

    Ok(ai_sdk_sse_response(sse_body_stream(sse_rx)))
}

async fn resume_stream(Path((agent_id, chat_id)): Path<(String, String)>) -> Response {
    let key = stream_key(&agent_id, &chat_id);
    let Some(mut receiver) = stream_registry().subscribe(&key).await else {
        return StatusCode::NO_CONTENT.into_response();
    };

    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(chunk) => yield Ok::<Bytes, Infallible>(chunk),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    ai_sdk_sse_response(stream)
}

fn ai_sdk_sse_response<S>(stream: S) -> Response
where
    S: futures::Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
{
    let mut response = sse_response(stream);
    response.headers_mut().insert(
        header::HeaderName::from_static("x-vercel-ai-ui-message-stream"),
        HeaderValue::from_static("v1"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-tirea-ai-sdk-version"),
        HeaderValue::from_static(AI_SDK_VERSION),
    );
    response
}
