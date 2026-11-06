// @formal surface requirement=runtime.background_tool_execution.durable_session_scope
// @formal reason: Static owner: the configurable model-facing projection over accepted background task
// @formal reason: truth. Dynamic path: only a reconciled, accepted terminal task becomes one
// @formal reason: source-labelled message under the authored presentation rule, bounded in fields and
// @formal reason: body, with a revision receipt that makes redelivery idempotent; the rule cannot alter
// @formal reason: source identity, destination, execution permission or task status, and an oversized body
// @formal reason: is truncated with an explicit marker rather than silently cut. Evidence:
// @formal reason: runtime.background_tool_execution plus this module's notification rule tests and the
// @formal reason: coordinator attention chain; this is a product-requirement classification, not a direct
// @formal reason: proof claim.
//! Configurable model-facing projection over accepted task truth. No delivery
//! queue or lifecycle is owned here; normal Thread messages own deduplication.

use std::collections::BTreeMap;

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope, StateCell, Store};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{BackgroundTask, BackgroundTaskLifecycle};
use awaken_tool_contract::tool::background_task::BackgroundTaskEnd;

pub(crate) const RECEIPT_PREFIX: &str = "background_task_notification/";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskNotificationEvent {
    Completed,
    Failed,
    Cancelled,
    Indeterminate,
}

impl TaskNotificationEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Indeterminate => "indeterminate",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskNotificationFormat {
    #[default]
    Json,
    Tagged,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TaskNotificationRule {
    /// Whether this result is included in Agent context. Recovery stays active.
    pub enabled: bool,
    pub format: TaskNotificationFormat,
    /// XML-like presentation tag; never changes the message's User role.
    pub tag: String,
    /// Only {task_id}, {tool_id} and {event} placeholders are supported.
    pub heading: String,
    /// Output label -> JSON pointer in the public get_background_task view.
    /// Empty selects that complete public view, never invocation/lease fields.
    pub fields: BTreeMap<String, String>,
    /// Exact predicates over the public task view, keyed by JSON pointer.
    /// All must match. This is data selection, not a script or permission rule.
    pub when: BTreeMap<String, Value>,
    pub max_body_chars: usize,
}

impl Default for TaskNotificationRule {
    fn default() -> Self {
        Self {
            enabled: true,
            format: TaskNotificationFormat::Json,
            tag: "background-task-notification".into(),
            heading: "Background task {task_id}: {event}".into(),
            fields: BTreeMap::new(),
            when: BTreeMap::new(),
            max_body_chars: 16_384,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TaskNotifications {
    pub default: TaskNotificationRule,
    pub events: BTreeMap<TaskNotificationEvent, TaskNotificationRule>,
    /// Exact canonical tool id -> event -> complete replacement rule.
    pub tools: BTreeMap<String, BTreeMap<TaskNotificationEvent, TaskNotificationRule>>,
}

impl TaskNotifications {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.tools.len() > 128 || self.tools.keys().any(|key| key.trim().is_empty()) {
            return Err(
                "notification tool overrides require at most 128 non-empty tool ids".into(),
            );
        }
        for rule in std::iter::once(&self.default)
            .chain(self.events.values())
            .chain(self.tools.values().flat_map(|rules| rules.values()))
        {
            if !(256..=65_536).contains(&rule.max_body_chars)
                || rule.heading.len() > 1024
                || rule.fields.len() > 32
                || rule.when.len() > 16
            {
                return Err("notification rule exceeds heading, field or body limits".into());
            }
            if rule.tag.is_empty()
                || rule.tag.len() > 64
                || !rule.tag.as_bytes()[0].is_ascii_alphabetic()
                || !rule
                    .tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err("notification tag must be a bounded identifier".into());
            }
            let heading = rule
                .heading
                .replace("{task_id}", "")
                .replace("{tool_id}", "")
                .replace("{event}", "");
            if heading.contains(['{', '}']) {
                return Err("notification heading supports only task_id, tool_id and event".into());
            }
            if rule.fields.iter().any(|(label, pointer)| {
                label.trim().is_empty()
                    || label.len() > 128
                    || pointer.len() > 512
                    || (!pointer.is_empty() && !pointer.starts_with('/'))
            }) {
                return Err("notification fields require bounded labels and JSON pointers".into());
            }
            if rule.when.iter().any(|(pointer, expected)| {
                pointer.len() > 512
                    || (!pointer.is_empty() && !pointer.starts_with('/'))
                    || expected.to_string().len() > 4096
            }) {
                return Err(
                    "notification predicates require bounded JSON pointers and values".into(),
                );
            }
        }
        Ok(())
    }

    pub(crate) fn message(
        &self,
        task: &BackgroundTask,
        state: &Store,
    ) -> Option<(Message, Command)> {
        let BackgroundTaskLifecycle::Ended { end } = &task.lifecycle else {
            return None;
        };
        let event = match end {
            BackgroundTaskEnd::Completed { .. } => TaskNotificationEvent::Completed,
            BackgroundTaskEnd::Failed { .. } => TaskNotificationEvent::Failed,
            BackgroundTaskEnd::Cancelled {} => TaskNotificationEvent::Cancelled,
            BackgroundTaskEnd::Indeterminate { .. } => TaskNotificationEvent::Indeterminate,
        };
        // Length framing prevents caller-independent task ids containing ':'
        // from aliasing another task/revision. Content is not part of identity.
        let id = Id(format!(
            "background-result:{}:{}:{}",
            task.id.as_str().len(),
            task.id.as_str(),
            task.revision
        ));
        let receipt = StateCell::<u64>::new(
            Scope::Thread,
            MergePolicy::Exclusive,
            format!("{RECEIPT_PREFIX}{}", task.id.as_str()),
        );
        // A tiny delivery receipt shares ThreadCommit with the message. It
        // survives transcript compaction and cannot be forged via message ids.
        if receipt
            .load(state)
            .ok()?
            .is_some_and(|revision| revision >= task.revision)
        {
            return None;
        }
        let tool = &task.invocation.call.tool_id;
        let rule = self
            .tools
            .get(tool)
            .and_then(|rules| rules.get(&event))
            .or_else(|| self.events.get(&event))
            .unwrap_or(&self.default);
        if !rule.enabled {
            return None;
        }
        let public = crate::tools::task_view_value(task);
        if rule
            .when
            .iter()
            .any(|(pointer, expected)| public.pointer(pointer) != Some(expected))
        {
            return None;
        }
        let body = if rule.fields.is_empty() {
            public
        } else {
            Value::Object(
                rule.fields
                    .iter()
                    .map(|(label, pointer)| {
                        (
                            label.clone(),
                            public.pointer(pointer).cloned().unwrap_or(Value::Null),
                        )
                    })
                    .collect(),
            )
        };
        let encoded = body.to_string();
        let truncated = encoded.chars().count() > rule.max_body_chars;
        let body = if truncated {
            json!({"preview": encoded.chars().take(rule.max_body_chars).collect::<String>(), "truncated": true})
        } else {
            body
        };
        let heading = rule
            .heading
            .replace("{task_id}", task.id.as_str())
            .replace("{tool_id}", tool)
            .replace("{event}", event.name());
        let envelope = json!({"source":"background_task", "task_id":task.id.as_str(),
            "event":event, "heading":heading, "result":body});
        let text = match rule.format {
            TaskNotificationFormat::Json => envelope.to_string(),
            TaskNotificationFormat::Tagged => format!(
                "<{}>\n{}\n</{}>",
                rule.tag,
                envelope
                    .to_string()
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;"),
                rule.tag
            ),
            TaskNotificationFormat::Text => format!("{heading}\n{body}"),
        };
        Some((
            Message::text(id, Role::User, text),
            receipt.write(&task.revision).ok()?,
        ))
    }
}
