use awaken_protocol_a2a::{MessageRole, SendMessageRequest, SendMessageResponse, StreamResponse};
use awaken_runtime::RunActivation;
use awaken_server_contract::contract::message::Message as AwakenMessage;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use bytes::Bytes;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::app::ProtocolRoutesState;
use crate::http_sse::{format_sse_data, sse_body_stream, sse_response};

use super::common::{
    ensure_runnable_agent, ensure_supported_version, public_agent_id, trim_to_option,
};
use super::conversion::a2a_part_to_content_block;
use super::error::{A2aError, FieldViolation};
use super::push_config::{
    load_push_notification_configs, normalize_push_config, spawn_push_notification_driver,
    upsert_push_notification_config_for_thread,
};
use super::stream_projector::{InitialStreamEvent, TaskStreamProjector};
use super::task::{
    load_task_snapshot, record_task_binding, resolve_task, run_is_a2a_resumable, submitted_task,
    task_context_id, wait_for_task,
};
use super::types::{BLOCKING_POLL_INTERVAL, PreparedRequest, SUPPORTED_OUTPUT_MODE, TaskSnapshot};

pub(super) async fn a2a_message_send_default(
    State(st): State<ProtocolRoutesState>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, A2aError> {
    send_message(st, headers, None, payload).await
}

pub(super) async fn a2a_message_send_tenant(
    State(st): State<ProtocolRoutesState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, A2aError> {
    send_message(st, headers, Some(tenant), payload).await
}

pub(super) async fn a2a_message_stream_default(
    State(st): State<ProtocolRoutesState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Response, A2aError> {
    stream_message(st, headers, Some(&uri), None, payload).await
}

pub(super) async fn a2a_message_stream_tenant(
    State(st): State<ProtocolRoutesState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Response, A2aError> {
    stream_message(st, headers, Some(&uri), Some(tenant), payload).await
}

pub(super) async fn subscribe_task(
    st: ProtocolRoutesState,
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

async fn send_message(
    st: ProtocolRoutesState,
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

    st.run
        .mailbox()
        .submit_background(st.run.scope_activation(request))
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
    st: ProtocolRoutesState,
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

    st.run
        .mailbox()
        .submit_background(st.run.scope_activation(request))
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

fn stream_task_response(
    st: ProtocolRoutesState,
    task_id: String,
    tenant: Option<String>,
    history_length: usize,
) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(st.sse_buffer_size);

    tokio::spawn(async move {
        let mut projector = TaskStreamProjector::new(InitialStreamEvent::TaskSnapshot);

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

            for response in projector.project(&snapshot) {
                if send_stream_response(&tx, response).await.is_err() {
                    return;
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

async fn prepare_send_request(
    st: &ProtocolRoutesState,
    path_tenant: Option<String>,
    payload: SendMessageRequest,
) -> Result<PreparedRequest, A2aError> {
    let mut violations = Vec::new();
    let request_tenant = trim_to_option(payload.agent_id.as_deref());
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
    let mut request = RunActivation::new(thread_id.clone(), vec![awaken_message])
        .with_origin(awaken_server_contract::contract::storage::RunRequestOrigin::A2A)
        .with_adapter(awaken_server_contract::contract::tool_intercept::AdapterKind::A2a);
    let mut new_task_start_message_id = None;

    if let Some(ref tenant) = effective_tenant {
        request = request.with_agent_id(tenant.clone());
    } else if let Some(agent_id) = latest_context_agent_id(st, &thread_id).await? {
        request = request.with_agent_id(agent_id);
    } else if thread_has_prior_context(st, &thread_id).await? {
        return Err(A2aError::invalid(
            "agent",
            "thread has prior context but no identifiable agent binding; specify the agent via tenant path or body agent_id",
        ));
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
            request = request
                .with_run_id_hint(task_id.clone())
                .with_dispatch_id_hint(task_id.clone());
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

async fn latest_context_agent_id(
    st: &ProtocolRoutesState,
    thread_id: &str,
) -> Result<Option<String>, A2aError> {
    Ok(st
        .run
        .store()
        .latest_run(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .and_then(|run| {
            let agent_id = run.agent_id.trim();
            (!agent_id.is_empty()).then(|| agent_id.to_string())
        }))
}

async fn thread_has_prior_context(
    st: &ProtocolRoutesState,
    thread_id: &str,
) -> Result<bool, A2aError> {
    use awaken_server_contract::contract::mailbox::RunDispatchStatus;

    let store = st.run.store();
    let Some(thread) = store
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    else {
        return Ok(false);
    };

    if thread.active_run_id.is_some() || thread.open_run_id.is_some() {
        return Ok(true);
    }
    if thread
        .metadata
        .custom
        .get(super::types::TASK_BINDINGS_METADATA_KEY)
        .and_then(|value| {
            serde_json::from_value::<super::types::StoredTaskBindings>(value.clone()).ok()
        })
        .is_some_and(|bindings| !bindings.tasks.is_empty())
    {
        return Ok(true);
    }

    let messages = store
        .load_messages(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    if messages.is_some_and(|messages| !messages.is_empty()) {
        return Ok(true);
    }

    let pending = st
        .run
        .mailbox()
        .list_dispatches(
            &st.run.scoped_id(thread_id),
            Some(&[RunDispatchStatus::Queued, RunDispatchStatus::Claimed]),
            1,
            0,
        )
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    Ok(!pending.is_empty())
}
