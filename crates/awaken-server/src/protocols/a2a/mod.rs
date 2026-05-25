//! A2A v1.0 HTTP+JSON endpoints.

mod agent_card;
mod common;
mod conversion;
mod error;
mod message;
mod push_config;
mod task;
mod types;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use awaken_protocol_a2a::PushNotificationConfig as A2aPushNotificationConfig;
pub use awaken_protocol_a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentProvider, AgentSkill, Artifact,
    AuthenticationInfo, ListPushNotificationConfigsResponse, ListTasksResponse,
    Message as A2aMessage, MessageRole, Part, PushNotificationConfig, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent,
};

use crate::app::ProtocolRoutesState;

use common::{
    decode_json_body, decode_query, ensure_supported_version_from_request, parse_a2a_tail,
    parse_task_action_segment,
};
use error::A2aError;
use types::{DISCOVERY_PATH, GetTaskQuery, ListPushConfigsQuery, ListTasksQuery};

/// Build A2A routes.
pub fn a2a_routes() -> Router<ProtocolRoutesState> {
    Router::new()
        .route(DISCOVERY_PATH, get(a2a_agent_card_route))
        .route(
            "/v1/a2a/*tail",
            get(a2a_get_dispatch)
                .post(a2a_post_dispatch)
                .delete(a2a_delete_dispatch),
        )
}

async fn a2a_agent_card_route(
    State(st): State<ProtocolRoutesState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<AgentCard>, A2aError> {
    agent_card::a2a_agent_card(st, headers, uri).await
}

async fn a2a_get_dispatch(
    State(st): State<ProtocolRoutesState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, A2aError> {
    ensure_supported_version_from_request(&headers, &uri)?;
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["tasks"] => {
            let query = decode_query::<ListTasksQuery>(&uri)?;
            Ok(
                task::a2a_list_tasks_default(State(st), headers, Query(query))
                    .await?
                    .into_response(),
            )
        }
        ["tasks", task_id] => {
            let query = decode_query::<GetTaskQuery>(&uri)?;
            Ok(task::a2a_get_task_default(
                State(st),
                Path((*task_id).to_string()),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        ["tasks", task_id, "pushNotificationConfigs", config_id] => {
            Ok(push_config::a2a_get_push_config_default(
                State(st),
                Path(((*task_id).to_string(), (*config_id).to_string())),
                headers,
            )
            .await?)
        }
        ["tasks", task_id, "pushNotificationConfigs"] => {
            let query = decode_query::<ListPushConfigsQuery>(&uri)?;
            Ok(push_config::a2a_list_push_configs_default(
                State(st),
                Path((*task_id).to_string()),
                headers,
                Query(query),
            )
            .await?
            .into_response())
        }
        ["extendedAgentCard"] => {
            Ok(agent_card::a2a_extended_agent_card_default(st, headers).await?)
        }
        [tenant, "tasks"] => {
            let query = decode_query::<ListTasksQuery>(&uri)?;
            Ok(task::a2a_list_tasks_tenant(
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
            Ok(task::a2a_get_task_tenant(
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
            Ok(push_config::a2a_list_push_configs_tenant(
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
        ] => Ok(push_config::a2a_get_push_config_tenant(
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
                agent_card::a2a_extended_agent_card_tenant(st, (*tenant).to_string(), headers)
                    .await?,
            )
        }
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A path: /v1/a2a/{tail}"
        ))),
    }
}

async fn a2a_post_dispatch(
    State(st): State<ProtocolRoutesState>,
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
            Ok(
                message::a2a_message_send_default(State(st), headers, Json(payload))
                    .await?
                    .into_response(),
            )
        }
        ["message:stream"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(
                message::a2a_message_stream_default(State(st), headers, uri, Json(payload))
                    .await?
                    .into_response(),
            )
        }
        ["tasks", task_action] => {
            let (task_id, action) = parse_task_action_segment(task_action)?;
            match action {
                "cancel" => Ok(task::cancel_task(st, headers, None, task_id)
                    .await?
                    .into_response()),
                "subscribe" => message::subscribe_task(st, headers, None, task_id).await,
                _ => unreachable!("task action parser only returns supported actions"),
            }
        }
        ["tasks", task_id, "pushNotificationConfigs"] => {
            let payload = decode_json_body::<A2aPushNotificationConfig>(&headers, &body)?;
            Ok(push_config::a2a_create_push_config_default(
                State(st),
                Path((*task_id).to_string()),
                headers,
                Json(payload),
            )
            .await?)
        }
        [tenant, "message:send"] => {
            let payload = decode_json_body::<SendMessageRequest>(&headers, &body)?;
            Ok(message::a2a_message_send_tenant(
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
            Ok(message::a2a_message_stream_tenant(
                State(st),
                Path((*tenant).to_string()),
                headers,
                uri,
                Json(payload),
            )
            .await?
            .into_response())
        }
        [tenant, "tasks", task_action] => {
            let (task_id, action) = parse_task_action_segment(task_action)?;
            match action {
                "cancel" => {
                    Ok(
                        task::cancel_task(st, headers, Some((*tenant).to_string()), task_id)
                            .await?
                            .into_response(),
                    )
                }
                "subscribe" => {
                    message::subscribe_task(st, headers, Some((*tenant).to_string()), task_id).await
                }
                _ => unreachable!("task action parser only returns supported actions"),
            }
        }
        [tenant, "tasks", task_id, "pushNotificationConfigs"] => {
            let payload = decode_json_body::<A2aPushNotificationConfig>(&headers, &body)?;
            Ok(push_config::a2a_create_push_config_tenant(
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
    State(st): State<ProtocolRoutesState>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, A2aError> {
    ensure_supported_version_from_request(&headers, &uri)?;
    let segments = parse_a2a_tail(&tail);

    match segments.as_slice() {
        ["tasks", task_id, "pushNotificationConfigs", config_id] => {
            Ok(push_config::a2a_delete_push_config_default(
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
        ] => Ok(push_config::a2a_delete_push_config_tenant(
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

#[cfg(test)]
mod tests;
