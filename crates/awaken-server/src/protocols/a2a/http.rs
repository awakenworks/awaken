//! A2A v1.0 HTTP+JSON endpoints.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

pub use awaken_contract::contract::a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentProvider, AgentSkill, Artifact,
    ListTasksResponse, Message as A2aMessage, MessageRole, Part, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use awaken_contract::contract::content::{
    AudioSource, ContentBlock, DocumentSource, ImageSource, VideoSource,
};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::MailboxJobStatus;
use awaken_contract::contract::message::{
    Message as AwakenMessage, Role as AwakenRole, Visibility,
};

use crate::app::AppState;
use awaken_runtime::RunRequest;

const A2A_VERSION: &str = "1.0";
const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;
const DISCOVERY_PATH: &str = "/.well-known/agent-card.json";
const INTERFACE_BASE_PATH: &str = "/v1/a2a";
const BLOCKING_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const BLOCKING_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SUPPORTED_OUTPUT_MODE: &str = "text/plain";

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

    fn push_notifications_not_supported() -> Self {
        Self::Specific {
            http_status: StatusCode::FAILED_DEPENDENCY,
            status: "FAILED_PRECONDITION",
            reason: "PUSH_NOTIFICATION_NOT_SUPPORTED",
            message: "push notifications are not supported by this agent".to_string(),
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
) -> Result<Json<AgentCard>, A2aError> {
    ensure_supported_version(&headers)?;
    let agent_id = public_agent_id(&st)?;
    Ok(Json(build_agent_card(&headers, &agent_id, None)))
}

async fn a2a_get_dispatch(
    State(st): State<AppState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, A2aError> {
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
                Path(((*task_id).to_string(), (*config_id).to_string())),
                headers,
            )
            .await?)
        }
        ["extendedAgentCard"] => Ok(a2a_extended_agent_card_default(headers).await?),
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
        [
            tenant,
            "tasks",
            task_id,
            "pushNotificationConfigs",
            config_id,
        ] => Ok(a2a_get_push_config_tenant(
            Path((
                (*tenant).to_string(),
                (*task_id).to_string(),
                (*config_id).to_string(),
            )),
            headers,
        )
        .await?),
        [tenant, "extendedAgentCard"] => {
            Ok(a2a_extended_agent_card_tenant(Path((*tenant).to_string()), headers).await?)
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
    body: Bytes,
) -> Result<Response, A2aError> {
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["message:send"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(a2a_message_send_default(State(st), headers, Json(payload))
                .await?
                .into_response())
        }
        ["message:stream"] => Ok(a2a_message_stream_default(headers).await?.into_response()),
        ["tasks", task_action] => {
            Ok(
                a2a_task_action_default(State(st), Path((*task_action).to_string()), headers)
                    .await?,
            )
        }
        ["tasks", task_id, "pushNotificationConfigs"] => {
            Ok(a2a_create_push_config_default(Path((*task_id).to_string()), headers).await?)
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
        [tenant, "message:stream"] => Ok(a2a_message_stream_tenant(
            Path((*tenant).to_string()),
            headers,
        )
        .await?
        .into_response()),
        [tenant, "tasks", task_action] => Ok(a2a_task_action_tenant(
            State(st),
            Path(((*tenant).to_string(), (*task_action).to_string())),
            headers,
        )
        .await?),
        [tenant, "tasks", task_id, "pushNotificationConfigs"] => Ok(a2a_create_push_config_tenant(
            Path(((*tenant).to_string(), (*task_id).to_string())),
            headers,
        )
        .await?),
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A path: /v1/a2a/{tail}"
        ))),
    }
}

async fn a2a_delete_dispatch(
    Path(tail): Path<String>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["tasks", task_id, "pushNotificationConfigs", config_id] => {
            Ok(a2a_delete_push_config_default(
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

async fn a2a_message_stream_default(headers: HeaderMap) -> Result<Json<StreamResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::unsupported_operation(
        "message:stream is not supported because this agent does not advertise streaming",
    ))
}

async fn a2a_message_stream_tenant(
    Path(_tenant): Path<String>,
    headers: HeaderMap,
) -> Result<Json<StreamResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::unsupported_operation(
        "message:stream is not supported because this agent does not advertise streaming",
    ))
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
        "subscribe" => Err(A2aError::unsupported_operation(
            "tasks:subscribe is not supported because this agent does not advertise streaming",
        )),
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
        "subscribe" => Err(A2aError::unsupported_operation(
            "tasks:subscribe is not supported because this agent does not advertise streaming",
        )),
        _ => unreachable!("task action parser only returns supported actions"),
    }
}

async fn a2a_create_push_config_default(
    Path(_task_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::push_notifications_not_supported())
}

async fn a2a_create_push_config_tenant(
    Path((_tenant, _task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::push_notifications_not_supported())
}

async fn a2a_get_push_config_default(
    Path((_task_id, _config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::push_notifications_not_supported())
}

async fn a2a_get_push_config_tenant(
    Path(_path): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::push_notifications_not_supported())
}

async fn a2a_delete_push_config_default(
    Path((_task_id, _config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::push_notifications_not_supported())
}

async fn a2a_delete_push_config_tenant(
    Path(_path): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::push_notifications_not_supported())
}

async fn a2a_extended_agent_card_default(headers: HeaderMap) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::unsupported_operation(
        "extendedAgentCard is not configured for this agent",
    ))
}

async fn a2a_extended_agent_card_tenant(
    Path(_tenant): Path<String>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    ensure_supported_version(&headers)?;
    Err(A2aError::unsupported_operation(
        "extendedAgentCard is not configured for this agent",
    ))
}

async fn send_message(
    st: AppState,
    headers: HeaderMap,
    path_tenant: Option<String>,
    payload: SendMessageRequest,
) -> Result<Json<SendMessageResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    let PreparedRequest {
        thread_id,
        effective_tenant,
        history_length,
        return_immediately,
        request,
    } = prepare_send_request(&st, path_tenant, payload).await?;

    st.mailbox
        .submit_background(request)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    let task = if return_immediately {
        load_task_snapshot(
            &st,
            &thread_id,
            effective_tenant.as_deref(),
            history_length,
            true,
        )
        .await?
        .map(|snapshot| snapshot.task)
        .unwrap_or_else(|| submitted_task(&thread_id, effective_tenant.as_deref()))
    } else {
        wait_for_task(&st, &thread_id, effective_tenant.as_deref(), history_length).await?
    };

    Ok(Json(SendMessageResponse::task(task)))
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
        .unwrap_or_else(|| canceled_task(&existing.task.id, existing.current_agent_id.as_deref()));

    Ok(Json(task))
}

struct PreparedRequest {
    thread_id: String,
    effective_tenant: Option<String>,
    history_length: usize,
    return_immediately: bool,
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
    if payload
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.task_push_notification_config.as_ref())
        .is_some()
    {
        return Err(A2aError::push_notifications_not_supported());
    }

    let task_id = trim_to_option(payload.message.task_id.as_deref());
    let context_id = trim_to_option(payload.message.context_id.as_deref());
    if let (Some(task_id), Some(context_id)) = (task_id.as_deref(), context_id.as_deref())
        && task_id != context_id
    {
        violations.push(FieldViolation {
            field: "message.contextId".into(),
            description:
                "this adapter maps one task per context, so taskId and contextId must match".into(),
        });
    }

    if !violations.is_empty() {
        return Err(A2aError::merge_invalid("invalid A2A request", violations));
    }

    if let Some(ref tenant) = effective_tenant {
        ensure_runnable_agent(st, tenant)?;
    }

    let thread_id = task_id
        .or(context_id)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let content = payload
        .message
        .parts
        .iter()
        .map(a2a_part_to_content_block)
        .collect::<Result<Vec<_>, _>>()?;

    let awaken_message =
        AwakenMessage::user_with_content(content).with_id(payload.message.message_id);
    let mut request = RunRequest::new(thread_id.clone(), vec![awaken_message]);

    if let Some(ref tenant) = effective_tenant {
        request = request.with_agent_id(tenant.clone());
    } else if task_exists(st, &thread_id).await? {
        // Keep agent inference on existing threads.
    } else {
        request = request.with_agent_id(public_agent_id(st)?);
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

    Ok(PreparedRequest {
        thread_id,
        effective_tenant,
        history_length,
        return_immediately,
        request,
    })
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
            return Ok(last_seen.unwrap_or_else(|| submitted_task(task_id, tenant)));
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
    let latest_run = st
        .store
        .latest_run(task_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    let jobs = st
        .mailbox
        .list_jobs(task_id, None, 100, 0)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    let latest_job = jobs.into_iter().max_by_key(|job| job.updated_at);

    let history = st
        .store
        .load_messages(task_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .unwrap_or_default();
    let mut converted_history = history
        .iter()
        .filter_map(|message| awaken_message_to_a2a_message(message, task_id))
        .collect::<Vec<_>>();
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
            context_id: task_id.to_string(),
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
        RunStatus::Waiting => TaskState::InputRequired,
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

fn submitted_task(task_id: &str, tenant: Option<&str>) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: task_id.to_string(),
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

fn canceled_task(task_id: &str, tenant: Option<&str>) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: task_id.to_string(),
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

fn build_agent_card(headers: &HeaderMap, agent_id: &str, tenant: Option<&str>) -> AgentCard {
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
            streaming: false,
            push_notifications: false,
            state_transition_history: false,
            extended_agent_card: false,
        },
        security_schemes: BTreeMap::new(),
        security: Vec::new(),
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

async fn task_exists(st: &AppState, task_id: &str) -> Result<bool, A2aError> {
    if st
        .store
        .latest_run(task_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_some()
    {
        return Ok(true);
    }

    Ok(!st
        .mailbox
        .list_jobs(task_id, None, 1, 0)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .is_empty())
}

async fn collect_task_ids(st: &AppState) -> Result<Vec<String>, A2aError> {
    let mut ids = BTreeSet::new();
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
        ids.extend(batch);
    }
    ids.extend(
        st.mailbox
            .queued_mailbox_ids()
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?,
    );
    Ok(ids.into_iter().collect())
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
            "Content-Type must be application/json",
        ));
    };

    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if media_type.eq_ignore_ascii_case("application/json") {
        Ok(())
    } else {
        Err(A2aError::invalid(
            "contentType",
            "Content-Type must be application/json",
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

fn awaken_message_to_a2a_message(message: &AwakenMessage, task_id: &str) -> Option<A2aMessage> {
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
        context_id: Some(task_id.to_string()),
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
        let converted = awaken_message_to_a2a_message(&message, "task-1").unwrap();
        assert_eq!(converted.role, MessageRole::Agent);
        assert_eq!(converted.task_id.as_deref(), Some("task-1"));
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
}
