//! A2A 1.0 ProtoJSON projections kept separate from transport dispatch.

use serde_json::{Value, json};

/// Normalize the v1 ProtoJSON `Part` representation at the version ACL. The
/// authoritative v0.3 DTO remains a strict `kind`-tagged union; legacy fields
/// never enter that type directly.
pub(crate) fn normalize_send_params(mut value: Value) -> Result<Value, String> {
    let message = value
        .get_mut("message")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "message must be an object".to_string())?;
    message
        .entry("kind".to_string())
        .or_insert_with(|| json!("message"));
    if let Some(role) = message.get_mut("role") {
        match role.as_str() {
            Some("ROLE_USER") => *role = json!("user"),
            Some("ROLE_AGENT") => *role = json!("agent"),
            _ => {}
        }
    }
    let parts = message
        .get_mut("parts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "message.parts must be an array".to_string())?;
    for (index, part) in parts.iter_mut().enumerate() {
        let object = part
            .as_object_mut()
            .ok_or_else(|| format!("message.parts[{index}] must be an object"))?;
        if object.contains_key("kind") {
            continue;
        }
        let metadata = object.remove("metadata");
        let normalized = if let Some(text) = object.remove("text") {
            json!({"kind":"text", "text":text})
        } else if let Some(data) = object.remove("data") {
            json!({"kind":"data", "data":data})
        } else if let Some(bytes) = object.remove("raw") {
            let mut file = json!({"bytes":bytes});
            if let Some(mime) = object.remove("mediaType") {
                file["mimeType"] = mime;
            }
            if let Some(name) = object.remove("filename") {
                file["name"] = name;
            }
            json!({"kind":"file", "file":file})
        } else if let Some(uri) = object.remove("url") {
            let mut file = json!({"uri":uri});
            if let Some(mime) = object.remove("mediaType") {
                file["mimeType"] = mime;
            }
            if let Some(name) = object.remove("filename") {
                file["name"] = name;
            }
            json!({"kind":"file", "file":file})
        } else {
            return Err(format!("message.parts[{index}] has no supported payload"));
        };
        *part = normalized;
        if let Some(metadata) = metadata {
            part["metadata"] = metadata;
        }
    }
    Ok(value)
}

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
    match response {
        StreamResponse::Task(task) => json!({ "task": task_value(task) }),
        StreamResponse::Message(message) => json!({ "message": message_value(message) }),
        StreamResponse::StatusUpdate(update) => {
            let mut event = json!({
                "taskId": update.task_id,
                "contextId": update.context_id,
                "status": status_value(&update.status),
            });
            if let Some(metadata) = &update.metadata {
                event["metadata"] =
                    serde_json::to_value(metadata).expect("A2A metadata serializes");
            }
            json!({ "statusUpdate": event })
        }
        StreamResponse::ArtifactUpdate(update) => {
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
                event["metadata"] =
                    serde_json::to_value(metadata).expect("A2A metadata serializes");
            }
            json!({ "artifactUpdate": event })
        }
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
    let mut value = match part {
        Part::Data { data, .. } => json!({ "data": data, "mediaType": "application/json" }),
        Part::Text { text, .. } => json!({ "text": text, "mediaType": "text/plain" }),
        Part::File { file, .. } => {
            let (mut value, mime_type, name) = match file {
                crate::types::FilePart::Bytes(file) => {
                    (json!({ "raw": file.bytes }), &file.mime_type, &file.name)
                }
                crate::types::FilePart::Uri(file) => {
                    (json!({ "url": file.uri }), &file.mime_type, &file.name)
                }
            };
            if let Some(media_type) = mime_type {
                value["mediaType"] = Value::String(media_type.clone());
            }
            if let Some(name) = name {
                value["filename"] = Value::String(name.clone());
            }
            value
        }
    };
    if let Some(metadata) = part.metadata() {
        value["metadata"] = serde_json::to_value(metadata).expect("A2A metadata serializes");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_projection_preserves_data_payload_and_metadata_exactly() {
        // Causal graph: stored A2A Part -> v1 ProtoJSON projection -> remote client.
        //
        // Decision table:
        // | data present | metadata present | projection                         |
        // | yes          | yes              | exact data + metadata + mediaType  |
        // | no           | either           | null (invalid internal data part)  |
        let data = json!({"nested":[1, true, null], "literal":"{not encoded}"});
        let metadata = std::collections::BTreeMap::from([("trace".into(), json!("t-1"))]);
        let part = Part::Data {
            data: serde_json::from_value(data.clone()).unwrap(),
            metadata: Some(metadata.clone()),
        };

        assert_eq!(
            part_value(&part),
            json!({
                "data": data,
                "mediaType": "application/json",
                "metadata": metadata,
            })
        );
    }
}
