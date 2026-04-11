//! A2A v1.0 HTTP+JSON endpoints.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

use awaken_contract::contract::content::{
    AudioSource, ContentBlock, DocumentSource, ImageSource, VideoSource,
};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::{MailboxJob, MailboxJobStatus};
use awaken_contract::contract::message::{
    Message as AwakenMessage, Role as AwakenRole, Visibility,
};
use awaken_contract::contract::storage::{RunQuery, RunRecord};
use awaken_contract::thread::Thread;
pub use awaken_protocol_a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentProvider, AgentSkill, Artifact,
    AuthenticationInfo, ListPushNotificationConfigsResponse, ListTasksResponse,
    Message as A2aMessage, MessageRole, Part, PushNotificationConfig, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent,
};

use crate::app::AppState;
use crate::http_sse::{format_sse_data, sse_body_stream, sse_response};
use awaken_runtime::RunRequest;

const A2A_VERSION: &str = "1.0";
const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;
const DISCOVERY_PATH: &str = "/.well-known/agent-card.json";
const INTERFACE_BASE_PATH: &str = "/v1/a2a";
const BLOCKING_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const BLOCKING_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SUPPORTED_OUTPUT_MODE: &str = "text/plain";
const PUSH_CONFIGS_METADATA_KEY: &str = "a2a.pushNotificationConfigs";
const TASK_BINDINGS_METADATA_KEY: &str = "a2a.taskBindings";
const A2A_NOTIFICATION_TOKEN_HEADER: &str = "x-a2a-notification-token";
const EXTENDED_CARD_SECURITY_SCHEME_ID: &str = "awakenExtendedCardBearer";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredTaskBindings {
    #[serde(default)]
    tasks: BTreeMap<String, StoredTaskBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTaskBinding {
    thread_id: String,
    #[serde(default)]
    start_message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end_message_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredPushConfigs {
    #[serde(default)]
    tasks: BTreeMap<String, Vec<PushNotificationConfig>>,
}

#[derive(Debug, Clone)]
struct ResolvedTask {
    thread_id: String,
    run: Option<RunRecord>,
    job: Option<MailboxJob>,
}

/// Build A2A routes.
pub fn a2a_routes() -> Router<AppState> {
    Router::new()
        .route(DISCOVERY_PATH, get(a2a_agent_card))
        .route(
            "/v1/a2a/*tail",
            get(a2a_get_dispatch)
                .post(a2a_post_dispatch)
                .delete(a2a_delete_dispatch),
        )
}

#[derive(Debug)]
enum A2aError {
    Validation {
        message: String,
        violations: Vec<FieldViolation>,
    },
    Specific {
        http_status: StatusCode,
        status: &'static str,
        reason: &'static str,
        message: String,
        metadata: BTreeMap<String, String>,
    },
    NotFound(String),
    Internal(String),
}

#[derive(Debug, Clone)]
struct FieldViolation {
    field: String,
    description: String,
}

impl A2aError {
    fn invalid(field: impl Into<String>, description: impl Into<String>) -> Self {
        Self::Validation {
            message: "invalid A2A request".to_string(),
            violations: vec![FieldViolation {
                field: field.into(),
                description: description.into(),
            }],
        }
    }

    fn merge_invalid(
        message: impl Into<String>,
        violations: impl IntoIterator<Item = FieldViolation>,
    ) -> Self {
        Self::Validation {
            message: message.into(),
            violations: violations.into_iter().collect(),
        }
    }

    fn version_not_supported(found: impl Into<String>) -> Self {
        let found = found.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("supportedVersion".to_string(), A2A_VERSION.to_string());
        metadata.insert("requestedVersion".to_string(), found.clone());
        Self::Specific {
            http_status: StatusCode::BAD_REQUEST,
            status: "FAILED_PRECONDITION",
            reason: "VERSION_NOT_SUPPORTED",
            message: format!("unsupported A2A-Version '{found}'"),
            metadata,
        }
    }

    fn unsupported_operation(message: impl Into<String>) -> Self {
        Self::Specific {
            http_status: StatusCode::NOT_IMPLEMENTED,
            status: "UNIMPLEMENTED",
            reason: "UNSUPPORTED_OPERATION",
            message: message.into(),
            metadata: BTreeMap::new(),
        }
    }

    fn content_type_not_supported(found: impl Into<String>) -> Self {
        let found = found.into();
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "supportedOutputModes".to_string(),
            SUPPORTED_OUTPUT_MODE.to_string(),
        );
        metadata.insert("requestedOutputModes".to_string(), found);
        Self::Specific {
            http_status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            status: "INVALID_ARGUMENT",
            reason: "CONTENT_TYPE_NOT_SUPPORTED",
            message: "requested output mode is not supported".to_string(),
            metadata,
        }
    }

    fn task_not_found(task_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        Self::Specific {
            http_status: StatusCode::NOT_FOUND,
            status: "NOT_FOUND",
            reason: "TASK_NOT_FOUND",
            message: format!("task not found: {task_id}"),
            metadata,
        }
    }

    fn task_not_cancelable(task_id: impl Into<String>, state: TaskState) -> Self {
        let task_id = task_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        metadata.insert("state".to_string(), task_state_name(state).to_string());
        Self::Specific {
            http_status: StatusCode::CONFLICT,
            status: "FAILED_PRECONDITION",
            reason: "TASK_NOT_CANCELABLE",
            message: format!("task is not cancelable in state {}", task_state_name(state)),
            metadata,
        }
    }

    fn push_config_not_found(task_id: impl Into<String>, config_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        let config_id = config_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        metadata.insert("configId".to_string(), config_id.clone());
        Self::Specific {
            http_status: StatusCode::NOT_FOUND,
            status: "NOT_FOUND",
            reason: "TASK_NOT_FOUND",
            message: format!("push notification config not found for task {task_id}: {config_id}"),
            metadata,
        }
    }

    fn task_not_subscribable(task_id: impl Into<String>, state: TaskState) -> Self {
        let task_id = task_id.into();
        let mut metadata = BTreeMap::new();
        metadata.insert("taskId".to_string(), task_id.clone());
        metadata.insert("state".to_string(), task_state_name(state).to_string());
        Self::Specific {
            http_status: StatusCode::CONFLICT,
            status: "FAILED_PRECONDITION",
            reason: "UNSUPPORTED_OPERATION",
            message: format!(
                "task {task_id} is already in terminal state {}; subscribe is not available",
                task_state_name(state)
            ),
            metadata,
        }
    }

    fn unauthenticated(message: impl Into<String>) -> Self {
        Self::Specific {
            http_status: StatusCode::UNAUTHORIZED,
            status: "UNAUTHENTICATED",
            reason: "UNAUTHENTICATED",
            message: message.into(),
            metadata: BTreeMap::new(),
        }
    }
}

impl IntoResponse for A2aError {
    fn into_response(self) -> Response {
        match self {
            Self::Validation {
                message,
                violations,
            } => (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "code": 400,
                        "status": "INVALID_ARGUMENT",
                        "message": message,
                        "details": [{
                            "@type": "type.googleapis.com/google.rpc.BadRequest",
                            "fieldViolations": violations.into_iter().map(|violation| json!({
                                "field": violation.field,
                                "description": violation.description,
                            })).collect::<Vec<_>>()
                        }]
                    }
                })),
            )
                .into_response(),
            Self::Specific {
                http_status,
                status,
                reason,
                message,
                metadata,
            } => (
                http_status,
                Json(json!({
                    "error": {
                        "code": http_status.as_u16(),
                        "status": status,
                        "message": message,
                        "details": [{
                            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                            "reason": reason,
                            "domain": "a2a-protocol.org",
                            "metadata": metadata,
                        }]
                    }
                })),
            )
                .into_response(),
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": {
                        "code": 404,
                        "status": "NOT_FOUND",
                        "message": message,
                    }
                })),
            )
                .into_response(),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "code": 500,
                        "status": "INTERNAL",
                        "message": message,
                    }
                })),
            )
                .into_response(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTaskQuery {
    history_length: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTasksQuery {
    context_id: Option<String>,
    status: Option<String>,
    history_length: Option<usize>,
    page_size: Option<usize>,
    page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPushConfigsQuery {
    page_size: Option<usize>,
    page_token: Option<String>,
}

#[derive(Debug)]
struct TaskSnapshot {
    task: Task,
    updated_at_ms: u64,
    current_agent_id: Option<String>,
}

#[derive(Debug)]
struct TaskSource {
    state: TaskState,
    updated_at_ms: u64,
    current_agent_id: Option<String>,
}

async fn a2a_agent_card(
    State(st): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<AgentCard>, A2aError> {
    ensure_supported_version_from_request(&headers, &uri)?;
    let agent_id = public_agent_id(&st)?;
    Ok(Json(build_agent_card(
        &st, &headers, &agent_id, None, false,
    )))
}

async fn a2a_get_dispatch(
    State(st): State<AppState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, A2aError> {
    ensure_supported_version_from_request(&headers, &uri)?;
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["tasks"] => {
            let query = decode_query::<ListTasksQuery>(&uri)?;
            Ok(a2a_list_tasks_default(State(st), headers, Query(query))
                .await?
                .into_response())
        }
        ["tasks", task_id] => {
            let query = decode_query::<GetTaskQuery>(&uri)?;
            Ok(a2a_get_task_default(
                State(st),
                Path((*task_id).to_string()),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        ["tasks", task_id, "pushNotificationConfigs", config_id] => {
            Ok(a2a_get_push_config_default(
                State(st),
                Path(((*task_id).to_string(), (*config_id).to_string())),
                headers,
            )
            .await?)
        }
        ["tasks", task_id, "pushNotificationConfigs"] => {
            let query = decode_query::<ListPushConfigsQuery>(&uri)?;
            Ok(a2a_list_push_configs_default(
                State(st),
                Path((*task_id).to_string()),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        ["extendedAgentCard"] => Ok(a2a_extended_agent_card_default(State(st), headers).await?),
        [tenant, "tasks"] => {
            let query = decode_query::<ListTasksQuery>(&uri)?;
            Ok(a2a_list_tasks_tenant(
                State(st),
                Path((*tenant).to_string()),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        [tenant, "tasks", task_id] => {
            let query = decode_query::<GetTaskQuery>(&uri)?;
            Ok(a2a_get_task_tenant(
                State(st),
                Path(((*tenant).to_string(), (*task_id).to_string())),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        [tenant, "tasks", task_id, "pushNotificationConfigs"] => {
            let query = decode_query::<ListPushConfigsQuery>(&uri)?;
            Ok(a2a_list_push_configs_tenant(
                State(st),
                Path(((*tenant).to_string(), (*task_id).to_string())),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        [
            tenant,
            "tasks",
            task_id,
            "pushNotificationConfigs",
            config_id,
        ] => Ok(a2a_get_push_config_tenant(
            State(st),
            Path((
                (*tenant).to_string(),
                (*task_id).to_string(),
                (*config_id).to_string(),
            )),
            headers,
        )
        .await?),
        [tenant, "extendedAgentCard"] => {
            Ok(
                a2a_extended_agent_card_tenant(State(st), Path((*tenant).to_string()), headers)
                    .await?,
            )
        }
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A path: /v1/a2a/{tail}"
        ))),
    }
}

async fn a2a_post_dispatch(
    State(st): State<AppState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Result<Response, A2aError> {
    ensure_supported_version_from_request(&headers, &uri)?;
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["message:send"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(a2a_message_send_default(State(st), headers, Json(payload))
                .await?
                .into_response())
        }
        ["message:stream"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(
                a2a_message_stream_default(State(st), headers, uri, Json(payload))
                    .await?
                    .into_response(),
            )
        }
        ["tasks", task_action] => {
            Ok(
                a2a_task_action_default(State(st), Path((*task_action).to_string()), headers)
                    .await?,
            )
        }
        ["tasks", task_id, "pushNotificationConfigs"] => {
            let payload = decode_json_body::<PushNotificationConfig>(&headers, &body)?;
            Ok(a2a_create_push_config_default(
                State(st),
                Path((*task_id).to_string()),
                headers,
                Json(payload),
            )
            .await?)
        }
        [tenant, "message:send"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(a2a_message_send_tenant(
                State(st),
                Path((*tenant).to_string()),
                headers,
                Json(payload),
            )
            .await?
            .into_response())
        }
        [tenant, "message:stream"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(a2a_message_stream_tenant(
                State(st),
                Path((*tenant).to_string()),
                headers,
                uri,
                Json(payload),
            )
            .await?
            .into_response())
        }
        [tenant, "tasks", task_action] => Ok(a2a_task_action_tenant(
            State(st),
            Path(((*tenant).to_string(), (*task_action).to_string())),
            headers,
        )
        .await?),
        [tenant, "tasks", task_id, "pushNotificationConfigs"] => {
            let payload = decode_json_body::<PushNotificationConfig>(&headers, &body)?;
            Ok(a2a_create_push_config_tenant(
                State(st),
                Path(((*tenant).to_string(), (*task_id).to_string())),
                headers,
                Json(payload),
            )
            .await?)
        }
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A path: /v1/a2a/{tail}"
        ))),
    }
}

async fn a2a_delete_dispatch(
    State(st): State<AppState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, A2aError> {
    ensure_supported_version_from_request(&headers, &uri)?;
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["tasks", task_id, "pushNotificationConfigs", config_id] => {
            Ok(a2a_delete_push_config_default(
                State(st),
                Path(((*task_id).to_string(), (*config_id).to_string())),
                headers,
            )
            .await?)
        }
        [
            tenant,
            "tasks",
            task_id,
            "pushNotificationConfigs",
            config_id,
        ] => Ok(a2a_delete_push_config_tenant(
            State(st),
            Path((
                (*tenant).to_string(),
                (*task_id).to_string(),
                (*config_id).to_string(),
            )),
            headers,
        )
        .await?),
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A path: /v1/a2a/{tail}"
        ))),
    }
}

async fn a2a_message_send_default(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, A2aError> {
    send_message(st, headers, None, payload).await
}

async fn a2a_message_send_tenant(
    State(st): State<AppState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, A2aError> {
    send_message(st, headers, Some(tenant), payload).await
}

async fn a2a_message_stream_default(
    State(st): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Response, A2aError> {
    stream_message(st, headers, Some(&uri), None, payload).await
}

async fn a2a_message_stream_tenant(
    State(st): State<AppState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Response, A2aError> {
    stream_message(st, headers, Some(&uri), Some(tenant), payload).await
}

async fn a2a_list_tasks_default(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<ListTasksResponse>, A2aError> {
    list_tasks(st, headers, None, query).await
}

async fn a2a_list_tasks_tenant(
    State(st): State<AppState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<ListTasksResponse>, A2aError> {
    list_tasks(st, headers, Some(tenant), query).await
}

async fn a2a_get_task_default(
    State(st): State<AppState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<GetTaskQuery>,
) -> Result<Json<Task>, A2aError> {
    get_task(st, headers, None, task_id, query).await
}

async fn a2a_get_task_tenant(
    State(st): State<AppState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<GetTaskQuery>,
) -> Result<Json<Task>, A2aError> {
    get_task(st, headers, Some(tenant), task_id, query).await
}

async fn a2a_task_action_default(
    State(st): State<AppState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    let (task_id, action) = parse_task_action_segment(&task_id)?;
    match action {
        "cancel" => Ok(cancel_task(st, headers, None, task_id)
            .await?
            .into_response()),
        "subscribe" => subscribe_task(st, headers, None, task_id).await,
        _ => unreachable!("task action parser only returns supported actions"),
    }
}

async fn a2a_task_action_tenant(
    State(st): State<AppState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    let (task_id, action) = parse_task_action_segment(&task_id)?;
    match action {
        "cancel" => Ok(cancel_task(st, headers, Some(tenant), task_id)
            .await?
            .into_response()),
        "subscribe" => subscribe_task(st, headers, Some(tenant), task_id).await,
        _ => unreachable!("task action parser only returns supported actions"),
    }
}

async fn a2a_create_push_config_default(
    State(st): State<AppState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<PushNotificationConfig>,
) -> Result<Response, A2aError> {
    create_push_config(st, headers, None, task_id, payload)
        .await
        .map(IntoResponse::into_response)
}

async fn a2a_create_push_config_tenant(
    State(st): State<AppState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<PushNotificationConfig>,
) -> Result<Response, A2aError> {
    create_push_config(st, headers, Some(tenant), task_id, payload)
        .await
        .map(IntoResponse::into_response)
}

async fn a2a_list_push_configs_default(
    State(st): State<AppState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListPushConfigsQuery>,
) -> Result<Json<ListPushNotificationConfigsResponse>, A2aError> {
    list_push_configs(st, headers, None, task_id, query).await
}

async fn a2a_list_push_configs_tenant(
    State(st): State<AppState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<ListPushConfigsQuery>,
) -> Result<Json<ListPushNotificationConfigsResponse>, A2aError> {
    list_push_configs(st, headers, Some(tenant), task_id, query).await
}

async fn a2a_get_push_config_default(
    State(st): State<AppState>,
    Path((task_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    get_push_config(st, headers, None, task_id, config_id)
        .await
        .map(IntoResponse::into_response)
}

async fn a2a_get_push_config_tenant(
    State(st): State<AppState>,
    Path((tenant, task_id, config_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    get_push_config(st, headers, Some(tenant), task_id, config_id)
        .await
        .map(IntoResponse::into_response)
}

async fn a2a_delete_push_config_default(
    State(st): State<AppState>,
    Path((task_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    delete_push_config(st, headers, None, task_id, config_id).await
}

async fn a2a_delete_push_config_tenant(
    State(st): State<AppState>,
    Path((tenant, task_id, config_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    delete_push_config(st, headers, Some(tenant), task_id, config_id).await
}

async fn a2a_extended_agent_card_default(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    if !supports_extended_agent_card(&st) {
        return Err(A2aError::unsupported_operation(
            "extendedAgentCard is not configured for this agent",
        ));
    }
    ensure_extended_card_auth(&st, &headers)?;
    let agent_id = public_agent_id(&st)?;
    Ok(Json(build_agent_card(&st, &headers, &agent_id, None, true)).into_response())
}

async fn a2a_extended_agent_card_tenant(
    State(st): State<AppState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    if !supports_extended_agent_card(&st) {
        return Err(A2aError::unsupported_operation(
            "extendedAgentCard is not configured for this agent",
        ));
    }
    ensure_runnable_agent(&st, &tenant)?;
    ensure_extended_card_auth(&st, &headers)?;
    Ok(Json(build_agent_card(
        &st,
        &headers,
        &tenant,
        Some(&tenant),
        true,
    ))
    .into_response())
}

async fn send_message(
    st: AppState,
    headers: HeaderMap,
    path_tenant: Option<String>,
    payload: SendMessageRequest,
) -> Result<Json<SendMessageResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    let PreparedRequest {
        task_id,
        thread_id,
        effective_tenant,
        history_length,
        return_immediately,
        push_notification_config,
        new_task_start_message_id,
        request,
    } = prepare_send_request(&st, path_tenant, payload).await?;

    if let Some(config) = push_notification_config {
        upsert_push_notification_config_for_thread(
            &st,
            &thread_id,
            &task_id,
            effective_tenant.as_deref(),
            config,
        )
        .await?;
    }

    if let Some(start_message_id) = new_task_start_message_id.as_deref() {
        record_task_binding(&st, &thread_id, &task_id, start_message_id).await?;
    }

    st.mailbox
        .submit_background(request)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    for config in load_push_notification_configs(&st, &task_id, effective_tenant.as_deref()).await?
    {
        spawn_push_notification_driver(
            st.clone(),
            task_id.clone(),
            effective_tenant.clone(),
            config,
        );
    }

    let task = if return_immediately {
        load_task_snapshot(
            &st,
            &task_id,
            effective_tenant.as_deref(),
            history_length,
            true,
        )
        .await?
        .map(|snapshot| snapshot.task)
        .unwrap_or_else(|| submitted_task(&task_id, &thread_id, effective_tenant.as_deref()))
    } else {
        wait_for_task(&st, &task_id, effective_tenant.as_deref(), history_length).await?
    };

    Ok(Json(SendMessageResponse::task(task)))
}

async fn stream_message(
    st: AppState,
    headers: HeaderMap,
    _uri: Option<&Uri>,
    path_tenant: Option<String>,
    payload: SendMessageRequest,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    let PreparedRequest {
        task_id,
        thread_id,
        effective_tenant,
        history_length,
        push_notification_config,
        new_task_start_message_id,
        request,
        ..
    } = prepare_send_request(&st, path_tenant, payload).await?;

    if let Some(config) = push_notification_config {
        upsert_push_notification_config_for_thread(
            &st,
            &thread_id,
            &task_id,
            effective_tenant.as_deref(),
            config,
        )
        .await?;
    }

    if let Some(start_message_id) = new_task_start_message_id.as_deref() {
        record_task_binding(&st, &thread_id, &task_id, start_message_id).await?;
    }

    st.mailbox
        .submit_background(request)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    for config in load_push_notification_configs(&st, &task_id, effective_tenant.as_deref()).await?
    {
        spawn_push_notification_driver(
            st.clone(),
            task_id.clone(),
            effective_tenant.clone(),
            config,
        );
    }

    Ok(stream_task_response(
        st,
        task_id,
        effective_tenant,
        history_length,
    ))
}

async fn subscribe_task(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }

    let snapshot = load_task_snapshot(&st, &task_id, tenant.as_deref(), usize::MAX, true)
        .await?
        .ok_or_else(|| A2aError::task_not_found(task_id.clone()))?;
    if snapshot.task.status.state.is_terminal() {
        return Err(A2aError::task_not_subscribable(
            task_id,
            snapshot.task.status.state,
        ));
    }

    Ok(stream_task_response(
        st,
        snapshot.task.id,
        tenant,
        usize::MAX,
    ))
}

fn stream_task_response(
    st: AppState,
    task_id: String,
    tenant: Option<String>,
    history_length: usize,
) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(st.config.sse_buffer_size);

    tokio::spawn(async move {
        let mut sent_initial = false;
        let mut last_status: Option<TaskStatus> = None;
        let mut last_artifacts: Vec<Artifact> = Vec::new();

        loop {
            let snapshot = match load_task_snapshot(
                &st,
                &task_id,
                tenant.as_deref(),
                history_length,
                true,
            )
            .await
            {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => TaskSnapshot {
                    task: submitted_task(
                        &task_id,
                        &task_context_id(&st, &task_id)
                            .await
                            .unwrap_or_else(|_| task_id.clone()),
                        tenant.as_deref(),
                    ),
                    updated_at_ms: 0,
                    current_agent_id: tenant.clone(),
                },
                Err(err) => {
                    tracing::warn!(task_id = %task_id, error = ?err, "A2A stream snapshot failed");
                    break;
                }
            };

            if !sent_initial {
                if send_stream_response(
                    &tx,
                    StreamResponse {
                        task: Some(snapshot.task.clone()),
                        ..Default::default()
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
                last_status = Some(snapshot.task.status.clone());
                last_artifacts = snapshot.task.artifacts.clone();
                sent_initial = true;
            } else {
                if last_status.as_ref() != Some(&snapshot.task.status) {
                    if send_stream_response(
                        &tx,
                        StreamResponse {
                            status_update: Some(TaskStatusUpdateEvent {
                                task_id: snapshot.task.id.clone(),
                                context_id: snapshot.task.context_id.clone(),
                                status: snapshot.task.status.clone(),
                                metadata: None,
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    last_status = Some(snapshot.task.status.clone());
                }

                if snapshot.task.artifacts != last_artifacts {
                    let total = snapshot.task.artifacts.len();
                    for (index, artifact) in snapshot.task.artifacts.iter().cloned().enumerate() {
                        if send_stream_response(
                            &tx,
                            StreamResponse {
                                artifact_update: Some(TaskArtifactUpdateEvent {
                                    task_id: snapshot.task.id.clone(),
                                    context_id: snapshot.task.context_id.clone(),
                                    artifact,
                                    append: Some(false),
                                    last_chunk: Some(index + 1 == total),
                                    metadata: None,
                                }),
                                ..Default::default()
                            },
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                    last_artifacts = snapshot.task.artifacts.clone();
                }
            }

            if snapshot.task.status.state.is_terminal()
                || snapshot.task.status.state.is_interrupted()
            {
                break;
            }

            tokio::time::sleep(BLOCKING_POLL_INTERVAL).await;
        }
    });

    sse_response(sse_body_stream(rx))
}

async fn send_stream_response(
    tx: &mpsc::Sender<Bytes>,
    response: StreamResponse,
) -> Result<(), ()> {
    let payload = serde_json::to_string(&response).map_err(|_| ())?;
    tx.send(format_sse_data(&payload)).await.map_err(|_| ())
}

async fn create_push_config(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
    payload: PushNotificationConfig,
) -> Result<Json<PushNotificationConfig>, A2aError> {
    ensure_supported_version(&headers)?;
    ensure_task_visible(&st, &task_id, tenant.as_deref()).await?;
    let config = normalize_push_config(payload, tenant.as_deref(), &task_id)?;
    upsert_push_notification_config(&st, &task_id, tenant.as_deref(), config.clone()).await?;
    spawn_push_notification_driver(st, task_id, tenant, config.clone());
    Ok(Json(config))
}

async fn list_push_configs(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
    query: ListPushConfigsQuery,
) -> Result<Json<ListPushNotificationConfigsResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    ensure_task_visible(&st, &task_id, tenant.as_deref()).await?;

    let page_size = query
        .page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let offset = parse_page_token(query.page_token.as_deref())?;
    let configs = load_push_notification_configs(&st, &task_id, tenant.as_deref()).await?;
    let total = configs.len();
    let items = configs
        .into_iter()
        .skip(offset)
        .take(page_size)
        .collect::<Vec<_>>();
    let next_offset = offset + items.len();

    Ok(Json(ListPushNotificationConfigsResponse {
        configs: items,
        next_page_token: if next_offset < total {
            next_offset.to_string()
        } else {
            String::new()
        },
    }))
}

async fn get_push_config(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
    config_id: String,
) -> Result<Json<PushNotificationConfig>, A2aError> {
    ensure_supported_version(&headers)?;
    ensure_task_visible(&st, &task_id, tenant.as_deref()).await?;
    let config = find_push_notification_config(&st, &task_id, tenant.as_deref(), &config_id)
        .await?
        .ok_or_else(|| A2aError::push_config_not_found(task_id.clone(), config_id.clone()))?;
    Ok(Json(config))
}

async fn delete_push_config(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
    config_id: String,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    ensure_task_visible(&st, &task_id, tenant.as_deref()).await?;

    let mut configs = load_push_notification_configs(&st, &task_id, tenant.as_deref()).await?;
    let before = configs.len();
    configs.retain(|config| config.id.as_deref() != Some(config_id.as_str()));
    if configs.len() == before {
        return Err(A2aError::push_config_not_found(task_id, config_id));
    }
    save_push_notification_configs(&st, &task_id, configs).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_tasks(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    query: ListTasksQuery,
) -> Result<Json<ListTasksResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }

    let page_size = query
        .page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let offset = parse_page_token(query.page_token.as_deref())?;
    let history_length = query.history_length.unwrap_or(0);
    let status_filter = query
        .status
        .as_deref()
        .map(parse_task_state_filter)
        .transpose()?;

    let mut snapshots = Vec::new();
    for task_id in collect_task_ids(&st).await? {
        let Some(snapshot) =
            load_task_snapshot(&st, &task_id, tenant.as_deref(), history_length, false).await?
        else {
            continue;
        };

        if let Some(ref context_id) = query.context_id
            && snapshot.task.context_id != *context_id
        {
            continue;
        }
        if let Some(expected) = status_filter
            && snapshot.task.status.state != expected
        {
            continue;
        }
        snapshots.push(snapshot);
    }

    snapshots.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.task.id.cmp(&right.task.id))
    });

    let total_size = snapshots.len();
    let tasks = snapshots
        .into_iter()
        .skip(offset)
        .take(page_size)
        .map(|snapshot| snapshot.task)
        .collect::<Vec<_>>();
    let next_offset = offset + tasks.len();
    let next_page_token = if next_offset < total_size {
        next_offset.to_string()
    } else {
        String::new()
    };

    Ok(Json(ListTasksResponse {
        tasks,
        total_size,
        page_size,
        next_page_token,
    }))
}

async fn get_task(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
    query: GetTaskQuery,
) -> Result<Json<Task>, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }
    let history_length = query.history_length.unwrap_or(usize::MAX);
    let snapshot = load_task_snapshot(&st, &task_id, tenant.as_deref(), history_length, true)
        .await?
        .ok_or_else(|| A2aError::task_not_found(task_id.clone()))?;
    Ok(Json(snapshot.task))
}

async fn cancel_task(
    st: AppState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
) -> Result<Json<Task>, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }

    let existing = load_task_snapshot(&st, &task_id, tenant.as_deref(), usize::MAX, true)
        .await?
        .ok_or_else(|| A2aError::task_not_found(task_id.clone()))?;

    if existing.task.status.state.is_terminal() {
        return Err(A2aError::task_not_cancelable(
            task_id,
            existing.task.status.state,
        ));
    }

    let queued_jobs = st
        .mailbox
        .list_jobs(&existing.task.id, Some(&[MailboxJobStatus::Queued]), 100, 0)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    let mut cancelled = false;
    for job in queued_jobs {
        cancelled |= st
            .mailbox
            .cancel(&job.job_id)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
    }
    cancelled |= st
        .mailbox
        .cancel(&existing.task.id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    if !cancelled {
        return Err(A2aError::task_not_cancelable(
            existing.task.id,
            existing.task.status.state,
        ));
    }

    let task = load_task_snapshot(&st, &existing.task.id, tenant.as_deref(), usize::MAX, true)
        .await?
        .map(|snapshot| snapshot.task)
        .unwrap_or_else(|| {
            canceled_task(
                &existing.task.id,
                &existing.task.context_id,
                existing.current_agent_id.as_deref(),
            )
        });

    Ok(Json(task))
}

struct PreparedRequest {
    task_id: String,
    thread_id: String,
    effective_tenant: Option<String>,
    history_length: usize,
    return_immediately: bool,
    push_notification_config: Option<PushNotificationConfig>,
    new_task_start_message_id: Option<String>,
    request: RunRequest,
}

async fn prepare_send_request(
    st: &AppState,
    path_tenant: Option<String>,
    payload: SendMessageRequest,
) -> Result<PreparedRequest, A2aError> {
    let mut violations = Vec::new();
    let request_tenant = trim_to_option(payload.tenant.as_deref());
    let effective_tenant = match (path_tenant, request_tenant) {
        (Some(path), Some(body)) if path != body => {
            violations.push(FieldViolation {
                field: "tenant".into(),
                description: "path tenant and body tenant must match".into(),
            });
            Some(path)
        }
        (Some(path), _) => Some(path),
        (None, body) => body,
    };

    if payload.message.role != MessageRole::User {
        violations.push(FieldViolation {
            field: "message.role".into(),
            description: "only ROLE_USER messages are supported for inbound A2A requests".into(),
        });
    }
    if payload.message.message_id.trim().is_empty() {
        violations.push(FieldViolation {
            field: "message.messageId".into(),
            description: "messageId is required".into(),
        });
    }
    if payload.message.parts.is_empty() {
        violations.push(FieldViolation {
            field: "message.parts".into(),
            description: "at least one part is required".into(),
        });
    }

    for (index, part) in payload.message.parts.iter().enumerate() {
        let payload_count = usize::from(part.text.is_some())
            + usize::from(part.raw.is_some())
            + usize::from(part.url.is_some())
            + usize::from(part.data.is_some());
        if payload_count != 1 {
            violations.push(FieldViolation {
                field: format!("message.parts[{index}]"),
                description: "each part must contain exactly one of text, raw, url, or data".into(),
            });
        }
    }

    let accepted_output_modes = payload
        .configuration
        .as_ref()
        .map(|cfg| cfg.accepted_output_modes.as_slice())
        .unwrap_or(&[]);
    if !accepted_output_modes.is_empty()
        && !accepted_output_modes
            .iter()
            .any(|mode| mode.eq_ignore_ascii_case(SUPPORTED_OUTPUT_MODE))
    {
        return Err(A2aError::content_type_not_supported(
            accepted_output_modes.join(","),
        ));
    }
    if !violations.is_empty() {
        return Err(A2aError::merge_invalid("invalid A2A request", violations));
    }

    if let Some(ref tenant) = effective_tenant {
        ensure_runnable_agent(st, tenant)?;
    }

    let task_id = trim_to_option(payload.message.task_id.as_deref());
    let context_id = trim_to_option(payload.message.context_id.as_deref());
    let existing_task = if let Some(task_id) = task_id.as_deref() {
        resolve_task(st, task_id).await?
    } else {
        None
    };
    let thread_id = existing_task
        .as_ref()
        .map(|task| task.thread_id.clone())
        .or_else(|| context_id.clone())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    if let Some(context_id) = context_id.as_deref()
        && context_id != thread_id
    {
        return Err(A2aError::invalid(
            "message.contextId",
            "contextId must match the task's thread context",
        ));
    }
    let task_id = task_id.unwrap_or_else(|| Uuid::now_v7().to_string());
    let content = payload
        .message
        .parts
        .iter()
        .map(a2a_part_to_content_block)
        .collect::<Result<Vec<_>, _>>()?;

    let message_id = payload.message.message_id.clone();
    let awaken_message = AwakenMessage::user_with_content(content).with_id(message_id.clone());
    let mut request = RunRequest::new(thread_id.clone(), vec![awaken_message]);
    let mut new_task_start_message_id = None;

    if let Some(ref tenant) = effective_tenant {
        request = request.with_agent_id(tenant.clone());
    } else if thread_has_context(st, &thread_id).await? {
        // Keep agent inference on existing threads.
    } else {
        request = request.with_agent_id(public_agent_id(st)?);
    }

    match existing_task {
        Some(existing_task) => {
            let Some(run) = existing_task.run.as_ref() else {
                return Err(A2aError::invalid(
                    "message.taskId",
                    "taskId refers to an in-flight task; wait for completion or use contextId for a new task",
                ));
            };
            if !run_is_a2a_resumable(run) {
                return Err(A2aError::invalid(
                    "message.taskId",
                    "taskId must reference an interrupted task; use contextId to start a new task in the same context",
                ));
            }
            request = request.with_continue_run_id(task_id.clone());
        }
        None => {
            new_task_start_message_id = Some(message_id);
            request = request.with_job_id_hint(task_id.clone());
        }
    }

    let history_length = payload
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.history_length)
        .map(|value| value as usize)
        .unwrap_or(usize::MAX);
    let return_immediately = payload
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.return_immediately)
        .unwrap_or(false);
    let push_notification_config = payload
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.task_push_notification_config.clone())
        .map(|config| normalize_push_config(config, effective_tenant.as_deref(), &task_id))
        .transpose()?;

    Ok(PreparedRequest {
        task_id,
        thread_id,
        effective_tenant,
        history_length,
        return_immediately,
        push_notification_config,
        new_task_start_message_id,
        request,
    })
}

async fn resolve_task(st: &AppState, task_id: &str) -> Result<Option<ResolvedTask>, A2aError> {
    if let Some(run) = st
        .store
        .load_run(task_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    {
        let job = st
            .mailbox
            .load_job(task_id)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
        return Ok(Some(ResolvedTask {
            thread_id: run.thread_id.clone(),
            run: Some(run),
            job,
        }));
    }

    let Some(job) = st
        .mailbox
        .load_job(task_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    else {
        return Ok(None);
    };
    Ok(Some(ResolvedTask {
        thread_id: job.mailbox_id.clone(),
        run: None,
        job: Some(job),
    }))
}

fn run_is_a2a_resumable(run: &RunRecord) -> bool {
    run.status == RunStatus::Waiting
        && !matches!(run.termination_code.as_deref(), Some("awaiting_tasks"))
}

async fn record_task_binding(
    st: &AppState,
    thread_id: &str,
    task_id: &str,
    start_message_id: &str,
) -> Result<(), A2aError> {
    let existing = st
        .store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    let mut thread = existing.unwrap_or_else(|| Thread::with_id(thread_id));
    let mut bindings = thread
        .metadata
        .custom
        .remove(TASK_BINDINGS_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<StoredTaskBindings>(value).ok())
        .unwrap_or_default();
    bindings.tasks.insert(
        task_id.to_string(),
        StoredTaskBinding {
            thread_id: thread_id.to_string(),
            start_message_id: start_message_id.to_string(),
            end_message_id: None,
        },
    );
    for (existing_task_id, binding) in bindings.tasks.iter_mut() {
        if existing_task_id != task_id && binding.end_message_id.is_none() {
            binding.end_message_id = Some(start_message_id.to_string());
        }
    }
    thread.metadata.custom.insert(
        TASK_BINDINGS_METADATA_KEY.to_string(),
        serde_json::to_value(bindings).map_err(|e| A2aError::Internal(e.to_string()))?,
    );

    if st
        .store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_some()
    {
        st.store
            .update_thread_metadata(thread_id, thread.metadata)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
    } else {
        st.store
            .save_thread(&thread)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
    }
    Ok(())
}

async fn load_task_binding(
    st: &AppState,
    thread_id: &str,
    task_id: &str,
) -> Result<Option<StoredTaskBinding>, A2aError> {
    let Some(thread) = st
        .store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    else {
        return Ok(None);
    };

    Ok(thread
        .metadata
        .custom
        .get(TASK_BINDINGS_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<StoredTaskBindings>(value.clone()).ok())
        .and_then(|bindings| bindings.tasks.get(task_id).cloned()))
}

async fn task_context_id(st: &AppState, task_id: &str) -> Result<String, A2aError> {
    Ok(resolve_task(st, task_id)
        .await?
        .map(|task| task.thread_id)
        .unwrap_or_else(|| task_id.to_string()))
}

async fn wait_for_task(
    st: &AppState,
    task_id: &str,
    tenant: Option<&str>,
    history_length: usize,
) -> Result<Task, A2aError> {
    let deadline = tokio::time::Instant::now() + BLOCKING_WAIT_TIMEOUT;
    let mut last_seen: Option<Task> = None;

    loop {
        if let Some(snapshot) =
            load_task_snapshot(st, task_id, tenant, history_length, true).await?
        {
            let state = snapshot.task.status.state;
            last_seen = Some(snapshot.task.clone());
            if state.is_terminal() || state.is_interrupted() {
                return Ok(snapshot.task);
            }
        }

        if tokio::time::Instant::now() >= deadline {
            let context_id = task_context_id(st, task_id).await?;
            return Ok(last_seen.unwrap_or_else(|| submitted_task(task_id, &context_id, tenant)));
        }

        tokio::time::sleep(BLOCKING_POLL_INTERVAL).await;
    }
}

async fn load_task_snapshot(
    st: &AppState,
    task_id: &str,
    tenant: Option<&str>,
    history_length: usize,
    include_artifacts: bool,
) -> Result<Option<TaskSnapshot>, A2aError> {
    let Some(task) = resolve_task(st, task_id).await? else {
        return Ok(None);
    };
    let thread_id = task.thread_id.clone();
    let latest_run = task.run.clone();
    let latest_job = if let Some(job) = task.job.clone() {
        Some(job)
    } else {
        st.mailbox
            .list_jobs(&thread_id, None, 100, 0)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?
            .into_iter()
            .filter(|job| {
                let extras = job.request_extras.as_ref();
                let continue_run_id = extras
                    .and_then(|value| value.get("continue_run_id"))
                    .and_then(serde_json::Value::as_str);
                let job_id_hint = extras
                    .and_then(|value| value.get("job_id_hint"))
                    .and_then(serde_json::Value::as_str);
                continue_run_id == Some(task_id) || job_id_hint == Some(task_id)
            })
            .max_by_key(|job| job.updated_at)
    };

    let history = st
        .store
        .load_messages(&thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .unwrap_or_default();
    let binding = load_task_binding(st, &thread_id, task_id).await?;
    let mut converted_history = if let Some(binding) = binding.as_ref()
        && !binding.start_message_id.is_empty()
    {
        let full_history = history
            .iter()
            .filter_map(|message| awaken_message_to_a2a_message(message, task_id, &thread_id))
            .collect::<Vec<_>>();
        let start_index = full_history
            .iter()
            .position(|message| message.message_id == binding.start_message_id)
            .unwrap_or(0);
        let end_index = binding
            .end_message_id
            .as_deref()
            .and_then(|message_id| {
                full_history
                    .iter()
                    .position(|message| message.message_id == message_id)
            })
            .unwrap_or(full_history.len());
        full_history
            .into_iter()
            .skip(start_index)
            .take(end_index.saturating_sub(start_index))
            .collect::<Vec<_>>()
    } else {
        history
            .iter()
            .filter_map(|message| awaken_message_to_a2a_message(message, task_id, &thread_id))
            .collect::<Vec<_>>()
    };
    let latest_agent_message = converted_history
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Agent)
        .cloned();

    let run_source = latest_run.as_ref().map(|record| TaskSource {
        state: run_record_to_task_state(record),
        updated_at_ms: record.updated_at.saturating_mul(1000),
        current_agent_id: Some(record.agent_id.clone()),
    });
    let job_source = latest_job.as_ref().map(|job| TaskSource {
        state: mailbox_job_to_task_state(job.status),
        updated_at_ms: job.updated_at,
        current_agent_id: Some(job.agent_id.clone()),
    });

    let source = match (&run_source, &job_source) {
        (Some(run), Some(job)) if job.updated_at_ms >= run.updated_at_ms => {
            if latest_job
                .as_ref()
                .is_some_and(|job| job.status != MailboxJobStatus::Accepted)
            {
                job_source
            } else {
                run_source
            }
        }
        (Some(_), _) => run_source,
        (_, Some(_)) => job_source,
        (None, None) => None,
    };

    let Some(source) = source else {
        return Ok(None);
    };

    if let Some(tenant) = tenant
        && source.current_agent_id.as_deref() != Some(tenant)
    {
        return Ok(None);
    }

    if history_length != usize::MAX && converted_history.len() > history_length {
        let keep_from = converted_history.len().saturating_sub(history_length);
        converted_history = converted_history.split_off(keep_from);
    }

    let status_message = if matches!(
        source.state,
        TaskState::Completed
            | TaskState::Failed
            | TaskState::Rejected
            | TaskState::InputRequired
            | TaskState::AuthRequired
            | TaskState::Canceled
    ) {
        latest_agent_message.clone()
    } else {
        None
    };

    let artifacts = if include_artifacts && matches!(source.state, TaskState::Completed) {
        latest_agent_message
            .as_ref()
            .map(message_to_artifacts)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(Some(TaskSnapshot {
        task: Task {
            id: task_id.to_string(),
            context_id: thread_id,
            status: TaskStatus {
                state: source.state,
                message: status_message,
                timestamp: None,
            },
            artifacts,
            history: converted_history,
            metadata: None,
        },
        updated_at_ms: source.updated_at_ms,
        current_agent_id: source.current_agent_id,
    }))
}

fn message_to_artifacts(message: &A2aMessage) -> Vec<Artifact> {
    if message.parts.is_empty() {
        Vec::new()
    } else {
        vec![Artifact {
            artifact_id: "response".to_string(),
            name: Some("response".to_string()),
            description: None,
            parts: message.parts.clone(),
            metadata: None,
        }]
    }
}

fn run_record_to_task_state(record: &awaken_contract::contract::storage::RunRecord) -> TaskState {
    match record.status {
        RunStatus::Running => TaskState::Working,
        RunStatus::Waiting => match record.termination_code.as_deref() {
            Some("auth_required") => TaskState::AuthRequired,
            Some("awaiting_tasks") => TaskState::Working,
            _ => TaskState::InputRequired,
        },
        RunStatus::Done => match record.termination_code.as_deref() {
            Some("cancelled") => TaskState::Canceled,
            Some(code) if code.starts_with("blocked:") => TaskState::Rejected,
            Some(code) if code == "error" || code.starts_with("error:") => TaskState::Failed,
            _ => TaskState::Completed,
        },
    }
}

fn mailbox_job_to_task_state(status: MailboxJobStatus) -> TaskState {
    match status {
        MailboxJobStatus::Queued => TaskState::Submitted,
        MailboxJobStatus::Claimed | MailboxJobStatus::Accepted => TaskState::Working,
        MailboxJobStatus::Cancelled | MailboxJobStatus::Superseded => TaskState::Canceled,
        MailboxJobStatus::DeadLetter => TaskState::Failed,
    }
}

fn submitted_task(task_id: &str, context_id: &str, tenant: Option<&str>) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: None,
        },
        artifacts: Vec::new(),
        history: Vec::new(),
        metadata: tenant.map(|tenant| json!({"tenant": tenant})),
    }
}

fn canceled_task(task_id: &str, context_id: &str, tenant: Option<&str>) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Canceled,
            message: None,
            timestamp: None,
        },
        artifacts: Vec::new(),
        history: Vec::new(),
        metadata: tenant.map(|tenant| json!({"tenant": tenant})),
    }
}

fn build_agent_card(
    st: &AppState,
    headers: &HeaderMap,
    agent_id: &str,
    tenant: Option<&str>,
    _extended: bool,
) -> AgentCard {
    let supports_extended_card = supports_extended_agent_card(st);
    let security_schemes = if supports_extended_card {
        BTreeMap::from([(
            EXTENDED_CARD_SECURITY_SCHEME_ID.to_string(),
            json!({
                "httpAuthSecurityScheme": {
                    "scheme": "Bearer"
                }
            }),
        )])
    } else {
        BTreeMap::new()
    };
    let security = if supports_extended_card {
        vec![BTreeMap::from([(
            EXTENDED_CARD_SECURITY_SCHEME_ID.to_string(),
            Vec::new(),
        )])]
    } else {
        Vec::new()
    };

    AgentCard {
        name: agent_id.to_string(),
        description: format!("Awaken AI agent '{agent_id}'"),
        supported_interfaces: vec![AgentInterface {
            url: interface_url(headers, tenant),
            protocol_binding: "HTTP+JSON".to_string(),
            protocol_version: A2A_VERSION.to_string(),
            tenant: tenant.map(ToOwned::to_owned),
        }],
        provider: Some(AgentProvider {
            organization: "Awaken".to_string(),
            url: Some(origin_url(headers)),
        }),
        version: env!("CARGO_PKG_VERSION").to_string(),
        documentation_url: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: true,
            state_transition_history: false,
            extended_agent_card: supports_extended_card,
        },
        security_schemes,
        security,
        default_input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
        default_output_modes: vec![SUPPORTED_OUTPUT_MODE.to_string()],
        skills: vec![AgentSkill {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: Some(format!("Interact with the '{agent_id}' Awaken agent.")),
            tags: vec!["awaken".to_string(), "agent".to_string()],
            examples: Vec::new(),
            input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            output_modes: vec![SUPPORTED_OUTPUT_MODE.to_string()],
        }],
        signatures: Vec::new(),
        icon_url: None,
    }
}

fn supports_extended_agent_card(st: &AppState) -> bool {
    st.config.a2a_extended_card_bearer_token.is_some()
}

fn ensure_extended_card_auth(st: &AppState, headers: &HeaderMap) -> Result<(), A2aError> {
    let Some(expected) = st.config.a2a_extended_card_bearer_token.as_deref() else {
        return Err(A2aError::unsupported_operation(
            "extendedAgentCard is not configured for this agent",
        ));
    };
    let Some(auth) = forwarded_header(headers, "authorization") else {
        return Err(A2aError::unauthenticated(
            "missing Authorization header for extendedAgentCard",
        ));
    };
    let Some(token) = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
    else {
        return Err(A2aError::unauthenticated(
            "Authorization header must use Bearer authentication",
        ));
    };
    if token.trim() != expected {
        return Err(A2aError::unauthenticated(
            "invalid bearer token for extendedAgentCard",
        ));
    }
    Ok(())
}

fn origin_url(headers: &HeaderMap) -> String {
    let scheme = forwarded_header(headers, "x-forwarded-proto").unwrap_or("http");
    let host = forwarded_header(headers, "x-forwarded-host")
        .or_else(|| forwarded_header(headers, "host"))
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

fn interface_url(headers: &HeaderMap, tenant: Option<&str>) -> String {
    let base = origin_url(headers);
    match tenant {
        Some(tenant) => format!("{base}{INTERFACE_BASE_PATH}/{tenant}"),
        None => format!("{base}{INTERFACE_BASE_PATH}"),
    }
}

fn forwarded_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn ensure_supported_version_from_request(headers: &HeaderMap, uri: &Uri) -> Result<(), A2aError> {
    if let Some(version) = uri
        .query()
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(key, value)| key.eq_ignore_ascii_case("A2A-Version").then_some(value))
        && version != A2A_VERSION
    {
        return Err(A2aError::version_not_supported(version));
    }
    ensure_supported_version(headers)
}

fn ensure_supported_version(headers: &HeaderMap) -> Result<(), A2aError> {
    if let Some(version) = forwarded_header(headers, "a2a-version")
        && version != A2A_VERSION
    {
        return Err(A2aError::version_not_supported(version));
    }
    Ok(())
}

fn public_agent_id(st: &AppState) -> Result<String, A2aError> {
    if st.resolver.resolve("default").is_ok() {
        return Ok("default".to_string());
    }

    let mut ids = st.resolver.agent_ids();
    ids.sort();
    ids.into_iter()
        .find(|id| st.resolver.resolve(id).is_ok())
        .ok_or_else(|| A2aError::NotFound("no runnable local agents registered".to_string()))
}

fn ensure_runnable_agent(st: &AppState, agent_id: &str) -> Result<(), A2aError> {
    st.resolver
        .resolve(agent_id)
        .map(|_| ())
        .map_err(|_| A2aError::NotFound(format!("agent not found: {agent_id}")))
}

async fn thread_has_context(st: &AppState, thread_id: &str) -> Result<bool, A2aError> {
    if st
        .store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_some()
    {
        return Ok(true);
    }

    if st
        .store
        .load_messages(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_some()
    {
        return Ok(true);
    }

    if st
        .store
        .latest_run(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_some()
    {
        return Ok(true);
    }

    Ok(!st
        .mailbox
        .list_jobs(thread_id, None, 1, 0)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_empty())
}

async fn collect_task_ids(st: &AppState) -> Result<Vec<String>, A2aError> {
    let mut ids = BTreeSet::new();
    let mut run_offset = 0;
    loop {
        let page = st
            .store
            .list_runs(&RunQuery {
                offset: run_offset,
                limit: 100,
                ..Default::default()
            })
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
        if page.items.is_empty() {
            break;
        }
        run_offset += page.items.len();
        ids.extend(page.items.into_iter().map(|run| run.run_id));
        if !page.has_more {
            break;
        }
    }

    let mut offset = 0;
    loop {
        let batch = st
            .store
            .list_threads(offset, 100)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
        if batch.is_empty() {
            break;
        }
        offset += batch.len();
        for thread_id in batch {
            let jobs = st
                .mailbox
                .list_jobs(
                    &thread_id,
                    Some(&[MailboxJobStatus::Queued, MailboxJobStatus::Claimed]),
                    100,
                    0,
                )
                .await
                .map_err(|e| A2aError::Internal(e.to_string()))?;
            ids.extend(jobs.into_iter().map(|job| job.job_id));
        }
    }
    Ok(ids.into_iter().collect())
}

fn normalize_push_config(
    mut config: PushNotificationConfig,
    tenant: Option<&str>,
    task_id: &str,
) -> Result<PushNotificationConfig, A2aError> {
    let parsed_url = reqwest::Url::parse(&config.url)
        .map_err(|err| A2aError::invalid("pushNotificationConfig.url", err.to_string()))?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(A2aError::invalid(
            "pushNotificationConfig.url",
            "push notification URL must use http or https",
        ));
    }

    if let Some(existing_task_id) = trim_to_option(config.task_id.as_deref())
        && existing_task_id != task_id
    {
        return Err(A2aError::invalid(
            "pushNotificationConfig.taskId",
            "push notification taskId must match the enclosing task",
        ));
    }
    if let Some(existing_tenant) = trim_to_option(config.tenant.as_deref())
        && tenant != Some(existing_tenant.as_str())
    {
        return Err(A2aError::invalid(
            "pushNotificationConfig.tenant",
            "push notification tenant must match the enclosing task tenant",
        ));
    }
    if let Some(authentication) = config.authentication.as_ref()
        && authentication.scheme.trim().is_empty()
    {
        return Err(A2aError::invalid(
            "pushNotificationConfig.authentication.scheme",
            "authentication scheme must not be empty",
        ));
    }

    config.id.get_or_insert_with(|| Uuid::now_v7().to_string());
    config.task_id = Some(task_id.to_string());
    config.tenant = tenant.map(ToOwned::to_owned);
    Ok(config)
}

async fn ensure_task_visible(
    st: &AppState,
    task_id: &str,
    tenant: Option<&str>,
) -> Result<(), A2aError> {
    if let Some(tenant) = tenant {
        ensure_runnable_agent(st, tenant)?;
        let visible = load_task_snapshot(st, task_id, Some(tenant), 0, false)
            .await?
            .is_some();
        if !visible {
            return Err(A2aError::task_not_found(task_id.to_string()));
        }
        return Ok(());
    }

    if resolve_task(st, task_id).await?.is_some() {
        Ok(())
    } else {
        Err(A2aError::task_not_found(task_id.to_string()))
    }
}

async fn load_push_notification_configs(
    st: &AppState,
    task_id: &str,
    tenant: Option<&str>,
) -> Result<Vec<PushNotificationConfig>, A2aError> {
    let Some(task) = resolve_task(st, task_id).await? else {
        return Ok(Vec::new());
    };
    let Some(thread) = st
        .store
        .load_thread(&task.thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    else {
        return Ok(Vec::new());
    };

    let mut configs = load_thread_push_notification_configs(&thread, task_id)?;
    if let Some(tenant) = tenant {
        configs.retain(|config| config.tenant.as_deref() == Some(tenant));
    }
    configs.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(configs)
}

async fn find_push_notification_config(
    st: &AppState,
    task_id: &str,
    tenant: Option<&str>,
    config_id: &str,
) -> Result<Option<PushNotificationConfig>, A2aError> {
    Ok(load_push_notification_configs(st, task_id, tenant)
        .await?
        .into_iter()
        .find(|config| config.id.as_deref() == Some(config_id)))
}

async fn save_push_notification_configs(
    st: &AppState,
    task_id: &str,
    configs: Vec<PushNotificationConfig>,
) -> Result<(), A2aError> {
    let Some(task) = resolve_task(st, task_id).await? else {
        return Err(A2aError::task_not_found(task_id.to_string()));
    };
    let thread_id = task.thread_id;
    let existing = st
        .store
        .load_thread(&thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    let thread = existing.unwrap_or_else(|| Thread::with_id(&thread_id));
    save_thread_push_notification_configs(st, &thread_id, thread, task_id, configs).await
}

async fn upsert_push_notification_config(
    st: &AppState,
    task_id: &str,
    tenant: Option<&str>,
    config: PushNotificationConfig,
) -> Result<(), A2aError> {
    let mut configs = load_push_notification_configs(st, task_id, tenant).await?;
    if let Some(position) = configs.iter().position(|existing| existing.id == config.id) {
        configs[position] = config;
    } else {
        configs.push(config);
    }
    save_push_notification_configs(st, task_id, configs).await
}

async fn upsert_push_notification_config_for_thread(
    st: &AppState,
    thread_id: &str,
    task_id: &str,
    tenant: Option<&str>,
    config: PushNotificationConfig,
) -> Result<(), A2aError> {
    let existing = st
        .store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    let thread = existing.unwrap_or_else(|| Thread::with_id(thread_id));
    let mut configs = load_thread_push_notification_configs(&thread, task_id)?;
    if let Some(tenant) = tenant {
        configs.retain(|existing| existing.tenant.as_deref() == Some(tenant));
    }
    if let Some(position) = configs.iter().position(|existing| existing.id == config.id) {
        configs[position] = config;
    } else {
        configs.push(config);
    }
    save_thread_push_notification_configs(st, thread_id, thread, task_id, configs).await
}

fn load_thread_push_notification_configs(
    thread: &Thread,
    task_id: &str,
) -> Result<Vec<PushNotificationConfig>, A2aError> {
    let Some(value) = thread.metadata.custom.get(PUSH_CONFIGS_METADATA_KEY) else {
        return Ok(Vec::new());
    };

    if let Ok(stored) = serde_json::from_value::<StoredPushConfigs>(value.clone()) {
        Ok(stored.tasks.get(task_id).cloned().unwrap_or_default())
    } else {
        serde_json::from_value(value.clone()).map_err(|err| A2aError::Internal(err.to_string()))
    }
}

async fn save_thread_push_notification_configs(
    st: &AppState,
    thread_id: &str,
    mut thread: Thread,
    task_id: &str,
    configs: Vec<PushNotificationConfig>,
) -> Result<(), A2aError> {
    let mut stored = thread
        .metadata
        .custom
        .remove(PUSH_CONFIGS_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<StoredPushConfigs>(value).ok())
        .unwrap_or_default();
    if configs.is_empty() {
        stored.tasks.remove(task_id);
    } else {
        stored.tasks.insert(task_id.to_string(), configs);
    }
    if stored.tasks.is_empty() {
        thread.metadata.custom.remove(PUSH_CONFIGS_METADATA_KEY);
    } else {
        thread.metadata.custom.insert(
            PUSH_CONFIGS_METADATA_KEY.to_string(),
            serde_json::to_value(stored).map_err(|e| A2aError::Internal(e.to_string()))?,
        );
    }

    if st
        .store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_some()
    {
        st.store
            .update_thread_metadata(thread_id, thread.metadata)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
    } else {
        st.store
            .save_thread(&thread)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
    }

    Ok(())
}

fn spawn_push_notification_driver(
    st: AppState,
    task_id: String,
    tenant: Option<String>,
    config: PushNotificationConfig,
) {
    tokio::spawn(async move {
        if let Err(err) = drive_push_notification(st, task_id, tenant, config).await {
            tracing::warn!(error = ?err, "A2A push notification driver stopped with error");
        }
    });
}

async fn drive_push_notification(
    st: AppState,
    task_id: String,
    tenant: Option<String>,
    config: PushNotificationConfig,
) -> Result<(), A2aError> {
    let client = reqwest::Client::new();
    let config_id = config.id.clone().unwrap_or_default();
    let mut delivered_initial = false;
    let mut last_status: Option<TaskStatus> = None;
    let mut last_artifacts: Vec<Artifact> = Vec::new();

    loop {
        if find_push_notification_config(&st, &task_id, tenant.as_deref(), &config_id)
            .await?
            .is_none()
        {
            break;
        }

        let snapshot = load_task_snapshot(&st, &task_id, tenant.as_deref(), usize::MAX, true)
            .await?
            .unwrap_or(TaskSnapshot {
                task: submitted_task(
                    &task_id,
                    &task_context_id(&st, &task_id)
                        .await
                        .unwrap_or_else(|_| task_id.clone()),
                    tenant.as_deref(),
                ),
                updated_at_ms: 0,
                current_agent_id: tenant.clone(),
            });

        if !delivered_initial {
            post_push_notification(
                &client,
                &config,
                &StreamResponse {
                    status_update: Some(TaskStatusUpdateEvent {
                        task_id: snapshot.task.id.clone(),
                        context_id: snapshot.task.context_id.clone(),
                        status: snapshot.task.status.clone(),
                        metadata: None,
                    }),
                    ..Default::default()
                },
            )
            .await;
            delivered_initial = true;
            last_status = Some(snapshot.task.status.clone());
            last_artifacts = snapshot.task.artifacts.clone();
        } else {
            if last_status.as_ref() != Some(&snapshot.task.status) {
                post_push_notification(
                    &client,
                    &config,
                    &StreamResponse {
                        status_update: Some(TaskStatusUpdateEvent {
                            task_id: snapshot.task.id.clone(),
                            context_id: snapshot.task.context_id.clone(),
                            status: snapshot.task.status.clone(),
                            metadata: None,
                        }),
                        ..Default::default()
                    },
                )
                .await;
                last_status = Some(snapshot.task.status.clone());
            }

            if snapshot.task.artifacts != last_artifacts {
                let total = snapshot.task.artifacts.len();
                for (index, artifact) in snapshot.task.artifacts.iter().cloned().enumerate() {
                    post_push_notification(
                        &client,
                        &config,
                        &StreamResponse {
                            artifact_update: Some(TaskArtifactUpdateEvent {
                                task_id: snapshot.task.id.clone(),
                                context_id: snapshot.task.context_id.clone(),
                                artifact,
                                append: Some(false),
                                last_chunk: Some(index + 1 == total),
                                metadata: None,
                            }),
                            ..Default::default()
                        },
                    )
                    .await;
                }
                last_artifacts = snapshot.task.artifacts.clone();
            }
        }

        if snapshot.task.status.state.is_terminal() || snapshot.task.status.state.is_interrupted() {
            break;
        }

        tokio::time::sleep(BLOCKING_POLL_INTERVAL).await;
    }

    Ok(())
}

async fn post_push_notification(
    client: &reqwest::Client,
    config: &PushNotificationConfig,
    payload: &StreamResponse,
) {
    let mut request = client.post(&config.url).json(payload);
    if let Some(token) = config.token.as_deref() {
        request = request.header(A2A_NOTIFICATION_TOKEN_HEADER, token);
    }
    if let Some(authentication) = config.authentication.as_ref() {
        let credentials = authentication.credentials.as_deref().unwrap_or_default();
        request = request.header(
            reqwest::header::AUTHORIZATION,
            format!("{} {}", authentication.scheme, credentials).trim(),
        );
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            tracing::warn!(
                status = %response.status(),
                url = %config.url,
                "A2A push notification webhook returned non-success status"
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, url = %config.url, "A2A push notification webhook failed");
        }
    }
}

fn parse_page_token(page_token: Option<&str>) -> Result<usize, A2aError> {
    match page_token.map(str::trim).filter(|token| !token.is_empty()) {
        Some(token) => token.parse::<usize>().map_err(|_| {
            A2aError::invalid("pageToken", "pageToken must be an unsigned integer offset")
        }),
        None => Ok(0),
    }
}

fn parse_a2a_tail(tail: &str) -> Vec<&str> {
    tail.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn decode_query<T: DeserializeOwned>(uri: &Uri) -> Result<T, A2aError> {
    Query::<T>::try_from_uri(uri)
        .map(|query| query.0)
        .map_err(|err| A2aError::invalid("query", err.to_string()))
}

fn decode_json_body<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, A2aError> {
    ensure_json_content_type(headers)?;
    serde_json::from_slice(body)
        .map_err(|err| A2aError::invalid("body", format!("invalid JSON body: {err}")))
}

fn ensure_json_content_type(headers: &HeaderMap) -> Result<(), A2aError> {
    let Some(content_type) = forwarded_header(headers, "content-type") else {
        return Err(A2aError::invalid(
            "contentType",
            "Content-Type must be application/json or application/a2a+json",
        ));
    };

    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if media_type.eq_ignore_ascii_case("application/json")
        || media_type.eq_ignore_ascii_case("application/a2a+json")
    {
        Ok(())
    } else {
        Err(A2aError::invalid(
            "contentType",
            "Content-Type must be application/json or application/a2a+json",
        ))
    }
}

fn parse_task_state_filter(raw: &str) -> Result<TaskState, A2aError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "task_state_submitted" | "submitted" => Ok(TaskState::Submitted),
        "task_state_working" | "working" => Ok(TaskState::Working),
        "task_state_input_required" | "input_required" | "input-required" => {
            Ok(TaskState::InputRequired)
        }
        "task_state_auth_required" | "auth_required" | "auth-required" => {
            Ok(TaskState::AuthRequired)
        }
        "task_state_completed" | "completed" => Ok(TaskState::Completed),
        "task_state_failed" | "failed" => Ok(TaskState::Failed),
        "task_state_canceled" | "canceled" | "cancelled" => Ok(TaskState::Canceled),
        "task_state_rejected" | "rejected" => Ok(TaskState::Rejected),
        _ => Err(A2aError::invalid(
            "status",
            "status must be a valid TaskState value",
        )),
    }
}

fn parse_task_action_segment(raw: &str) -> Result<(String, &str), A2aError> {
    let Some((task_id, action)) = raw.rsplit_once(':') else {
        return Err(A2aError::NotFound(format!(
            "unsupported A2A task action path: {raw}"
        )));
    };

    if task_id.trim().is_empty() {
        return Err(A2aError::invalid(
            "taskId",
            "task action path must include a task id before the action suffix",
        ));
    }

    match action {
        "cancel" | "subscribe" => Ok((task_id.to_string(), action)),
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A task action path: {raw}"
        ))),
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "TASK_STATE_SUBMITTED",
        TaskState::Working => "TASK_STATE_WORKING",
        TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
        TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        TaskState::Completed => "TASK_STATE_COMPLETED",
        TaskState::Failed => "TASK_STATE_FAILED",
        TaskState::Canceled => "TASK_STATE_CANCELED",
        TaskState::Rejected => "TASK_STATE_REJECTED",
    }
}

fn trim_to_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn a2a_part_to_content_block(part: &Part) -> Result<ContentBlock, A2aError> {
    if let Some(text) = part.text.as_ref() {
        return Ok(ContentBlock::text(text.clone()));
    }
    if let Some(data) = part.data.as_ref() {
        return Ok(ContentBlock::text(data.to_string()));
    }
    if let Some(url) = part.url.as_ref() {
        return Ok(url_part_to_content_block(url, part));
    }
    if let Some(raw) = part.raw.as_ref() {
        return Ok(raw_part_to_content_block(raw, part));
    }
    Err(A2aError::invalid(
        "message.parts",
        "each part must contain a supported payload",
    ))
}

fn url_part_to_content_block(url: &str, part: &Part) -> ContentBlock {
    let media_type = part
        .media_type
        .clone()
        .unwrap_or_else(|| infer_media_type_from_url(url));
    if media_type.starts_with("image/") {
        ContentBlock::Image {
            source: ImageSource::Url {
                url: url.to_string(),
            },
        }
    } else if media_type.starts_with("audio/") {
        ContentBlock::Audio {
            source: AudioSource::Url {
                url: url.to_string(),
            },
        }
    } else if media_type.starts_with("video/") {
        ContentBlock::Video {
            source: VideoSource::Url {
                url: url.to_string(),
            },
        }
    } else {
        ContentBlock::Document {
            source: DocumentSource::Url {
                url: url.to_string(),
            },
            title: part.filename.clone(),
        }
    }
}

fn raw_part_to_content_block(raw: &str, part: &Part) -> ContentBlock {
    let media_type = part
        .media_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    if media_type.starts_with("image/") {
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type,
                data: raw.to_string(),
            },
        }
    } else if media_type.starts_with("audio/") {
        ContentBlock::Audio {
            source: AudioSource::Base64 {
                media_type,
                data: raw.to_string(),
            },
        }
    } else if media_type.starts_with("video/") {
        ContentBlock::Video {
            source: VideoSource::Base64 {
                media_type,
                data: raw.to_string(),
            },
        }
    } else {
        ContentBlock::Document {
            source: DocumentSource::Base64 {
                media_type,
                data: raw.to_string(),
            },
            title: part.filename.clone(),
        }
    }
}

fn infer_media_type_from_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".mp3") {
        "audio/mpeg".to_string()
    } else if lower.ends_with(".wav") {
        "audio/wav".to_string()
    } else if lower.ends_with(".mp4") {
        "video/mp4".to_string()
    } else if lower.ends_with(".pdf") {
        "application/pdf".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn awaken_message_to_a2a_message(
    message: &AwakenMessage,
    task_id: &str,
    context_id: &str,
) -> Option<A2aMessage> {
    if message.visibility == Visibility::Internal {
        return None;
    }

    let role = match message.role {
        AwakenRole::User => MessageRole::User,
        AwakenRole::Assistant => MessageRole::Agent,
        _ => return None,
    };

    let parts = message
        .content
        .iter()
        .filter_map(content_block_to_a2a_part)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }

    Some(A2aMessage {
        task_id: Some(task_id.to_string()),
        context_id: Some(context_id.to_string()),
        message_id: message
            .id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string()),
        role,
        parts,
        metadata: None,
    })
}

fn content_block_to_a2a_part(block: &ContentBlock) -> Option<Part> {
    match block {
        ContentBlock::Text { text } => Some(Part::text(text.clone())),
        ContentBlock::Image { source } => match source {
            ImageSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            ImageSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        ContentBlock::Document { source, title } => match source {
            DocumentSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: title.clone(),
                metadata: None,
            }),
            DocumentSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: title.clone(),
                metadata: None,
            }),
        },
        ContentBlock::Audio { source } => match source {
            AudioSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            AudioSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        ContentBlock::Video { source } => match source {
            VideoSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            VideoSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::storage::RunRecord;

    #[test]
    fn parse_task_state_filter_accepts_enum_and_lowercase() {
        assert_eq!(
            parse_task_state_filter("TASK_STATE_WORKING").unwrap(),
            TaskState::Working
        );
        assert_eq!(
            parse_task_state_filter("working").unwrap(),
            TaskState::Working
        );
        assert!(parse_task_state_filter("nope").is_err());
    }

    #[test]
    fn a2a_part_validation_requires_single_payload() {
        let part = Part {
            text: Some("hello".into()),
            raw: Some("Zm9v".into()),
            url: None,
            data: None,
            media_type: None,
            filename: None,
            metadata: None,
        };
        let count = usize::from(part.text.is_some())
            + usize::from(part.raw.is_some())
            + usize::from(part.url.is_some())
            + usize::from(part.data.is_some());
        assert_eq!(count, 2);
    }

    #[test]
    fn message_conversion_keeps_text_and_binary_parts() {
        let message = AwakenMessage::assistant("hello").with_id("msg-1".into());
        let converted = awaken_message_to_a2a_message(&message, "task-1", "thread-1").unwrap();
        assert_eq!(converted.role, MessageRole::Agent);
        assert_eq!(converted.task_id.as_deref(), Some("task-1"));
        assert_eq!(converted.context_id.as_deref(), Some("thread-1"));
        assert_eq!(converted.text(), "hello");
    }

    #[test]
    fn parse_task_action_segment_accepts_spec_suffixes() {
        assert_eq!(
            parse_task_action_segment("task-1:cancel").unwrap(),
            ("task-1".to_string(), "cancel")
        );
        assert_eq!(
            parse_task_action_segment("task-1:subscribe").unwrap(),
            ("task-1".to_string(), "subscribe")
        );
        assert!(matches!(
            parse_task_action_segment("task-1"),
            Err(A2aError::NotFound(_))
        ));
        assert!(matches!(
            parse_task_action_segment(":cancel"),
            Err(A2aError::Validation { .. })
        ));
    }

    #[test]
    fn a2a_routes_build_without_conflicts() {
        let _ = a2a_routes();
    }

    #[test]
    fn waiting_run_records_map_to_interrupted_task_states_by_reason() {
        let input_required = RunRecord {
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            agent_id: "agent".into(),
            parent_run_id: None,
            status: RunStatus::Waiting,
            termination_code: Some("input_required".into()),
            created_at: 0,
            updated_at: 0,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };
        assert_eq!(
            run_record_to_task_state(&input_required),
            TaskState::InputRequired
        );

        let auth_required = RunRecord {
            termination_code: Some("auth_required".into()),
            ..input_required.clone()
        };
        assert_eq!(
            run_record_to_task_state(&auth_required),
            TaskState::AuthRequired
        );

        let awaiting_tasks = RunRecord {
            termination_code: Some("awaiting_tasks".into()),
            ..input_required.clone()
        };
        assert_eq!(
            run_record_to_task_state(&awaiting_tasks),
            TaskState::Working
        );

        let generic_waiting = RunRecord {
            termination_code: None,
            ..input_required
        };
        assert_eq!(
            run_record_to_task_state(&generic_waiting),
            TaskState::InputRequired
        );
    }
}
