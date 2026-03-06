use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tirea_agentos::contracts::storage::{
    MessagePage, MessageQuery, SortOrder, ThreadReader, ThreadStore, ThreadStoreError,
};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::thread::Visibility;
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::contracts::ToolCallDecision;
use tirea_agentos::orchestrator::{AgentOs, AgentOsRunError, ResolvedRun};
use tirea_agentos::runtime::loop_runner::RunCancellationToken;
use tirea_contract::storage::RunRecord;
use tirea_contract::{AgentEvent, Identity, RuntimeInput};
use tokio::sync::{mpsc, RwLock};

use crate::run_service::{global_run_service, origin_from_protocol, wrap_with_run_tracking};
use crate::transport::http_run::wire_http_sse_relay;
use crate::transport::runtime_endpoint::RunStarter;
use crate::transport::TransportError;

#[derive(Clone)]
pub struct AppState {
    pub os: Arc<AgentOs>,
    pub read_store: Arc<dyn ThreadReader>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("thread not found: {0}")]
    ThreadNotFound(String),

    #[error("run not found: {0}")]
    RunNotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match &self {
            ApiError::AgentNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::ThreadNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::RunNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        let body = Json(serde_json::json!({ "error": msg }));
        (code, body).into_response()
    }
}

impl From<AgentOsRunError> for ApiError {
    fn from(e: AgentOsRunError) -> Self {
        match e {
            AgentOsRunError::Resolve(
                tirea_agentos::orchestrator::AgentOsResolveError::AgentNotFound(id),
            ) => ApiError::AgentNotFound(id),
            AgentOsRunError::Resolve(other) => ApiError::BadRequest(other.to_string()),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

#[derive(Default)]
struct ActiveRunRegistry {
    senders: RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<RuntimeInput>>>,
    run_id_to_key: RwLock<HashMap<String, String>>,
    cancellation_tokens: RwLock<HashMap<String, RunCancellationToken>>,
}

impl ActiveRunRegistry {
    fn run_id_from_key(key: &str) -> Option<String> {
        let run_id = key.rsplit(':').next()?.trim();
        if run_id.is_empty() {
            None
        } else {
            Some(run_id.to_string())
        }
    }

    async fn register(
        &self,
        key: String,
        run_id: Option<String>,
        tx: tokio::sync::mpsc::UnboundedSender<RuntimeInput>,
    ) {
        if let Some(run_id) = run_id.or_else(|| Self::run_id_from_key(&key)) {
            self.run_id_to_key.write().await.insert(run_id, key.clone());
        }
        self.senders.write().await.insert(key, tx);
    }

    async fn register_cancellation_token(&self, run_id: String, token: RunCancellationToken) {
        self.cancellation_tokens.write().await.insert(run_id, token);
    }

    async fn sender_for(
        &self,
        key: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<RuntimeInput>> {
        self.senders.read().await.get(key).cloned()
    }

    async fn sender_for_run_id(
        &self,
        run_id: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<RuntimeInput>> {
        let key = self.run_id_to_key.read().await.get(run_id).cloned()?;
        self.sender_for(&key).await
    }

    async fn cancellation_token_for_run_id(&self, run_id: &str) -> Option<RunCancellationToken> {
        self.cancellation_tokens.read().await.get(run_id).cloned()
    }

    async fn remove(&self, key: &str) {
        self.senders.write().await.remove(key);
        if let Some(run_id) = Self::run_id_from_key(key) {
            self.run_id_to_key.write().await.remove(&run_id);
            self.cancellation_tokens.write().await.remove(&run_id);
        }
    }

    async fn remove_by_run_id(&self, run_id: &str) {
        if let Some(key) = self.run_id_to_key.write().await.remove(run_id) {
            self.senders.write().await.remove(&key);
        }
        self.cancellation_tokens.write().await.remove(run_id);
    }
}

static ACTIVE_RUN_REGISTRY: OnceLock<ActiveRunRegistry> = OnceLock::new();

fn active_run_registry() -> &'static ActiveRunRegistry {
    ACTIVE_RUN_REGISTRY.get_or_init(ActiveRunRegistry::default)
}

pub fn active_run_key(protocol: &str, agent_id: &str, thread_id: &str, run_id: &str) -> String {
    format!("{protocol}:{agent_id}:{thread_id}:{run_id}")
}

pub async fn register_active_run(
    key: String,
    tx: tokio::sync::mpsc::UnboundedSender<RuntimeInput>,
) {
    active_run_registry().register(key, None, tx).await;
}

pub async fn register_active_run_with_id(
    key: String,
    run_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<RuntimeInput>,
) {
    active_run_registry().register(key, Some(run_id), tx).await;
}

pub async fn register_active_run_cancellation(run_id: String, token: RunCancellationToken) {
    active_run_registry()
        .register_cancellation_token(run_id, token)
        .await;
}

pub async fn remove_active_run(key: &str) {
    active_run_registry().remove(key).await;
}

pub async fn try_forward_decisions_to_active_run(
    active_key: &str,
    decisions: Vec<ToolCallDecision>,
) -> bool {
    if decisions.is_empty() {
        return false;
    }

    let Some(tx) = active_run_registry().sender_for(active_key).await else {
        return false;
    };

    for decision in decisions {
        if tx.send(RuntimeInput::Decision(decision)).is_err() {
            active_run_registry().remove(active_key).await;
            return false;
        }
    }

    true
}

pub async fn try_forward_decisions_to_active_run_by_id(
    run_id: &str,
    decisions: Vec<ToolCallDecision>,
) -> bool {
    if decisions.is_empty() {
        return false;
    }

    let Some(tx) = active_run_registry().sender_for_run_id(run_id).await else {
        return false;
    };

    for decision in decisions {
        if tx.send(RuntimeInput::Decision(decision)).is_err() {
            active_run_registry().remove_by_run_id(run_id).await;
            return false;
        }
    }

    true
}

pub async fn try_cancel_active_run_by_id(run_id: &str) -> bool {
    let token = active_run_registry()
        .cancellation_token_for_run_id(run_id)
        .await;
    let sender = active_run_registry().sender_for_run_id(run_id).await;

    let mut cancelled = false;
    if let Some(token) = token {
        token.cancel();
        cancelled = true;
    }

    if let Some(tx) = sender {
        if tx.send(RuntimeInput::Cancel).is_err() {
            active_run_registry().remove_by_run_id(run_id).await;
            return cancelled;
        }
        cancelled = true;
    }

    cancelled
}

fn default_message_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
pub struct MessageQueryParams {
    #[serde(default)]
    pub after: Option<i64>,
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default = "default_message_limit")]
    pub limit: usize,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

pub fn parse_message_query(params: &MessageQueryParams) -> MessageQuery {
    let limit = params.limit.clamp(1, 200);
    let order = match params.order.as_deref() {
        Some("desc") => SortOrder::Desc,
        _ => SortOrder::Asc,
    };
    let visibility = match params.visibility.as_deref() {
        Some("internal") => Some(Visibility::Internal),
        Some("none") => None,
        _ => Some(Visibility::All),
    };
    MessageQuery {
        after: params.after,
        before: params.before,
        limit,
        order,
        visibility,
        run_id: params.run_id.clone(),
    }
}

pub async fn load_message_page(
    read_store: &Arc<dyn ThreadReader>,
    thread_id: &str,
    params: &MessageQueryParams,
) -> Result<MessagePage, ApiError> {
    let query = parse_message_query(params);
    read_store
        .load_messages(thread_id, &query)
        .await
        .map_err(|e| match e {
            ThreadStoreError::NotFound(_) => ApiError::ThreadNotFound(thread_id.to_string()),
            other => ApiError::Internal(other.to_string()),
        })
}

#[derive(Debug, Serialize)]
pub struct EncodedMessagePage<M: Serialize> {
    pub messages: Vec<M>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<i64>,
}

pub fn encode_message_page<M: Serialize>(
    page: MessagePage,
    encode: impl Fn(&Message) -> M,
) -> EncodedMessagePage<M> {
    EncodedMessagePage {
        messages: page.messages.iter().map(|m| encode(&m.message)).collect(),
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        prev_cursor: page.prev_cursor,
    }
}

pub fn require_agent_state_store(os: &Arc<AgentOs>) -> Result<Arc<dyn ThreadStore>, ApiError> {
    os.agent_state_store()
        .cloned()
        .ok_or_else(|| ApiError::Internal("agent state store not configured".to_string()))
}

/// Shared HTTP run preparation: creates a cancellation token, calls `prepare_run`,
/// builds a `RunStarter`, sets up the ingress channel, and registers the active run.
pub struct PreparedHttpRun {
    pub starter: RunStarter,
    pub thread_id: String,
    pub run_id: String,
    pub cancellation_token: RunCancellationToken,
    pub ingress_rx: mpsc::UnboundedReceiver<RuntimeInput>,
    pub active_key: String,
}

pub async fn prepare_http_run(
    os: &Arc<AgentOs>,
    resolved: ResolvedRun,
    run_request: RunRequest,
    protocol_label: &str,
    agent_id: &str,
) -> Result<PreparedHttpRun, ApiError> {
    let parent_run_id = run_request.parent_run_id.clone();
    let parent_thread_id = run_request.parent_thread_id.clone();
    let cancellation_token = RunCancellationToken::new();
    let prepared = os.prepare_run(run_request.clone(), resolved).await?;
    let thread_id = prepared.thread_id.clone();
    let run_id = prepared.run_id.clone();

    let token_for_starter = cancellation_token.clone();
    let starter: RunStarter = Box::new(move |_request| {
        Box::pin(async move {
            let run = AgentOs::execute_prepared(
                prepared.with_cancellation_token(token_for_starter.clone()),
            )
            .map_err(|e| TransportError::Internal(e.to_string()))?;
            Ok((run, Some(token_for_starter)))
        })
    });

    let active_key = active_run_key(protocol_label, agent_id, &thread_id, &run_id);
    let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();
    ingress_tx
        .send(RuntimeInput::Run(run_request))
        .expect("ingress channel just created");
    register_active_run_with_id(active_key.clone(), run_id.clone(), ingress_tx).await;
    register_active_run_cancellation(run_id.clone(), cancellation_token.clone()).await;
    if let Some(service) = global_run_service() {
        let _ = service
            .begin_intent(
                &run_id,
                &thread_id,
                origin_from_protocol(protocol_label),
                parent_run_id,
                parent_thread_id,
            )
            .await;
    }

    Ok(PreparedHttpRun {
        starter,
        thread_id,
        run_id,
        cancellation_token,
        ingress_rx,
        active_key,
    })
}

// ---------------------------------------------------------------------------
// Shared run-control helpers (used by run_api and a2a)
// ---------------------------------------------------------------------------

/// Check whether a run record exists in the persistent run store.
pub async fn run_exists(run_id: &str) -> Result<bool, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    Ok(service
        .get_run(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .is_some())
}

/// Load the full [`RunRecord`] for a given run id.
pub async fn load_run_record(run_id: &str) -> Result<Option<RunRecord>, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    service
        .get_run(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Resolve the thread id that a run belongs to.
pub async fn resolve_thread_id_from_run(run_id: &str) -> Result<Option<String>, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    service
        .resolve_thread_id(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Result of checking whether a run is currently active.
pub enum RunLookup {
    ExistsButInactive,
    NotFound,
}

/// After an active-run operation (cancel/forward) fails, check if the run
/// exists in the persistent store. Returns [`RunLookup::ExistsButInactive`]
/// or [`RunLookup::NotFound`].
pub async fn check_run_liveness(run_id: &str) -> Result<RunLookup, ApiError> {
    if run_exists(run_id).await? {
        Ok(RunLookup::ExistsButInactive)
    } else {
        Ok(RunLookup::NotFound)
    }
}

/// Resolve an agent, prepare a run, wire an SSE relay that is immediately
/// drained in the background, and return `(thread_id, run_id)`.
///
/// This is the shared "fire-and-forget" background run launcher used by both
/// the run API and the A2A gateway.
pub async fn start_background_run(
    os: &Arc<AgentOs>,
    agent_id: &str,
    run_request: RunRequest,
    protocol_label: &'static str,
) -> Result<(String, String), ApiError> {
    let resolved = os.resolve(agent_id).map_err(AgentOsRunError::from)?;
    let prepared =
        prepare_http_run(os, resolved, run_request, protocol_label, agent_id).await?;
    let run_id = prepared.run_id.clone();
    let thread_id = prepared.thread_id.clone();
    let active_key = prepared.active_key.clone();
    let thread_for_session = thread_id.clone();

    let encoder = wrap_with_run_tracking(
        Identity::<AgentEvent>::default(),
        run_id.clone(),
        thread_id.clone(),
        protocol_label,
    );
    let mut sse_rx = wire_http_sse_relay(
        prepared.starter,
        encoder,
        prepared.ingress_rx,
        thread_for_session,
        None,
        false,
        protocol_label,
        move |_sse_tx| async move {
            remove_active_run(&active_key).await;
        },
        |msg| {
            let error = serde_json::json!({
                "type": "error",
                "message": msg,
                "code": "RELAY_ERROR",
            });
            let payload = serde_json::to_string(&error).unwrap_or_else(|_| {
                "{\"type\":\"error\",\"message\":\"relay error\",\"code\":\"RELAY_ERROR\"}"
                    .to_string()
            });
            Bytes::from(format!("data: {payload}\n\n"))
        },
    );
    tokio::spawn(async move { while sse_rx.recv().await.is_some() {} });

    Ok((thread_id, run_id))
}

/// Truncate a stored thread so it includes messages up to and including `message_id`.
pub async fn truncate_thread_at_message(
    os: &Arc<AgentOs>,
    thread_id: &str,
    message_id: &str,
) -> Result<(), ApiError> {
    let store = require_agent_state_store(os)?;
    let mut thread = store
        .load(thread_id)
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("thread not found for regenerate-message".to_string()))?
        .thread;
    let position = thread
        .messages
        .iter()
        .position(|m| m.id.as_deref() == Some(message_id))
        .ok_or_else(|| {
            ApiError::BadRequest("messageId does not reference a stored message".to_string())
        })?;
    thread.messages.truncate(position + 1);
    store
        .save(&thread)
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_run_key_includes_protocol_agent_thread_run() {
        assert_eq!(
            active_run_key("ag_ui", "assistant", "thread-1", "run-1"),
            "ag_ui:assistant:thread-1:run-1"
        );
    }

    #[test]
    fn parse_message_query_defaults_and_visibility() {
        let params = MessageQueryParams {
            after: None,
            before: None,
            limit: 999,
            order: None,
            visibility: None,
            run_id: None,
        };
        let query = parse_message_query(&params);
        assert_eq!(query.limit, 200);
        assert!(matches!(query.order, SortOrder::Asc));
        assert!(matches!(query.visibility, Some(Visibility::All)));

        let params = MessageQueryParams {
            after: None,
            before: None,
            limit: 1,
            order: Some("desc".to_string()),
            visibility: Some("internal".to_string()),
            run_id: Some("r1".to_string()),
        };
        let query = parse_message_query(&params);
        assert_eq!(query.limit, 1);
        assert!(matches!(query.order, SortOrder::Desc));
        assert!(matches!(query.visibility, Some(Visibility::Internal)));
        assert_eq!(query.run_id.as_deref(), Some("r1"));
    }
}
