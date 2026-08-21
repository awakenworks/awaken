//! The axum router: A2A `message:send` + agent-card routes over a `RunApplication`.
//!
//! Handlers decode the request, drive one turn (or resume an awaiting run on the same
//! context) through the port, and project the committed step into an A2A `Task`.
//! Errors are an HTTP status + A2A JSON error envelope — A2A `message:send` is
//! request/response, so failures are not in-stream events.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::run::{EndCause, Failure};
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use axum::Router;
use axum::body::Body;
use axum::extract::{Json, OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use awaken_session_contract::{
    EventForwardingSink, RunApplication, RunApplicationError, RunResume, StepOutcome,
};

use crate::card::agent_card;
use crate::encoder::{encode_task, working_task};
use crate::extract::A2aJson;
use crate::request::{process, resume_for_pending};
use crate::state::{A2aState, PushProtocolVersion};
use crate::state_error::{
    rpc_error, state_cancel_error, state_failure_update, state_fault_response, state_rpc_response,
    state_run_error,
};
use crate::types::{
    Artifact, DeleteTaskPushNotificationConfigParams, ErrorResponse,
    GetTaskPushNotificationConfigParams, ListPushNotificationConfigsResponse, Part,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskIdParams, TaskPushNotificationConfig, TaskQueryParams, TaskState,
};
use crate::v1::{
    agent_card_value as v1_agent_card_value, parse_push_config as parse_v1_push_config,
    push_value as v1_push_value, stream_value as v1_stream_value, task_value as v1_task_value,
};
use crate::version::{ProtocolVersion, negotiate_version};

type Runtime = A2aState;

/// Build the A2A router. Mount it alongside other protocol routers; the paths are
/// the `/v1/a2a...` surface an A2A `HTTP+JSON` client posts to.
pub fn router(runtime: Arc<dyn RunApplication>) -> Router {
    router_with_storage_root(runtime, None)
}

/// Build the A2A router with adapter projections persisted below an explicitly
/// injected deployment storage root.
///
/// Runtime/composition owns whether storage exists; this protocol adapter owns
/// only its filename and serialization format.
pub fn router_with_storage_root(
    runtime: Arc<dyn RunApplication>,
    storage_root: Option<&std::path::Path>,
) -> Router {
    let state = A2aState::new(
        runtime,
        storage_root.map(|root| root.join("a2a-state.json")),
    )
    .unwrap_or_else(|error| panic!("load A2A wire projection: {error}"));
    Router::new()
        // The JSON-RPC binding (the canonical A2A transport the official SDKs
        // default to): a single endpoint dispatching by `method`.
        .route(JSONRPC_PATH, post(jsonrpc))
        // The HTTP+JSON binding (a message posted straight to a method path).
        .route(crate::client::MESSAGE_SEND_PATH, post(message_send))
        .route("/v1/a2a/message:stream", post(message_stream))
        // Official A2A v0.3 HTTP+JSON binding.
        .route("/v1/message:send", post(message_send_rest))
        .route("/v1/message:stream", post(message_stream_rest))
        .route("/message:send", post(message_send_rest))
        .route("/message:stream", post(message_stream_rest))
        .route("/{tenant}/message:send", post(message_send_tenant_rest))
        .route("/{tenant}/message:stream", post(message_stream_tenant_rest))
        .route("/extendedAgentCard", get(crate::card::extended_card_rest))
        .route(
            "/{tenant}/extendedAgentCard",
            get(crate::card::extended_card_tenant_rest),
        )
        .route(
            "/v1/a2a/agents/{agent_id}/message:send",
            post(message_send_scoped),
        )
        .route(
            "/v1/a2a/agents/{agent_id}/message:stream",
            post(message_stream_scoped),
        )
        .route("/v1/a2a/tasks/{task_id}", get(task_get_http))
        .route("/v1/a2a/tasks/{task_id}/cancel", post(task_cancel_http))
        .route(
            "/v1/a2a/tasks/{task_id}/subscribe",
            get(task_subscribe_http).post(task_subscribe_http),
        )
        .route(
            "/v1/a2a/tasks/{task_id}/pushNotificationConfigs",
            get(list_push_configs).post(create_push_config),
        )
        .route(
            "/v1/a2a/tasks/{task_id}/pushNotificationConfigs/{config_id}",
            get(get_push_config).delete(delete_push_config),
        )
        .nest(
            "/v1/tasks",
            Router::new()
                .route(
                    "/{task_id}/pushNotificationConfigs",
                    get(list_push_configs).post(create_push_config),
                )
                .route(
                    "/{task_id}/pushNotificationConfigs/{config_id}",
                    get(get_push_config).delete(delete_push_config),
                )
                // Axum parameters must occupy a complete segment, whereas A2A
                // uses `{id}:cancel`; the fallback parses those exact URLs.
                .fallback(official_task_dispatch),
        )
        .nest(
            "/tasks",
            Router::new()
                .route("/", get(list_tasks_rest))
                .route(
                    "/{task_id}/pushNotificationConfigs",
                    get(list_push_configs_v1).post(create_push_config_v1),
                )
                .route(
                    "/{task_id}/pushNotificationConfigs/{config_id}",
                    get(get_push_config_v1).delete(delete_push_config_v1),
                )
                .fallback(v1_task_dispatch),
        )
        .route("/{tenant}/tasks", get(list_tasks_tenant_rest))
        .route(
            "/{tenant}/tasks/{task_action}",
            get(get_task_tenant_rest).post(task_action_tenant_rest),
        )
        .route(
            "/{tenant}/tasks/{task_id}/pushNotificationConfigs",
            get(list_push_configs_tenant_v1).post(create_push_config_tenant_v1),
        )
        .route(
            "/{tenant}/tasks/{task_id}/pushNotificationConfigs/{config_id}",
            get(get_push_config_tenant_v1).delete(delete_push_config_tenant_v1),
        )
        .route(crate::client::AGENT_CARD_PATH, get(crate::card::card))
        .route("/.well-known/agent-card.json", get(crate::card::card))
        .route("/v1/card", get(crate::card::card))
        .with_state(state)
}

/// The JSON-RPC service endpoint (advertised as the card's `url`).
pub const JSONRPC_PATH: &str = "/v1/a2a";

async fn message_send(
    State(rt): State<Runtime>,
    A2aJson(req): A2aJson<SendMessageRequest>,
) -> Response {
    send(rt, req, None).await
}

async fn message_send_scoped(
    State(rt): State<Runtime>,
    Path(agent_id): Path<String>,
    A2aJson(req): A2aJson<SendMessageRequest>,
) -> Response {
    send(rt, req, Some(agent_id)).await
}

async fn message_stream(
    State(rt): State<Runtime>,
    A2aJson(req): A2aJson<SendMessageRequest>,
) -> Response {
    stream_send(rt, req, None, None, false, ProtocolVersion::V03).await
}

async fn message_stream_scoped(
    State(rt): State<Runtime>,
    Path(agent_id): Path<String>,
    A2aJson(req): A2aJson<SendMessageRequest>,
) -> Response {
    stream_send(rt, req, Some(agent_id), None, false, ProtocolVersion::V03).await
}

async fn message_stream_rest(
    State(rt): State<Runtime>,
    headers: HeaderMap,
    A2aJson(params): A2aJson<Value>,
) -> Response {
    let version = match negotiate_version(&headers) {
        Ok(version) => version,
        Err(message) => return a2a_fault(StatusCode::BAD_REQUEST, -32009, message),
    };
    let req = match decode_send_params(params, version) {
        Ok(req) => req,
        Err(error) => return rest_driver_error_version(error, version),
    };
    stream_send(rt, req, None, None, true, version).await
}

async fn message_stream_tenant_rest(
    State(rt): State<Runtime>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    A2aJson(params): A2aJson<Value>,
) -> Response {
    let version = match negotiate_version(&headers) {
        Ok(version) => version,
        Err(message) => return a2a_fault(StatusCode::BAD_REQUEST, -32009, message),
    };
    let mut req = match decode_send_params(params, version) {
        Ok(req) => req,
        Err(error) => return rest_driver_error_version(error, version),
    };
    req.agent_id = Some(tenant.clone());
    stream_send(rt, req, Some(tenant), None, true, version).await
}

/// Drive one turn and transcode the neutral live stream into A2A SSE events.
/// JSON-RPC streams wrap each event in `{jsonrpc,id,result}`; REST/HTTP+JSON
/// streams carry the result object directly.
async fn stream_send(
    rt: Runtime,
    req: SendMessageRequest,
    path_agent: Option<String>,
    rpc_id: Option<Value>,
    rest_errors: bool,
    version: ProtocolVersion,
) -> Response {
    let prepared = match prepare_send(&rt, req, path_agent, version).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return stream_binding_error(error, rpc_id.as_ref(), rest_errors, version);
        }
    };
    let PreparedSend {
        processed,
        history_length,
        working,
        ..
    } = prepared;
    let thread = processed.thread_id.clone();
    let task_id = processed.task_id.clone();
    let agent_id = processed.agent_id.clone();

    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let initial = StreamResponse::Task(working);
    let _ = out_tx.send(sse_event(&initial, rpc_id.as_ref(), version));

    tokio::spawn(async move {
        let (live_tx, mut live_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let sink: Arc<dyn StreamSink> = Arc::new(EventForwardingSink::new(move |event| {
            live_tx
                .send(event)
                .map_err(|_| awaken_agent_contract::stream::sink::Error::Closed)
        }));
        let runtime = Arc::clone(&rt.runtime);
        let turn_thread = thread.clone();
        let turn_agent = agent_id.clone();
        let turn = tokio::spawn(async move {
            drive_processed_runtime(&runtime, processed, Some(sink), turn_agent, &turn_thread).await
        });

        while let Some(event) = live_rx.recv().await {
            let AgentEvent::Delta(Delta::TextDelta { delta }) = event else {
                continue;
            };
            let response = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                kind: crate::types::TaskArtifactUpdateKind::ArtifactUpdate,
                task_id: task_id.clone(),
                context_id: thread.clone(),
                artifact: Artifact {
                    artifact_id: "response".into(),
                    name: Some("response".into()),
                    description: None,
                    extensions: Vec::new(),
                    metadata: None,
                    parts: vec![Part::text(delta)],
                },
                append: Some(true),
                last_chunk: Some(false),
                metadata: None,
            });
            if let Err(error) = rt
                .publish(&task_id, agent_id.as_deref(), response.clone())
                .await
            {
                let _ = rt.runtime.interrupt(&thread).await;
                let failure = state_failure_update(&task_id, &thread, error);
                let _ = out_tx.send(sse_event(&failure, rpc_id.as_ref(), version));
                return;
            }
            if out_tx
                .send(sse_event(&response, rpc_id.as_ref(), version))
                .is_err()
            {
                let _ = rt.runtime.interrupt(&thread).await;
                return;
            }
        }

        let outcome = match turn.await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => StepOutcome::ended(
                Vec::new(),
                EndCause::Error(Failure::Inference {
                    code: "a2a_run_failed".into(),
                    message: error.to_string(),
                }),
                false,
                false,
            ),
            Err(error) => StepOutcome::ended(
                Vec::new(),
                EndCause::Error(Failure::Inference {
                    code: "a2a_run_cancelled".into(),
                    message: error.to_string(),
                }),
                false,
                false,
            ),
        };
        let (history, outcome) = match rt.runtime.history(&thread).await {
            Ok(history) => (history, outcome),
            Err(error) => (
                Vec::new(),
                StepOutcome::ended(
                    Vec::new(),
                    EndCause::Error(Failure::Inference {
                        code: error.code,
                        message: error.message,
                    }),
                    false,
                    false,
                ),
            ),
        };
        let mut task = encode_task(&thread, &history, &outcome);
        task.id = task_id.clone();
        truncate_history(&mut task, history_length);
        if let Err(error) = rt.record_task(task.clone(), agent_id.clone()).await {
            task.status.state = TaskState::Failed;
            task.status.message = Some(crate::types::Message::agent_text(
                format!("a2a-state-{}", task.id),
                error.to_string(),
            ));
            let retry_state = rt.clone();
            let retry_task = task.clone();
            let retry_agent = agent_id.clone();
            tokio::spawn(async move {
                retry_state
                    .record_task_reliably(retry_task, retry_agent)
                    .await;
            });
        }
        let terminal = StreamResponse::StatusUpdate(crate::types::TaskStatusUpdateEvent {
            kind: crate::types::TaskStatusUpdateKind::StatusUpdate,
            task_id,
            context_id: thread,
            status: task.status,
            final_: true,
            metadata: None,
        });
        let _ = out_tx.send(sse_event(&terminal, rpc_id.as_ref(), version));
    });

    stream_response(out_rx)
}

struct PreparedSend {
    processed: crate::request::Processed,
    history_length: Option<usize>,
    asynchronous: bool,
    working: Task,
}

/// One authoritative admission and A2A projection-preparation path shared by
/// request/response and streaming sends. It prevents the two transports from
/// drifting on context resolution, working-task publication, and inline push
/// configuration.
async fn prepare_send(
    rt: &Runtime,
    mut req: SendMessageRequest,
    path_agent: Option<String>,
    version: ProtocolVersion,
) -> Result<PreparedSend, RunApplicationError> {
    validate_send_configuration(&req)?;
    let history_length = req
        .configuration
        .as_ref()
        .and_then(|configuration| configuration.history_length)
        .map(|length| length as usize);
    let asynchronous = req
        .configuration
        .as_ref()
        .is_some_and(|configuration| match version {
            ProtocolVersion::V03 => configuration.blocking == Some(false),
            ProtocolVersion::V1 => configuration.return_immediately.unwrap_or(false),
        });
    let push_config = req
        .configuration
        .as_ref()
        .and_then(|configuration| configuration.task_push_notification_config.clone());
    resolve_task_context(rt, &mut req, path_agent.as_deref()).await?;
    let processed = process(req, path_agent);
    let working = working_task(&processed.task_id, &processed.thread_id);
    rt.record_task_with_config(
        working.clone(),
        processed.agent_id.clone(),
        push_config,
        version.push_version(),
    )
    .await
    .map_err(state_run_error)?;
    Ok(PreparedSend {
        processed,
        history_length,
        asynchronous,
        working,
    })
}

async fn task_get_http(State(rt): State<Runtime>, Path(task_id): Path<String>) -> Response {
    match rt.task(&task_id, None).await {
        Some(task) => Json(task).into_response(),
        None => a2a_fault(StatusCode::NOT_FOUND, -32001, "task not found"),
    }
}

async fn task_cancel_http(State(rt): State<Runtime>, Path(task_id): Path<String>) -> Response {
    match cancel_task(&rt, &task_id, None).await {
        Ok(task) => (StatusCode::ACCEPTED, Json(task)).into_response(),
        Err((status, code, message)) => a2a_fault(status, code, message),
    }
}

async fn task_subscribe_http(State(rt): State<Runtime>, Path(task_id): Path<String>) -> Response {
    subscribe_response(rt, task_id, None, None, ProtocolVersion::V03).await
}

async fn official_task_dispatch(
    State(rt): State<Runtime>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    let task_action = uri.path().strip_prefix("/v1/tasks/").unwrap_or_default();
    if request.method() == axum::http::Method::GET
        && !task_action.contains('/')
        && !task_action.contains(':')
    {
        return task_get_http(State(rt), Path(task_action.to_string())).await;
    }
    if request.method() == axum::http::Method::POST {
        if let Some(task_id) = task_action.strip_suffix(":cancel") {
            return task_cancel_http(State(rt), Path(task_id.to_string())).await;
        }
        if let Some(task_id) = task_action.strip_suffix(":subscribe") {
            return task_subscribe_http(State(rt), Path(task_id.to_string())).await;
        }
    }
    a2a_fault(StatusCode::NOT_FOUND, -32001, "task endpoint not found")
}

async fn v1_task_dispatch(
    State(rt): State<Runtime>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    let task_action = uri.path().strip_prefix("/tasks/").unwrap_or_default();
    if request.method() == axum::http::Method::GET
        && !task_action.contains('/')
        && !task_action.contains(':')
    {
        let history_length = uri
            .query()
            .and_then(|query| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("historyLength="))
            })
            .map(str::parse::<usize>)
            .transpose();
        let history_length = match history_length {
            Ok(value) => value,
            Err(_) => {
                return v1_fault(StatusCode::BAD_REQUEST, -32602, "invalid historyLength");
            }
        };
        return match rt.task(task_action, None).await {
            Some(mut task) => {
                truncate_history(&mut task, history_length);
                v1_json_response(StatusCode::OK, v1_task_value(&task))
            }
            None => v1_fault(StatusCode::NOT_FOUND, -32001, "task not found"),
        };
    }
    if request.method() == axum::http::Method::POST {
        if let Some(task_id) = task_action.strip_suffix(":cancel") {
            return match cancel_task(&rt, task_id, None).await {
                Ok(task) => v1_json_response(StatusCode::OK, v1_task_value(&task)),
                Err((status, code, message)) => v1_fault(status, code, message),
            };
        }
        if let Some(task_id) = task_action.strip_suffix(":subscribe") {
            return subscribe_response(rt, task_id.to_string(), None, None, ProtocolVersion::V1)
                .await;
        }
    }
    v1_fault(StatusCode::NOT_FOUND, -32001, "task endpoint not found")
}

#[allow(clippy::result_large_err)]
fn require_v1(headers: &HeaderMap) -> Result<(), Response> {
    match negotiate_version(headers) {
        Ok(ProtocolVersion::V1) => Ok(()),
        Ok(ProtocolVersion::V03) => Err(v1_fault(
            StatusCode::BAD_REQUEST,
            -32009,
            "this endpoint requires A2A-Version: 1.0",
        )),
        Err(message) => Err(v1_fault(StatusCode::BAD_REQUEST, -32009, message)),
    }
}

async fn list_tasks_rest(
    State(rt): State<Runtime>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match list_tasks_value(&rt, None, &query).await {
        Ok(value) => v1_json_response(StatusCode::OK, value),
        Err(message) => v1_fault(StatusCode::BAD_REQUEST, -32602, message),
    }
}

async fn list_tasks_tenant_rest(
    State(rt): State<Runtime>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match list_tasks_value(&rt, Some(&tenant), &query).await {
        Ok(value) => v1_json_response(StatusCode::OK, value),
        Err(message) => v1_fault(StatusCode::BAD_REQUEST, -32602, message),
    }
}

async fn get_task_tenant_rest(
    State(rt): State<Runtime>,
    Path((tenant, task_action)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    if task_action.contains(':') {
        return v1_fault(StatusCode::NOT_FOUND, -32001, "task endpoint not found");
    }
    let history_length = match history_length_from_query(&query) {
        Ok(value) => value,
        Err(message) => return v1_fault(StatusCode::BAD_REQUEST, -32602, message),
    };
    match rt.task(&task_action, Some(&tenant)).await {
        Some(mut task) => {
            truncate_history(&mut task, history_length);
            v1_json_response(StatusCode::OK, v1_task_value(&task))
        }
        None => v1_fault(StatusCode::NOT_FOUND, -32001, "task not found"),
    }
}

async fn task_action_tenant_rest(
    State(rt): State<Runtime>,
    Path((tenant, task_action)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    if let Some(task_id) = task_action.strip_suffix(":cancel") {
        return match cancel_task(&rt, task_id, Some(&tenant)).await {
            Ok(task) => v1_json_response(StatusCode::OK, v1_task_value(&task)),
            Err((status, code, message)) => v1_fault(status, code, message),
        };
    }
    if let Some(task_id) = task_action.strip_suffix(":subscribe") {
        return subscribe_response(
            rt,
            task_id.to_string(),
            Some(tenant),
            None,
            ProtocolVersion::V1,
        )
        .await;
    }
    v1_fault(StatusCode::NOT_FOUND, -32001, "task endpoint not found")
}

async fn create_push_config_v1(
    State(rt): State<Runtime>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    A2aJson(mut value): A2aJson<Value>,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    value["taskId"] = Value::String(task_id.clone());
    match parse_v1_push_config(&value) {
        Ok((payload, owner)) => match rt
            .upsert_config(
                &task_id,
                owner.as_deref(),
                payload.push_notification_config,
                PushProtocolVersion::V1,
            )
            .await
        {
            Ok(config) => v1_json_response(
                StatusCode::CREATED,
                v1_push_value(&task_id, owner.as_deref(), &config),
            ),
            Err(error) => state_fault_response(error, ProtocolVersion::V1),
        },
        Err(error) => v1_fault(StatusCode::BAD_REQUEST, -32602, error),
    }
}

async fn list_push_configs_v1(
    State(rt): State<Runtime>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match rt.configs(&task_id, None).await {
        Some(configs) => v1_json_response(
            StatusCode::OK,
            json!({
                "configs": configs.iter().map(|config| v1_push_value(&task_id, None, config)).collect::<Vec<_>>(),
                "nextPageToken": "",
            }),
        ),
        None => v1_fault(StatusCode::NOT_FOUND, -32001, "task not found"),
    }
}

async fn get_push_config_v1(
    State(rt): State<Runtime>,
    Path((task_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match rt.config(&task_id, &config_id, None).await {
        Some(config) => v1_json_response(StatusCode::OK, v1_push_value(&task_id, None, &config)),
        None => v1_fault(StatusCode::NOT_FOUND, -32001, "config not found"),
    }
}

async fn delete_push_config_v1(
    State(rt): State<Runtime>,
    Path((task_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match rt.delete_config(&task_id, &config_id, None).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => v1_fault(StatusCode::NOT_FOUND, -32001, "config not found"),
        Err(error) => state_fault_response(error, ProtocolVersion::V1),
    }
}

async fn create_push_config_tenant_v1(
    State(rt): State<Runtime>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    A2aJson(mut value): A2aJson<Value>,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    if value
        .get("tenant")
        .and_then(Value::as_str)
        .is_some_and(|body_tenant| !body_tenant.is_empty() && body_tenant != tenant)
    {
        return v1_fault(StatusCode::BAD_REQUEST, -32602, "tenant must match the URL");
    }
    value["tenant"] = Value::String(tenant.clone());
    value["taskId"] = Value::String(task_id.clone());
    match parse_v1_push_config(&value) {
        Ok((payload, _)) => match rt
            .upsert_config(
                &task_id,
                Some(&tenant),
                payload.push_notification_config,
                PushProtocolVersion::V1,
            )
            .await
        {
            Ok(config) => v1_json_response(
                StatusCode::CREATED,
                v1_push_value(&task_id, Some(&tenant), &config),
            ),
            Err(error) => state_fault_response(error, ProtocolVersion::V1),
        },
        Err(error) => v1_fault(StatusCode::BAD_REQUEST, -32602, error),
    }
}

async fn list_push_configs_tenant_v1(
    State(rt): State<Runtime>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match rt.configs(&task_id, Some(&tenant)).await {
        Some(configs) => v1_json_response(
            StatusCode::OK,
            json!({
                "configs": configs.iter().map(|config| v1_push_value(&task_id, Some(&tenant), config)).collect::<Vec<_>>(),
                "nextPageToken": "",
            }),
        ),
        None => v1_fault(StatusCode::NOT_FOUND, -32001, "task not found"),
    }
}

async fn get_push_config_tenant_v1(
    State(rt): State<Runtime>,
    Path((tenant, task_id, config_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match rt.config(&task_id, &config_id, Some(&tenant)).await {
        Some(config) => v1_json_response(
            StatusCode::OK,
            v1_push_value(&task_id, Some(&tenant), &config),
        ),
        None => v1_fault(StatusCode::NOT_FOUND, -32001, "config not found"),
    }
}

async fn delete_push_config_tenant_v1(
    State(rt): State<Runtime>,
    Path((tenant, task_id, config_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_v1(&headers) {
        return response;
    }
    match rt.delete_config(&task_id, &config_id, Some(&tenant)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => v1_fault(StatusCode::NOT_FOUND, -32001, "config not found"),
        Err(error) => state_fault_response(error, ProtocolVersion::V1),
    }
}

async fn subscribe_response(
    rt: Runtime,
    task_id: String,
    agent_id: Option<String>,
    rpc_id: Option<Value>,
    version: ProtocolVersion,
) -> Response {
    let Some((task, mut receiver)) = rt.subscribe(&task_id, agent_id.as_deref()).await else {
        return version_fault(version, StatusCode::NOT_FOUND, -32001, "task not found");
    };
    if task.status.state.is_terminal() {
        return version_fault(
            version,
            StatusCode::BAD_REQUEST,
            -32002,
            "task is not subscribable",
        );
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let initial = StreamResponse::Task(task);
    let _ = tx.send(sse_event(&initial, rpc_id.as_ref(), version));
    tokio::spawn(async move {
        while let Ok(response) = receiver.recv().await {
            let terminal = response.is_terminal();
            if tx
                .send(sse_event(&response, rpc_id.as_ref(), version))
                .is_err()
                || terminal
            {
                break;
            }
        }
    });
    stream_response(rx)
}

/// The core send logic (shared by the HTTP+JSON and JSON-RPC bindings). A message
/// on a thread with an awaiting run resumes it (delivering the text as the tool
/// answer); otherwise it is a fresh turn. Either way the committed step is
/// projected into a `Task`.
async fn run_send(
    rt: &Runtime,
    req: SendMessageRequest,
    path_agent: Option<String>,
    version: ProtocolVersion,
) -> Result<Task, RunApplicationError> {
    let PreparedSend {
        processed,
        history_length,
        asynchronous,
        working,
    } = prepare_send(rt, req, path_agent, version).await?;
    let thread = processed.thread_id.clone();
    let task_id = processed.task_id.clone();
    let agent_id = processed.agent_id.clone();

    if asynchronous {
        let rt = rt.clone();
        let agent_id = agent_id.clone();
        tokio::spawn(async move {
            let step = drive_processed(&rt, processed)
                .await
                .unwrap_or_else(|error| {
                    StepOutcome::ended(
                        Vec::new(),
                        EndCause::Error(Failure::Inference {
                            code: "a2a_run_failed".into(),
                            message: error.to_string(),
                        }),
                        false,
                        false,
                    )
                });
            let (history, step) = match rt.runtime.history(&thread).await {
                Ok(history) => (history, step),
                Err(error) => (
                    Vec::new(),
                    StepOutcome::ended(
                        Vec::new(),
                        EndCause::Error(Failure::Inference {
                            code: error.code,
                            message: error.message,
                        }),
                        false,
                        false,
                    ),
                ),
            };
            let mut task = encode_task(&thread, &history, &step);
            task.id = task_id;
            truncate_history(&mut task, history_length);
            rt.record_task_reliably(task, agent_id).await;
        });
        return Ok(working);
    }

    let step = drive_processed(rt, processed).await?;

    let history = rt.runtime.history(&thread).await?;
    let mut task = encode_task(&thread, &history, &step);
    task.id = task_id;
    truncate_history(&mut task, history_length);
    rt.record_task(task.clone(), agent_id)
        .await
        .map_err(state_run_error)?;
    Ok(task)
}

async fn drive_processed(
    rt: &Runtime,
    processed: crate::request::Processed,
) -> Result<StepOutcome, RunApplicationError> {
    let thread = processed.thread_id.clone();
    let agent_id = processed.agent_id.clone();
    drive_processed_runtime(&rt.runtime, processed, None, agent_id, &thread).await
}

async fn drive_processed_runtime(
    runtime: &Arc<dyn RunApplication>,
    processed: crate::request::Processed,
    sink: Option<Arc<dyn StreamSink>>,
    agent_id: Option<String>,
    thread: &str,
) -> Result<StepOutcome, RunApplicationError> {
    match runtime.pending(thread).await? {
        // A awaiting run on this context → the message is the awaited input.
        Some(pending) => {
            let resume =
                resume_for_pending(&processed.text, processed.approval.as_ref(), &pending)?;
            runtime.resume(thread, &pending.tool_use_id, resume).await
        }
        // No awaiting run → a fresh turn.
        None if processed.approval.is_some() => Err(RunApplicationError::bad_request(
            "A2A tool-approval decision has no awaiting tool",
        )),
        None => match sink {
            Some(sink) => {
                runtime
                    .run_streaming(thread, agent_id, vec![processed.message], sink)
                    .await
            }
            None => runtime.run(thread, agent_id, vec![processed.message]).await,
        },
    }
}

fn validate_send_configuration(req: &SendMessageRequest) -> Result<(), RunApplicationError> {
    let Some(configuration) = &req.configuration else {
        return Ok(());
    };
    if !configuration.accepted_output_modes.is_empty()
        && !configuration.accepted_output_modes.iter().any(|mode| {
            matches!(
                mode.as_str(),
                "text/plain" | "text/markdown" | "application/json"
            )
        })
    {
        return Err(RunApplicationError::bad_request(format!(
            "unsupported output modes: {}",
            configuration.accepted_output_modes.join(", ")
        )));
    }
    Ok(())
}

fn truncate_history(task: &mut Task, history_length: Option<usize>) {
    if let Some(length) = history_length
        && task.history.len() > length
    {
        task.history = task.history.split_off(task.history.len() - length);
    }
}

fn history_length_from_query(query: &HashMap<String, String>) -> Result<Option<usize>, String> {
    query
        .get("historyLength")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid historyLength".to_string())
}

async fn resolve_task_context(
    rt: &Runtime,
    req: &mut SendMessageRequest,
    path_agent: Option<&str>,
) -> Result<(), RunApplicationError> {
    let Some(task_id) = req.message.task_id.as_deref() else {
        return Ok(());
    };
    let owner = path_agent.or(req.agent_id.as_deref());
    let Some(task) = rt.task(task_id, owner).await else {
        return Err(RunApplicationError::bad_request(format!(
            "task not found: {task_id}"
        )));
    };
    if task.status.state.is_terminal() {
        return Err(RunApplicationError::bad_request(format!(
            "task is terminal and cannot accept another message: {task_id}"
        )));
    }
    match req.message.context_id.as_deref() {
        Some(context_id) if context_id != task.context_id => Err(RunApplicationError::bad_request(
            "message contextId does not match the referenced task",
        )),
        Some(_) => Ok(()),
        None => {
            req.message.context_id = Some(task.context_id);
            Ok(())
        }
    }
}

/// The HTTP+JSON `message:send` handler.
async fn send(rt: Runtime, req: SendMessageRequest, path_agent: Option<String>) -> Response {
    match run_send(&rt, req, path_agent, ProtocolVersion::V03).await {
        Ok(task) => (StatusCode::OK, Json(SendMessageResponse { task })).into_response(),
        Err(err) => error_response(err),
    }
}

fn decode_send_params(
    params: Value,
    version: ProtocolVersion,
) -> Result<SendMessageRequest, RunApplicationError> {
    let params = if version == ProtocolVersion::V1 {
        crate::v1::normalize_send_params(params).map_err(RunApplicationError::bad_request)?
    } else {
        params
    };
    serde_json::from_value(params)
        .map_err(|error| RunApplicationError::bad_request(format!("invalid params: {error}")))
}

async fn message_send_rest(
    State(rt): State<Runtime>,
    headers: HeaderMap,
    A2aJson(params): A2aJson<Value>,
) -> Response {
    let version = match negotiate_version(&headers) {
        Ok(version) => version,
        Err(message) => return a2a_fault(StatusCode::BAD_REQUEST, -32009, message),
    };
    let req = match decode_send_params(params, version) {
        Ok(req) => req,
        Err(error) => return rest_driver_error_version(error, version),
    };
    match run_send(&rt, req, None, version).await {
        Ok(task) if version == ProtocolVersion::V1 => {
            v1_json_response(StatusCode::OK, json!({ "task": v1_task_value(&task) }))
        }
        Ok(task) => (StatusCode::OK, Json(SendMessageResponse { task })).into_response(),
        Err(error) => rest_driver_error_version(error, version),
    }
}

async fn message_send_tenant_rest(
    State(rt): State<Runtime>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    A2aJson(params): A2aJson<Value>,
) -> Response {
    let version = match negotiate_version(&headers) {
        Ok(version) => version,
        Err(message) => return a2a_fault(StatusCode::BAD_REQUEST, -32009, message),
    };
    let mut req = match decode_send_params(params, version) {
        Ok(req) => req,
        Err(error) => return rest_driver_error_version(error, version),
    };
    req.agent_id = Some(tenant.clone());
    match run_send(&rt, req, Some(tenant), version).await {
        Ok(task) if version == ProtocolVersion::V1 => {
            v1_json_response(StatusCode::OK, json!({ "task": v1_task_value(&task) }))
        }
        Ok(task) => (StatusCode::OK, Json(SendMessageResponse { task })).into_response(),
        Err(error) => rest_driver_error_version(error, version),
    }
}

async fn create_push_config(
    State(rt): State<Runtime>,
    Path(task_id): Path<String>,
    A2aJson(payload): A2aJson<TaskPushNotificationConfig>,
) -> Response {
    if payload.task_id != task_id {
        return a2a_fault(
            StatusCode::BAD_REQUEST,
            -32602,
            "taskId must match the enclosing task",
        );
    }
    match rt
        .upsert_config(
            &task_id,
            None,
            payload.push_notification_config,
            PushProtocolVersion::V03,
        )
        .await
    {
        Ok(config) => Json(TaskPushNotificationConfig {
            task_id,
            push_notification_config: config.redacted(),
        })
        .into_response(),
        Err(error) => state_fault_response(error, ProtocolVersion::V03),
    }
}

async fn list_push_configs(State(rt): State<Runtime>, Path(task_id): Path<String>) -> Response {
    match rt.configs(&task_id, None).await {
        Some(configs) => Json(ListPushNotificationConfigsResponse {
            configs: configs
                .into_iter()
                .map(|push_notification_config| TaskPushNotificationConfig {
                    task_id: task_id.clone(),
                    push_notification_config,
                })
                .collect(),
            next_page_token: String::new(),
        })
        .into_response(),
        None => a2a_fault(StatusCode::NOT_FOUND, -32001, "task not found"),
    }
}

async fn get_push_config(
    State(rt): State<Runtime>,
    Path((task_id, config_id)): Path<(String, String)>,
) -> Response {
    match rt.config(&task_id, &config_id, None).await {
        Some(push_notification_config) => Json(TaskPushNotificationConfig {
            task_id,
            push_notification_config,
        })
        .into_response(),
        None => a2a_fault(
            StatusCode::NOT_FOUND,
            -32001,
            "task or push notification config not found",
        ),
    }
}

async fn delete_push_config(
    State(rt): State<Runtime>,
    Path((task_id, config_id)): Path<(String, String)>,
) -> Response {
    match rt.delete_config(&task_id, &config_id, None).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => a2a_fault(
            StatusCode::NOT_FOUND,
            -32001,
            "task or push notification config not found",
        ),
        Err(error) => state_fault_response(error, ProtocolVersion::V03),
    }
}

/// Project the current state of the task on the context recovered from `id`
/// (`task-{thread}`): an awaiting run reads back as `input-required`, otherwise the
/// committed history is a `completed` task.
async fn get_task(rt: &Runtime, id: &str, owner: Option<&str>) -> Option<Task> {
    rt.task(id, owner).await
}

/// Cancel the task on the context recovered from `id` (`task-{thread}`). A2A has
/// no in-band "deny" for a built-in tool approval; canceling the task is the
/// protocol-native way to reject it: an awaiting run is denied (unblocked with
/// `allow: false`) and the task reads back `canceled`. A task with nothing awaiting
/// is returned in its current state (not falsely canceled).
async fn cancel_task(
    rt: &Runtime,
    id: &str,
    owner: Option<&str>,
) -> Result<Task, (StatusCode, i32, String)> {
    let Some(mut task) = rt.task(id, owner).await else {
        return Err((StatusCode::NOT_FOUND, -32001, "task not found".into()));
    };
    if task.status.state.is_terminal() {
        return Err((
            StatusCode::BAD_REQUEST,
            -32002,
            "task is not cancelable".into(),
        ));
    }
    if let Some(pending) = rt
        .runtime
        .pending(&task.context_id)
        .await
        .map_err(cancel_driver_error)?
    {
        let _ = rt
            .runtime
            .resume(
                &task.context_id,
                &pending.tool_use_id,
                RunResume::Permission(PermissionDecision::Deny {
                    reason: Some("task canceled by the client".into()),
                }),
            )
            .await;
    } else {
        let _ = rt.runtime.interrupt(&task.context_id).await;
    }
    task.status.state = TaskState::Canceled;
    task.status.timestamp = Some(crate::time::now_rfc3339());
    rt.record_task(task.clone(), owner.map(ToOwned::to_owned))
        .await
        .map_err(state_cancel_error)?;
    Ok(task)
}

/// A minimal JSON-RPC 2.0 request envelope (the fields the A2A binding uses).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: JsonRpcRequestId,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A2A requests require the JSON-RPC identifier and permit only the two JSON-RPC
/// scalar identifier forms. `null`, objects and arrays cannot leak into replies.
#[derive(Deserialize)]
#[serde(untagged)]
enum JsonRpcRequestId {
    String(String),
    Number(serde_json::Number),
}

impl From<JsonRpcRequestId> for Value {
    fn from(id: JsonRpcRequestId) -> Self {
        match id {
            JsonRpcRequestId::String(value) => Self::String(value),
            JsonRpcRequestId::Number(value) => Self::Number(value),
        }
    }
}

/// The JSON-RPC endpoint: dispatch by `method`. `message/send` drives a turn;
/// `tasks/get` reads a task's state; `tasks/cancel` cancels/denies an awaiting task;
/// other methods return a JSON-RPC "method not found".
async fn jsonrpc(
    State(rt): State<Runtime>,
    headers: HeaderMap,
    A2aJson(req): A2aJson<JsonRpcRequest>,
) -> Response {
    let id = Value::from(req.id);
    if req.jsonrpc != "2.0" {
        return rpc_error(id, -32600, "invalid JSON-RPC version; expected 2.0");
    }
    let version = match negotiate_version(&headers) {
        Ok(version) => version,
        Err(message) => return rpc_error(id, -32009, message),
    };
    let Some(method) = canonical_method(version, &req.method) else {
        return rpc_error(id, -32601, format!("method not found: {}", req.method));
    };
    match method {
        "message/send" => match decode_send_params(req.params, version) {
            Ok(send_req) => match run_send(&rt, send_req, None, version).await {
                // A2A `message/send` returns the Task (or Message) directly as
                // the JSON-RPC `result`.
                Ok(task) if version == ProtocolVersion::V1 => {
                    rpc_ok(id, json!({ "task": v1_task_value(&task) }))
                }
                Ok(task) => rpc_ok(id, task),
                Err(err) => {
                    let (code, message) = rpc_fault(err);
                    rpc_error(id, code, message)
                }
            },
            Err(err) => rpc_error(id, -32602, err.to_string()),
        },
        "message/stream" => match decode_send_params(req.params, version) {
            Ok(send_req) => stream_send(rt, send_req, None, Some(id), false, version).await,
            Err(err) => rpc_error(id, -32602, err.to_string()),
        },
        "tasks/get" if version == ProtocolVersion::V03 => {
            match serde_json::from_value::<TaskQueryParams>(req.params) {
                Ok(params) => match get_task(&rt, &params.id, None).await {
                    Some(mut task) => {
                        truncate_history(
                            &mut task,
                            params.history_length.map(|value| value as usize),
                        );
                        rpc_ok(id, task)
                    }
                    None => rpc_error(id, -32001, "task not found"),
                },
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/get" => match req.params.get("id").and_then(|v| v.as_str()) {
            Some(task_id) => match get_task(
                &rt,
                task_id,
                req.params
                    .get("tenant")
                    .and_then(Value::as_str)
                    .filter(|tenant| !tenant.is_empty()),
            )
            .await
            {
                Some(mut task) if version == ProtocolVersion::V1 => {
                    let history_length = req
                        .params
                        .get("historyLength")
                        .and_then(Value::as_u64)
                        .map(|length| length as usize);
                    truncate_history(&mut task, history_length);
                    rpc_ok(id, v1_task_value(&task))
                }
                Some(task) => rpc_ok(id, task),
                None => rpc_error(id, -32001, "task not found"),
            },
            None => rpc_error(id, -32602, "invalid params: missing task `id`".to_string()),
        },
        "tasks/list" => list_tasks_rpc(&rt, id, &req.params).await,
        "tasks/cancel" if version == ProtocolVersion::V03 => {
            match serde_json::from_value::<TaskIdParams>(req.params) {
                Ok(params) => match cancel_task(&rt, &params.id, None).await {
                    Ok(task) => rpc_ok(id, task),
                    Err((_, code, message)) => rpc_error(id, code, message),
                },
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/cancel" => match req.params.get("id").and_then(|v| v.as_str()) {
            Some(task_id) => match cancel_task(
                &rt,
                task_id,
                req.params
                    .get("tenant")
                    .and_then(Value::as_str)
                    .filter(|tenant| !tenant.is_empty()),
            )
            .await
            {
                Ok(task) if version == ProtocolVersion::V1 => rpc_ok(id, v1_task_value(&task)),
                Ok(task) => rpc_ok(id, task),
                Err((_, code, message)) => rpc_error(id, code, message),
            },
            None => rpc_error(id, -32602, "invalid params: missing task `id`".to_string()),
        },
        "tasks/resubscribe" if version == ProtocolVersion::V03 => {
            match serde_json::from_value::<TaskIdParams>(req.params) {
                Ok(params) => subscribe_response(rt, params.id, None, Some(id), version).await,
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/resubscribe" => match req.params.get("id").and_then(|v| v.as_str()) {
            Some(task_id) => {
                subscribe_response(
                    rt,
                    task_id.to_string(),
                    req.params
                        .get("tenant")
                        .and_then(Value::as_str)
                        .filter(|tenant| !tenant.is_empty())
                        .map(ToOwned::to_owned),
                    Some(id),
                    version,
                )
                .await
            }
            None => rpc_error(id, -32602, "invalid params: missing task `id`".to_string()),
        },
        "tasks/pushNotificationConfig/set" => {
            let parsed = if version == ProtocolVersion::V1 {
                parse_v1_push_config(&req.params)
            } else {
                serde_json::from_value::<TaskPushNotificationConfig>(req.params)
                    .map(|payload| (payload, None))
                    .map_err(|error| error.to_string())
            };
            match parsed {
                Ok((payload, owner)) => {
                    let task_id = payload.task_id;
                    match rt
                        .upsert_config(
                            &task_id,
                            owner.as_deref(),
                            payload.push_notification_config,
                            version.push_version(),
                        )
                        .await
                    {
                        Ok(config) if version == ProtocolVersion::V1 => {
                            rpc_ok(id, v1_push_value(&task_id, owner.as_deref(), &config))
                        }
                        Ok(config) => rpc_ok(
                            id,
                            TaskPushNotificationConfig {
                                task_id,
                                push_notification_config: config.redacted(),
                            },
                        ),
                        Err(error) => state_rpc_response(id, error),
                    }
                }
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/pushNotificationConfig/get" if version == ProtocolVersion::V03 => {
            match serde_json::from_value::<GetTaskPushNotificationConfigParams>(req.params) {
                Ok(params) => match params.push_notification_config_id {
                    Some(config_id) => match rt.config(&params.id, &config_id, None).await {
                        Some(config) => rpc_ok(
                            id,
                            TaskPushNotificationConfig {
                                task_id: params.id,
                                push_notification_config: config,
                            },
                        ),
                        None => rpc_error(id, -32001, "push notification config not found"),
                    },
                    None => match rt.configs(&params.id, None).await {
                        Some(configs) if configs.len() == 1 => rpc_ok(
                            id,
                            TaskPushNotificationConfig {
                                task_id: params.id,
                                push_notification_config: configs.into_iter().next().unwrap(),
                            },
                        ),
                        Some(_) => rpc_error(
                            id,
                            -32602,
                            "pushNotificationConfigId is required when a task has multiple configs",
                        ),
                        None => rpc_error(id, -32001, "task not found"),
                    },
                },
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/pushNotificationConfig/get" => {
            let task_id = req
                .params
                .get(if version == ProtocolVersion::V1 {
                    "taskId"
                } else {
                    "id"
                })
                .and_then(Value::as_str);
            let config_id = req
                .params
                .get(if version == ProtocolVersion::V1 {
                    "id"
                } else {
                    "pushNotificationConfigId"
                })
                .and_then(Value::as_str);
            let owner = req
                .params
                .get("tenant")
                .and_then(Value::as_str)
                .filter(|tenant| !tenant.is_empty());
            match (task_id, config_id) {
                (Some(task_id), Some(config_id)) => {
                    match rt.config(task_id, config_id, owner).await {
                        Some(config) if version == ProtocolVersion::V1 => {
                            rpc_ok(id, v1_push_value(task_id, owner, &config))
                        }
                        Some(config) => rpc_ok(
                            id,
                            TaskPushNotificationConfig {
                                task_id: task_id.to_string(),
                                push_notification_config: config,
                            },
                        ),
                        None => rpc_error(id, -32001, "push notification config not found"),
                    }
                }
                (Some(task_id), None) => match rt.configs(task_id, None).await {
                    Some(configs) if configs.len() == 1 => rpc_ok(
                        id,
                        TaskPushNotificationConfig {
                            task_id: task_id.to_string(),
                            push_notification_config: configs.into_iter().next().unwrap(),
                        },
                    ),
                    Some(_) => rpc_error(
                        id,
                        -32602,
                        "pushNotificationConfigId is required when a task has multiple configs",
                    ),
                    None => rpc_error(id, -32001, "task not found"),
                },
                _ => rpc_error(id, -32602, "invalid params: missing task `id`"),
            }
        }
        "tasks/pushNotificationConfig/list" if version == ProtocolVersion::V03 => {
            match serde_json::from_value::<TaskIdParams>(req.params) {
                Ok(params) => match rt.configs(&params.id, None).await {
                    Some(configs) => rpc_ok(
                        id,
                        configs
                            .into_iter()
                            .map(|push_notification_config| TaskPushNotificationConfig {
                                task_id: params.id.clone(),
                                push_notification_config,
                            })
                            .collect::<Vec<_>>(),
                    ),
                    None => rpc_error(id, -32001, "task not found"),
                },
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/pushNotificationConfig/list" => match req
            .params
            .get(if version == ProtocolVersion::V1 {
                "taskId"
            } else {
                "id"
            })
            .and_then(Value::as_str)
        {
            Some(task_id) => {
                let owner = req
                    .params
                    .get("tenant")
                    .and_then(Value::as_str)
                    .filter(|tenant| !tenant.is_empty());
                match rt.configs(task_id, owner).await {
                    Some(configs) if version == ProtocolVersion::V1 => rpc_ok(
                        id,
                        json!({
                            "configs": configs.iter().map(|config| v1_push_value(task_id, owner, config)).collect::<Vec<_>>(),
                            "nextPageToken": "",
                        }),
                    ),
                    Some(configs) => rpc_ok(
                        id,
                        configs
                            .into_iter()
                            .map(|push_notification_config| TaskPushNotificationConfig {
                                task_id: task_id.to_string(),
                                push_notification_config,
                            })
                            .collect::<Vec<_>>(),
                    ),
                    None => rpc_error(id, -32001, "task not found"),
                }
            }
            None => rpc_error(id, -32602, "invalid params: missing task `id`"),
        },
        "tasks/pushNotificationConfig/delete" if version == ProtocolVersion::V03 => {
            match serde_json::from_value::<DeleteTaskPushNotificationConfigParams>(req.params) {
                Ok(params) => {
                    match rt
                        .delete_config(&params.id, &params.push_notification_config_id, None)
                        .await
                    {
                        Ok(true) => rpc_ok(id, Value::Null),
                        Ok(false) => rpc_error(id, -32001, "push notification config not found"),
                        Err(error) => state_rpc_response(id, error),
                    }
                }
                Err(error) => rpc_error(id, -32602, format!("invalid params: {error}")),
            }
        }
        "tasks/pushNotificationConfig/delete" => {
            let task_id = req
                .params
                .get(if version == ProtocolVersion::V1 {
                    "taskId"
                } else {
                    "id"
                })
                .and_then(Value::as_str);
            let config_id = req
                .params
                .get(if version == ProtocolVersion::V1 {
                    "id"
                } else {
                    "pushNotificationConfigId"
                })
                .and_then(Value::as_str);
            let owner = req
                .params
                .get("tenant")
                .and_then(Value::as_str)
                .filter(|tenant| !tenant.is_empty());
            match (task_id, config_id) {
                (Some(task_id), Some(config_id)) => {
                    match rt.delete_config(task_id, config_id, owner).await {
                        Ok(true) => rpc_ok(id, Value::Null),
                        Ok(false) => rpc_error(id, -32001, "push notification config not found"),
                        Err(error) => state_rpc_response(id, error),
                    }
                }
                _ => rpc_error(id, -32602, "invalid params"),
            }
        }
        "agent/getAuthenticatedExtendedCard" => {
            let host = headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("localhost");
            let origin = format!("http://{host}");
            if version == ProtocolVersion::V1 {
                rpc_ok(id, v1_agent_card_value(&rt.runtime.model(), &origin))
            } else {
                let mut card = agent_card(&rt.runtime.model());
                card.url = format!("{origin}{JSONRPC_PATH}");
                rpc_ok(id, card)
            }
        }
        _ => unreachable!("canonical methods are exhaustively dispatched"),
    }
}

async fn list_tasks_rpc(rt: &Runtime, id: Value, params: &Value) -> Response {
    let tenant = params
        .get("tenant")
        .and_then(Value::as_str)
        .filter(|tenant| !tenant.is_empty());
    let context_id = params
        .get("contextId")
        .and_then(Value::as_str)
        .filter(|context| !context.is_empty());
    let state = match params.get("status") {
        Some(Value::String(state)) if !state.is_empty() && state != "TASK_STATE_UNSPECIFIED" => {
            match serde_json::from_value::<TaskState>(Value::String(state.clone())) {
                Ok(state) => Some(state),
                Err(error) => return rpc_error(id, -32602, format!("invalid status: {error}")),
            }
        }
        _ => None,
    };
    let page_size = params.get("pageSize").and_then(Value::as_u64).unwrap_or(50);
    if !(1..=100).contains(&page_size) {
        return rpc_error(id, -32602, "pageSize must be between 1 and 100");
    }
    let offset = params
        .get("pageToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::parse::<usize>)
        .transpose();
    let offset = match offset {
        Ok(offset) => offset.unwrap_or(0),
        Err(_) => return rpc_error(id, -32602, "invalid pageToken"),
    };
    let history_length = params
        .get("historyLength")
        .and_then(Value::as_u64)
        .map(|length| length as usize);
    let include_artifacts = params
        .get("includeArtifacts")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let timestamp_after = match crate::time::parse_optional_rfc3339(
        params.get("statusTimestampAfter").and_then(Value::as_str),
    ) {
        Ok(value) => value,
        Err(message) => return rpc_error(id, -32602, message),
    };
    let mut tasks = rt.tasks(tenant).await;
    tasks.retain(|task| {
        context_id.is_none_or(|context| task.context_id == context)
            && state.is_none_or(|state| task.status.state == state)
            && crate::time::is_after(task.status.timestamp.as_deref(), timestamp_after)
    });
    let total_size = tasks.len();
    let mut page = tasks
        .into_iter()
        .skip(offset)
        .take(page_size as usize)
        .collect::<Vec<_>>();
    for task in &mut page {
        if let Some(length) = history_length
            && task.history.len() > length
        {
            task.history = task.history.split_off(task.history.len() - length);
        }
        if !include_artifacts {
            task.artifacts.clear();
        }
    }
    let next_offset = offset + page.len();
    rpc_ok(
        id,
        json!({
            "tasks": page.iter().map(v1_task_value).collect::<Vec<_>>(),
            "nextPageToken": if next_offset < total_size { next_offset.to_string() } else { String::new() },
            "pageSize": page_size,
            "totalSize": total_size,
        }),
    )
}

async fn list_tasks_value(
    rt: &Runtime,
    owner: Option<&str>,
    query: &HashMap<String, String>,
) -> Result<Value, String> {
    let context = query.get("contextId").filter(|value| !value.is_empty());
    let state = query
        .get("status")
        .filter(|value| !value.is_empty() && value.as_str() != "TASK_STATE_UNSPECIFIED")
        .map(|state| serde_json::from_value::<TaskState>(Value::String(state.clone())))
        .transpose()
        .map_err(|error| format!("invalid status: {error}"))?;
    let page_size = query
        .get("pageSize")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid pageSize".to_string())?
        .unwrap_or(50);
    if !(1..=100).contains(&page_size) {
        return Err("pageSize must be between 1 and 100".into());
    }
    let offset = query
        .get("pageToken")
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid pageToken".to_string())?
        .unwrap_or(0);
    let history_length = query
        .get("historyLength")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid historyLength".to_string())?;
    let include_artifacts = query
        .get("includeArtifacts")
        .map(|value| value.parse::<bool>())
        .transpose()
        .map_err(|_| "invalid includeArtifacts".to_string())?
        .unwrap_or(false);
    let timestamp_after = crate::time::parse_optional_rfc3339(
        query
            .get("statusTimestampAfter")
            .map(String::as_str)
            .filter(|value| !value.is_empty()),
    )?;
    let mut tasks = rt.tasks(owner).await;
    tasks.retain(|task| {
        context.is_none_or(|context| task.context_id == *context)
            && state.is_none_or(|state| task.status.state == state)
            && crate::time::is_after(task.status.timestamp.as_deref(), timestamp_after)
    });
    let total_size = tasks.len();
    let mut page = tasks
        .into_iter()
        .skip(offset)
        .take(page_size)
        .collect::<Vec<_>>();
    for task in &mut page {
        if let Some(length) = history_length
            && task.history.len() > length
        {
            task.history = task.history.split_off(task.history.len() - length);
        }
        if !include_artifacts {
            task.artifacts.clear();
        }
    }
    let next_offset = offset + page.len();
    Ok(json!({
        "tasks": page.iter().map(v1_task_value).collect::<Vec<_>>(),
        "nextPageToken": if next_offset < total_size { next_offset.to_string() } else { String::new() },
        "pageSize": page_size,
        "totalSize": total_size,
    }))
}

fn canonical_method(version: ProtocolVersion, method: &str) -> Option<&str> {
    let canonical = match (version, method) {
        (
            ProtocolVersion::V03,
            method @ ("message/send"
            | "message/stream"
            | "tasks/get"
            | "tasks/cancel"
            | "tasks/resubscribe"
            | "tasks/pushNotificationConfig/set"
            | "tasks/pushNotificationConfig/get"
            | "tasks/pushNotificationConfig/list"
            | "tasks/pushNotificationConfig/delete"
            | "agent/getAuthenticatedExtendedCard"),
        ) => method,
        (ProtocolVersion::V1, "SendMessage") => "message/send",
        (ProtocolVersion::V1, "SendStreamingMessage") => "message/stream",
        (ProtocolVersion::V1, "GetTask") => "tasks/get",
        (ProtocolVersion::V1, "CancelTask") => "tasks/cancel",
        (ProtocolVersion::V1, "SubscribeToTask") => "tasks/resubscribe",
        (ProtocolVersion::V1, "CreateTaskPushNotificationConfig") => {
            "tasks/pushNotificationConfig/set"
        }
        (ProtocolVersion::V1, "GetTaskPushNotificationConfig") => {
            "tasks/pushNotificationConfig/get"
        }
        (ProtocolVersion::V1, "ListTaskPushNotificationConfigs") => {
            "tasks/pushNotificationConfig/list"
        }
        (ProtocolVersion::V1, "DeleteTaskPushNotificationConfig") => {
            "tasks/pushNotificationConfig/delete"
        }
        (ProtocolVersion::V1, "GetExtendedAgentCard") => "agent/getAuthenticatedExtendedCard",
        (ProtocolVersion::V1, "ListTasks") => "tasks/list",
        _ => return None,
    };
    Some(canonical)
}

/// A JSON-RPC success: `{ jsonrpc, id, result }` on a 200.
fn rpc_ok(id: Value, result: impl serde::Serialize) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

/// Map a driver error to a JSON-RPC (code, message).
fn rpc_fault(err: RunApplicationError) -> (i32, String) {
    use awaken_session_contract::RunErrorKind;
    match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) if message.starts_with("unsupported output modes") => {
            (-32005, message)
        }
        (RunErrorKind::BadRequest, message) => (-32602, message),
        (RunErrorKind::Internal | RunErrorKind::Unavailable, message) => (-32603, message),
    }
}

fn cancel_driver_error(error: RunApplicationError) -> (StatusCode, i32, String) {
    use awaken_session_contract::RunErrorKind;
    let status = match error.kind {
        RunErrorKind::BadRequest => StatusCode::BAD_REQUEST,
        RunErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        RunErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, -32603, error.message)
}

/// Map a driver error to `(status, A2A error envelope)`.
fn error_response(err: RunApplicationError) -> Response {
    use awaken_session_contract::RunErrorKind;
    let (status, code, message) = match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) => (StatusCode::BAD_REQUEST, -32600, message),
        (RunErrorKind::Internal, message) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, message),
        (RunErrorKind::Unavailable, message) => (StatusCode::SERVICE_UNAVAILABLE, -32603, message),
    };
    (status, Json(ErrorResponse::new(code, message))).into_response()
}

fn rest_driver_error(err: RunApplicationError) -> Response {
    use awaken_session_contract::RunErrorKind;
    let (status, code, message) = match (err.kind, err.message) {
        (RunErrorKind::BadRequest, message) if message.starts_with("task not found") => {
            (StatusCode::NOT_FOUND, -32001, message)
        }
        (RunErrorKind::BadRequest, message) if message.starts_with("unsupported output modes") => {
            (StatusCode::BAD_REQUEST, -32005, message)
        }
        (RunErrorKind::BadRequest, message) => (StatusCode::BAD_REQUEST, -32602, message),
        (RunErrorKind::Internal, message) => (StatusCode::INTERNAL_SERVER_ERROR, -32603, message),
        (RunErrorKind::Unavailable, message) => (StatusCode::SERVICE_UNAVAILABLE, -32603, message),
    };
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

fn stream_binding_error(
    error: RunApplicationError,
    rpc_id: Option<&Value>,
    rest_errors: bool,
    version: ProtocolVersion,
) -> Response {
    if let Some(id) = rpc_id {
        let (code, message) = rpc_fault(error);
        rpc_error(id.clone(), code, message)
    } else if rest_errors {
        rest_driver_error_version(error, version)
    } else {
        error_response(error)
    }
}

fn rest_driver_error_version(error: RunApplicationError, version: ProtocolVersion) -> Response {
    if version == ProtocolVersion::V03 {
        return rest_driver_error(error);
    }
    let (code, message) = rpc_fault(error);
    let status = match code {
        -32001 => StatusCode::NOT_FOUND,
        -32603 => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    v1_fault(status, code, message)
}

fn sse_event(
    response: &StreamResponse,
    rpc_id: Option<&Value>,
    version: ProtocolVersion,
) -> String {
    let event = match version {
        ProtocolVersion::V03 => response.event_value(),
        ProtocolVersion::V1 => v1_stream_value(response),
    };
    let payload = match (rpc_id, version) {
        // JSON-RPC's result is the raw discriminated union member.
        (Some(id), _) => json!({ "jsonrpc": "2.0", "id": id, "result": event }),
        // HTTP+JSON uses the protobuf oneof JSON projection wrapper.
        (None, ProtocolVersion::V03) => response.oneof_value(),
        (None, ProtocolVersion::V1) => event,
    };
    format!("data: {payload}\n\n")
}

fn stream_response(rx: mpsc::UnboundedReceiver<String>) -> Response {
    let stream = UnboundedReceiverStream::new(rx).map(Ok::<String, Infallible>);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("valid A2A SSE response")
}

pub(crate) fn a2a_fault(status: StatusCode, code: i32, message: impl Into<String>) -> Response {
    let message = message.into();
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

pub(crate) fn v1_json_response(status: StatusCode, value: Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/a2a+json"),
    );
    response
}

pub(crate) fn v1_fault(status: StatusCode, code: i32, message: impl Into<String>) -> Response {
    let reason = match code {
        -32001 => "TASK_NOT_FOUND",
        -32002 => "TASK_NOT_CANCELABLE",
        -32003 => "PUSH_NOTIFICATION_NOT_SUPPORTED",
        -32004 => "UNSUPPORTED_OPERATION",
        -32005 => "CONTENT_TYPE_NOT_SUPPORTED",
        -32006 => "INVALID_AGENT_RESPONSE",
        -32007 => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
        -32008 => "EXTENSION_SUPPORT_REQUIRED",
        -32009 => "VERSION_NOT_SUPPORTED",
        -32603 => "INTERNAL_ERROR",
        _ => "INVALID_PARAMS",
    };
    let grpc_status = match code {
        -32001 => "NOT_FOUND",
        -32603 => "INTERNAL",
        -32009..=-32002 => "FAILED_PRECONDITION",
        _ => "INVALID_ARGUMENT",
    };
    v1_json_response(
        status,
        json!({ "error": {
            "code": status.as_u16(),
            "status": grpc_status,
            "message": message.into(),
            "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": reason,
                "domain": "a2a-protocol.org"
            }]
        }}),
    )
}

fn version_fault(
    version: ProtocolVersion,
    status: StatusCode,
    code: i32,
    message: impl Into<String>,
) -> Response {
    let message = message.into();
    match version {
        ProtocolVersion::V03 => a2a_fault(status, code, message),
        ProtocolVersion::V1 => v1_fault(status, code, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_maps_faults_to_status() {
        // A caller fault is a 400; a runtime fault is a 500 — both carry the A2A
        // JSON error envelope (its shape is covered by the `types` tests).
        assert_eq!(
            error_response(RunApplicationError::bad_request("bad")).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            error_response(RunApplicationError::internal("boom")).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
