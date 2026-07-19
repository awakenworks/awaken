//! A2A 1.0 ProtoJSON projections kept separate from transport dispatch.

use serde_json::{Value, json};

use crate::router::JSONRPC_PATH;
use crate::types::{
    Artifact, AuthenticationInfo, Part, PushNotificationConfig, StreamResponse, Task,
    TaskPushNotificationConfig, TaskState, TaskStatus,
};

pub(crate) fn parse_push_config(
    params: &Value,
) -> Result<(TaskPushNotificationConfig, Option<String>), String> {
    let task_id = params
        .get("taskId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing taskId".to_string())?
        .to_string();
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing url".to_string())?
        .to_string();
    let authentication = params
        .get("authentication")
        .map(|auth| {
            let scheme = auth
                .get("scheme")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "missing authentication.scheme".to_string())?;
            Ok::<_, String>(AuthenticationInfo {
                schemes: vec![scheme.to_string()],
                credentials: auth
                    .get("credentials")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            })
        })
        .transpose()?;
    let owner = params
        .get("tenant")
        .and_then(Value::as_str)
        .filter(|tenant| !tenant.is_empty())
        .map(ToOwned::to_owned);
    Ok((
        TaskPushNotificationConfig {
            task_id,
            push_notification_config: PushNotificationConfig {
                id: params
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned),
                url,
                token: params
                    .get("token")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned),
                authentication,
            },
        },
        owner,
    ))
}

pub(crate) fn push_value(
    task_id: &str,
    tenant: Option<&str>,
    config: &PushNotificationConfig,
) -> Value {
    let mut value = json!({ "taskId": task_id, "url": config.url });
    if let Some(tenant) = tenant.filter(|tenant| !tenant.is_empty()) {
        value["tenant"] = Value::String(tenant.to_string());
    }
    if let Some(id) = &config.id {
        value["id"] = Value::String(id.clone());
    }
    if let Some(authentication) = &config.authentication
        && let Some(scheme) = authentication.schemes.first()
    {
        value["authentication"] = json!({ "scheme": scheme });
    }
    value
}

pub(crate) fn stream_value(response: &StreamResponse) -> Value {
    if let Some(task) = &response.task {
        json!({ "task": task_value(task) })
    } else if let Some(message) = &response.message {
        json!({ "message": message_value(message) })
    } else if let Some(update) = &response.status_update {
        let mut event = json!({
            "taskId": update.task_id,
            "contextId": update.context_id,
            "status": status_value(&update.status),
        });
        if let Some(metadata) = &update.metadata {
            event["metadata"] = metadata.clone();
        }
        json!({ "statusUpdate": event })
    } else if let Some(update) = &response.artifact_update {
        let mut event = json!({
            "taskId": update.task_id,
            "contextId": update.context_id,
            "artifact": artifact_value(&update.artifact),
        });
        if update.append == Some(true) {
            event["append"] = Value::Bool(true);
        }
        if update.last_chunk == Some(true) {
            event["lastChunk"] = Value::Bool(true);
        }
        if let Some(metadata) = &update.metadata {
            event["metadata"] = metadata.clone();
        }
        json!({ "artifactUpdate": event })
    } else {
        Value::Null
    }
}

pub(crate) fn task_value(task: &Task) -> Value {
    let mut value = json!({
        "id": task.id,
        "contextId": task.context_id,
        "status": status_value(&task.status),
    });
    if !task.artifacts.is_empty() {
        value["artifacts"] = Value::Array(task.artifacts.iter().map(artifact_value).collect());
    }
    if !task.history.is_empty() {
        value["history"] = Value::Array(task.history.iter().map(message_value).collect());
    }
    value
}

fn status_value(status: &TaskStatus) -> Value {
    let state = match status.state {
        TaskState::Submitted => "TASK_STATE_SUBMITTED",
        TaskState::Working => "TASK_STATE_WORKING",
        TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
        TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        TaskState::Completed => "TASK_STATE_COMPLETED",
        TaskState::Failed => "TASK_STATE_FAILED",
        TaskState::Canceled => "TASK_STATE_CANCELED",
        TaskState::Rejected => "TASK_STATE_REJECTED",
        TaskState::Unknown => "TASK_STATE_UNSPECIFIED",
    };
    let mut value = json!({ "state": state });
    if let Some(message) = &status.message {
        value["message"] = message_value(message);
    }
    if let Some(timestamp) = &status.timestamp {
        value["timestamp"] = Value::String(timestamp.clone());
    }
    value
}

fn message_value(message: &crate::types::Message) -> Value {
    let mut value = json!({
        "messageId": message.message_id,
        "role": match message.role {
            crate::types::MessageRole::User => "ROLE_USER",
            crate::types::MessageRole::Agent => "ROLE_AGENT",
        },
        "parts": message.parts.iter().map(part_value).collect::<Vec<_>>(),
    });
    if let Some(context_id) = &message.context_id {
        value["contextId"] = Value::String(context_id.clone());
    }
    if let Some(task_id) = &message.task_id {
        value["taskId"] = Value::String(task_id.clone());
    }
    value
}

fn part_value(part: &Part) -> Value {
    let mut value = if part.kind.as_deref() == Some("data") {
        part.text
            .as_deref()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .map(|data| json!({ "data": data, "mediaType": "application/json" }))
            .unwrap_or(Value::Null)
    } else if let Some(text) = &part.text {
        json!({ "text": text, "mediaType": "text/plain" })
    } else if let Some(file) = &part.file {
        let mut file_value = if let Some(bytes) = &file.bytes {
            json!({ "raw": bytes })
        } else {
            json!({ "url": file.uri })
        };
        if let Some(media_type) = &file.mime_type {
            file_value["mediaType"] = Value::String(media_type.clone());
        }
        if let Some(name) = &file.name {
            file_value["filename"] = Value::String(name.clone());
        }
        file_value
    } else {
        Value::Null
    };
    if let Some(metadata) = &part.metadata {
        value["metadata"] = metadata.clone();
    }
    value
}

fn artifact_value(artifact: &Artifact) -> Value {
    let mut value = json!({
        "artifactId": artifact.artifact_id,
        "parts": artifact.parts.iter().map(part_value).collect::<Vec<_>>(),
    });
    if let Some(name) = &artifact.name {
        value["name"] = Value::String(name.clone());
    }
    value
}

pub(crate) fn agent_card_value(model: &str, origin: &str) -> Value {
    json!({
        "name": "assistant",
        "description": format!("Awaken agent over model `{model}`"),
        "supportedInterfaces": [
            { "url": format!("{origin}{JSONRPC_PATH}"), "protocolBinding": "JSONRPC", "tenant": "", "protocolVersion": "1.0" },
            { "url": origin, "protocolBinding": "HTTP+JSON", "tenant": "", "protocolVersion": "1.0" },
            { "url": format!("{origin}{JSONRPC_PATH}"), "protocolBinding": "JSONRPC", "tenant": "", "protocolVersion": "0.3" },
            { "url": origin, "protocolBinding": "HTTP+JSON", "tenant": "", "protocolVersion": "0.3" }
        ],
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "streaming": true, "pushNotifications": true, "extensions": [],
            "extendedAgentCard": true,
        },
        "securitySchemes": {},
        "securityRequirements": [],
        "defaultInputModes": ["text/plain", "image/*", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": [{
            "id": "assistant", "name": "Assistant",
            "description": "General-purpose agent execution", "tags": ["assistant"],
            "examples": [], "inputModes": [], "outputModes": [],
            "securityRequirements": [],
        }],
        "signatures": [],
    })
}
