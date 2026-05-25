use awaken_contract::thread::Thread;
use awaken_protocol_a2a::{ListPushNotificationConfigsResponse, PushNotificationConfig};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::app::ProtocolRoutesState;

use super::common::{
    ensure_supported_version, load_thread_metadata_projection, parse_page_token,
    persist_thread_metadata, trim_to_option,
};
use super::error::A2aError;
use super::push_outbox::enqueue_push_notification;
use super::stream_projector::{InitialStreamEvent, TaskStreamProjector};
use super::task::{
    ensure_task_visible, load_task_snapshot, resolve_task, submitted_task, task_context_id,
};
use super::types::{
    BLOCKING_POLL_INTERVAL, DEFAULT_PAGE_SIZE, ListPushConfigsQuery, MAX_PAGE_SIZE,
    PUSH_CONFIGS_METADATA_KEY, StoredPushConfigs, TaskSnapshot,
};

pub(super) async fn a2a_create_push_config_default(
    State(st): State<ProtocolRoutesState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<PushNotificationConfig>,
) -> Result<Response, A2aError> {
    create_push_config(st, headers, None, task_id, payload)
        .await
        .map(IntoResponse::into_response)
}

pub(super) async fn a2a_create_push_config_tenant(
    State(st): State<ProtocolRoutesState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<PushNotificationConfig>,
) -> Result<Response, A2aError> {
    create_push_config(st, headers, Some(tenant), task_id, payload)
        .await
        .map(IntoResponse::into_response)
}

pub(super) async fn a2a_list_push_configs_default(
    State(st): State<ProtocolRoutesState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListPushConfigsQuery>,
) -> Result<Json<ListPushNotificationConfigsResponse>, A2aError> {
    list_push_configs(st, headers, None, task_id, query).await
}

pub(super) async fn a2a_list_push_configs_tenant(
    State(st): State<ProtocolRoutesState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<ListPushConfigsQuery>,
) -> Result<Json<ListPushNotificationConfigsResponse>, A2aError> {
    list_push_configs(st, headers, Some(tenant), task_id, query).await
}

pub(super) async fn a2a_get_push_config_default(
    State(st): State<ProtocolRoutesState>,
    Path((task_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    get_push_config(st, headers, None, task_id, config_id)
        .await
        .map(IntoResponse::into_response)
}

pub(super) async fn a2a_get_push_config_tenant(
    State(st): State<ProtocolRoutesState>,
    Path((tenant, task_id, config_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    get_push_config(st, headers, Some(tenant), task_id, config_id)
        .await
        .map(IntoResponse::into_response)
}

pub(super) async fn a2a_delete_push_config_default(
    State(st): State<ProtocolRoutesState>,
    Path((task_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    delete_push_config(st, headers, None, task_id, config_id).await
}

pub(super) async fn a2a_delete_push_config_tenant(
    State(st): State<ProtocolRoutesState>,
    Path((tenant, task_id, config_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, A2aError> {
    delete_push_config(st, headers, Some(tenant), task_id, config_id).await
}

async fn create_push_config(
    st: ProtocolRoutesState,
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
    st: ProtocolRoutesState,
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
    st: ProtocolRoutesState,
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
    st: ProtocolRoutesState,
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

pub(super) fn normalize_push_config(
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
    if let Some(existing_tenant) = trim_to_option(config.agent_id.as_deref())
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
    config.agent_id = tenant.map(ToOwned::to_owned);
    Ok(config)
}

pub(super) async fn load_push_notification_configs(
    st: &ProtocolRoutesState,
    task_id: &str,
    tenant: Option<&str>,
) -> Result<Vec<PushNotificationConfig>, A2aError> {
    let Some(task) = resolve_task(st, task_id).await? else {
        return Ok(Vec::new());
    };
    let Some(thread) = st
        .run
        .store()
        .load_thread(&task.thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    else {
        return Ok(Vec::new());
    };

    let mut configs = load_thread_push_notification_configs(&thread, task_id)?;
    if let Some(tenant) = tenant {
        configs.retain(|config| config.agent_id.as_deref() == Some(tenant));
    }
    configs.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(configs)
}

async fn find_push_notification_config(
    st: &ProtocolRoutesState,
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
    st: &ProtocolRoutesState,
    task_id: &str,
    configs: Vec<PushNotificationConfig>,
) -> Result<(), A2aError> {
    let Some(task) = resolve_task(st, task_id).await? else {
        return Err(A2aError::task_not_found(task_id.to_string()));
    };
    let thread_id = task.thread_id;
    let (exists, thread) = load_thread_metadata_projection(st, &thread_id).await?;
    save_thread_push_notification_configs(st, &thread_id, exists, thread, task_id, configs).await
}

async fn upsert_push_notification_config(
    st: &ProtocolRoutesState,
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

pub(super) async fn upsert_push_notification_config_for_thread(
    st: &ProtocolRoutesState,
    thread_id: &str,
    task_id: &str,
    tenant: Option<&str>,
    config: PushNotificationConfig,
) -> Result<(), A2aError> {
    let (exists, thread) = load_thread_metadata_projection(st, thread_id).await?;
    let mut configs = load_thread_push_notification_configs(&thread, task_id)?;
    if let Some(tenant) = tenant {
        configs.retain(|existing| existing.agent_id.as_deref() == Some(tenant));
    }
    if let Some(position) = configs.iter().position(|existing| existing.id == config.id) {
        configs[position] = config;
    } else {
        configs.push(config);
    }
    save_thread_push_notification_configs(st, thread_id, exists, thread, task_id, configs).await
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
    st: &ProtocolRoutesState,
    thread_id: &str,
    exists: bool,
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
    persist_thread_metadata(st, thread_id, exists, thread).await?;

    Ok(())
}

pub(super) fn spawn_push_notification_driver(
    st: ProtocolRoutesState,
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
    st: ProtocolRoutesState,
    task_id: String,
    tenant: Option<String>,
    config: PushNotificationConfig,
) -> Result<(), A2aError> {
    let outbox = crate::protocol_replay_state::a2a_push_webhook_outbox_for_buffers(
        &st.protocol.replay_buffers,
    )
    .ok_or_else(|| {
        A2aError::Internal("A2A push notification outbox relay is not configured".to_string())
    })?;
    let config_id = config.id.clone().unwrap_or_default();
    let mut projector = TaskStreamProjector::new(InitialStreamEvent::StatusUpdate);

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

        for response in projector.project(&snapshot) {
            enqueue_push_notification(outbox.as_ref(), &config, &response)
                .await
                .map_err(|error| A2aError::Internal(error.to_string()))?;
            if let Err(error) =
                crate::protocol_replay_state::tick_a2a_push_webhook_outbox_for_buffers(
                    &st.protocol.replay_buffers,
                )
                .await
            {
                tracing::warn!(
                    error = %error,
                    "A2A push notification outbox relay tick failed"
                );
            }
        }

        if snapshot.task.status.state.is_terminal() || snapshot.task.status.state.is_interrupted() {
            break;
        }

        tokio::time::sleep(BLOCKING_POLL_INTERVAL).await;
    }

    Ok(())
}
