//! Explicit application notifications over the existing MCP connection.
//! These are not MCP task status or progress, and never grant System authority.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transport-local application sink. The owner must arrange durable
/// admission/replay; receipt of an SSE frame alone is not durable delivery.
#[async_trait]
pub trait McpNotificationHandler: Send + Sync {
    async fn receive(&self, method: &str, params: Value);
}

#[derive(Clone)]
pub(crate) struct NotificationRegistration {
    pub methods: Vec<String>,
    pub handler: Arc<dyn McpNotificationHandler>,
}

/// Configuration under the exact dynamic plugin id `mcp:<server-name>`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpNotificationConfig {
    pub notifications: BTreeMap<String, McpInputRule>,
}

/// Select complete application messages, not arbitrary token/audio fragments.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpInputRule {
    /// Pointer to a provider event id unique across reconnects for this server.
    pub event_id: String,
    pub delivery: McpInputDelivery,
    /// All predicates must match, e.g. `/kind` = `transcript.final`.
    #[serde(default)]
    pub when: BTreeMap<String, Value>,
}

/// Input and exact-response cancellation are separate capabilities. Neither
/// can select a Session from provider data or grant System message authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpInputDelivery {
    Message {
        text: String,
    },
    CancelResponse {
        input_method: String,
        input_event_id: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum McpSelectedInput {
    Message {
        event_id: String,
        text: String,
    },
    CancelResponse {
        input_method: String,
        input_event_id: String,
    },
}

impl McpNotificationConfig {
    pub fn parse(value: Option<&Value>) -> Result<Self, String> {
        let config: Self = value
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        if config.notifications.len() > 32 {
            return Err("at most 32 MCP input notification methods are allowed".into());
        }
        for (method, rule) in &config.notifications {
            if !application_method(method) {
                return Err(
                    "MCP input requires an explicit application notification method".into(),
                );
            }
            let pointer = match &rule.delivery {
                McpInputDelivery::Message { text } => text,
                McpInputDelivery::CancelResponse {
                    input_method,
                    input_event_id,
                } => {
                    if !config
                        .notifications
                        .get(input_method)
                        .is_some_and(|r| matches!(r.delivery, McpInputDelivery::Message { .. }))
                    {
                        return Err(
                            "MCP response cancellation must target a configured input method"
                                .into(),
                        );
                    }
                    input_event_id
                }
            };
            if [&rule.event_id, pointer]
                .into_iter()
                .chain(rule.when.keys())
                .any(|p| !p.starts_with('/') || p.len() > 512)
                || rule.when.len() > 16
                || rule.when.values().any(|v| v.to_string().len() > 4096)
            {
                return Err("MCP input rule requires bounded JSON pointers and predicates".into());
            }
        }
        Ok(config)
    }

    /// Missing/malformed/oversized messages fail closed without retaining raw
    /// provider data. Partial hypotheses must be excluded by authored predicates.
    pub fn select(&self, method: &str, params: &Value) -> Option<McpSelectedInput> {
        let rule = self.notifications.get(method)?;
        if rule.when.iter().any(|(p, v)| params.pointer(p) != Some(v)) {
            return None;
        }
        let id = params.pointer(&rule.event_id)?.as_str()?;
        if id.trim().is_empty() || id.len() > 256 {
            return None;
        }
        match &rule.delivery {
            McpInputDelivery::Message { text } => {
                let text = params.pointer(text)?.as_str()?;
                (!text.trim().is_empty() && text.len() <= 65_536).then(|| {
                    McpSelectedInput::Message {
                        event_id: id.into(),
                        text: text.into(),
                    }
                })
            }
            McpInputDelivery::CancelResponse {
                input_method,
                input_event_id,
            } => {
                let target = params.pointer(input_event_id)?.as_str()?;
                (!target.trim().is_empty() && target.len() <= 256).then(|| {
                    McpSelectedInput::CancelResponse {
                        input_method: input_method.clone(),
                        input_event_id: target.into(),
                    }
                })
            }
        }
    }
}

pub(crate) fn application_method(method: &str) -> bool {
    let Some(suffix) = method.strip_prefix("notifications/") else {
        return false;
    };
    let Some((namespace, event)) = suffix.split_once('/') else {
        return false;
    };
    method.len() <= 128
        && !event.is_empty()
        && !namespace.is_empty()
        && ![
            "tasks",
            "tools",
            "resources",
            "prompts",
            "progress",
            "message",
            "cancelled",
            "initialized",
        ]
        .contains(&namespace)
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn final_input_selection_is_explicit_bounded_and_cannot_retarget() {
        // Decision table: configured method + all predicates + bounded id/text
        // -> selected data. Wrong method/partial/missing/oversized -> no input.
        // A provider recipient field never changes the connection-owned target.
        let config = McpNotificationConfig::parse(Some(&json!({"notifications":{
            "notifications/phone/input":{"event_id":"/id","delivery":{"kind":"message","text":"/text"},"when":{"/kind":"final"}}
        }})))
        .unwrap();
        let good = json!({"id":"e1","text":"预算五万元","kind":"final","session_id":"foreign"});
        assert_eq!(
            config.select("notifications/phone/input", &good),
            Some(McpSelectedInput::Message {
                event_id: "e1".into(),
                text: "预算五万元".into()
            })
        );
        for bad in [
            json!({"id":"e1","text":"预算","kind":"partial"}),
            json!({"text":"预算","kind":"final"}),
            json!({"id":"e1","text":"x".repeat(65537),"kind":"final"}),
        ] {
            assert!(config.select("notifications/phone/input", &bad).is_none());
        }
        assert!(config.select("notifications/other/input", &good).is_none());
        assert!(
            McpNotificationConfig::parse(Some(&json!({"notifications":{
                "notifications/tasks/status":{"event_id":"/id","delivery":{"kind":"message","text":"/statusMessage"}}
            }})))
            .is_err()
        );
    }

    #[test]
    fn cancellation_can_only_address_a_configured_input_identity() {
        // C1 explicit cancellation rule; C2 target method is a configured
        // message rule; C3 provider references a bounded event id. C1+C2+C3
        // -> exact input identity, never a payload-selected Run/Session.
        // !C2 rejects configuration; !C3 is inert. Unknown fields fail closed.
        let config = serde_json::json!({"notifications": {
            "notifications/phone/turn": {"event_id":"/id","delivery":{"kind":"message","text":"/text"}},
            "notifications/phone/interrupt": {"event_id":"/id","delivery":{"kind":"cancel_response","input_method":"notifications/phone/turn","input_event_id":"/target"}}
        }});
        let policy = McpNotificationConfig::parse(Some(&config)).unwrap();
        assert_eq!(
            policy.select(
                "notifications/phone/interrupt",
                &json!({"id":"i1","target":"u2","session":"other","run":"other"})
            ),
            Some(McpSelectedInput::CancelResponse {
                input_method: "notifications/phone/turn".into(),
                input_event_id: "u2".into()
            })
        );
        assert!(
            policy
                .select(
                    "notifications/phone/interrupt",
                    &json!({"id":"i1","target":""})
                )
                .is_none()
        );
        let mut invalid = config.clone();
        invalid["notifications"]
            .as_object_mut()
            .unwrap()
            .remove("notifications/phone/turn");
        assert!(McpNotificationConfig::parse(Some(&invalid)).is_err());
        let mut invalid = config;
        invalid["notifications"]["notifications/phone/turn"]["role"] = json!("system");
        assert!(McpNotificationConfig::parse(Some(&invalid)).is_err());
    }
}
