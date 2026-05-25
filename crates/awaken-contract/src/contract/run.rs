//! Run activation contract types.

use serde::{Deserialize, Serialize};

use super::identity::RunOrigin;
use super::inference::InferenceOverride;
use super::message::Message;
use super::storage::{MessageSeqRange, PinnedRegistryManifest, RunRequestOrigin};
use super::suspension::ToolCallResume;
use super::tool::ToolDescriptor;
use super::tool_intercept::{AdapterKind, RunMode};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub thread_id: String,
    #[serde(default)]
    pub kind: RunKind,
}

impl RunIntent {
    #[must_use]
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            agent_id: None,
            thread_id: thread_id.into(),
            kind: RunKind::NewIntent,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunKind {
    #[default]
    NewIntent,
    HitlResume {
        run_id: String,
    },
    ContinuationFromRun {
        run_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum RunInput {
    NewMessages(Vec<Message>),
    AlreadyPersisted(RunInputSnapshot),
}

impl Default for RunInput {
    fn default() -> Self {
        Self::NewMessages(Vec::new())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunInputSnapshot {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<MessageSeqRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<InferenceOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontend_tools: Vec<ToolDescriptor>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunTraceContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub origin: RunOrigin,
    #[serde(default)]
    pub adapter: AdapterKind,
    #[serde(default)]
    pub run_mode: RunMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl RunTraceContext {
    #[must_use]
    pub fn with_legacy_origin(mut self, origin: RunRequestOrigin) -> Self {
        self.origin = origin.into();
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "scope", content = "manifest")]
pub enum RunResolutionScope {
    #[default]
    Live,
    Pinned(PinnedRegistryManifest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunActivationSnapshot {
    pub intent: RunIntent,
    pub input: RunInputSnapshot,
    pub options: RunOptions,
    pub trace: RunTraceContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeded_decisions: Vec<(String, ToolCallResume)>,
    pub resolution_scope: PinnedRegistryManifest,
}

impl From<RunRequestOrigin> for RunOrigin {
    fn from(origin: RunRequestOrigin) -> Self {
        match origin {
            RunRequestOrigin::User => Self::User,
            RunRequestOrigin::Mcp => Self::Mcp,
            RunRequestOrigin::A2A => Self::Subagent,
            RunRequestOrigin::Internal => Self::Internal,
        }
    }
}

impl From<RunOrigin> for RunRequestOrigin {
    fn from(origin: RunOrigin) -> Self {
        match origin {
            RunOrigin::User => Self::User,
            RunOrigin::Mcp => Self::Mcp,
            RunOrigin::Subagent => Self::A2A,
            RunOrigin::Internal => Self::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::storage::PinnedRegistryEntry;

    fn manifest() -> PinnedRegistryManifest {
        PinnedRegistryManifest {
            publication_id: Some("pub-1".into()),
            registry_snapshot_version: Some(1),
            entries: vec![PinnedRegistryEntry {
                kind: "agent".into(),
                id: "agent-a".into(),
                version: 1,
                content_hash:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            }],
        }
    }

    #[test]
    fn activation_snapshot_serializes_pinned_scope_not_live() {
        let snapshot = RunActivationSnapshot {
            intent: RunIntent {
                agent_id: Some("agent-a".into()),
                thread_id: "thread".into(),
                kind: RunKind::NewIntent,
            },
            input: RunInputSnapshot {
                thread_id: "thread".into(),
                trigger_message_ids: vec!["msg-1".into()],
                ..Default::default()
            },
            options: RunOptions::default(),
            trace: RunTraceContext::default(),
            seeded_decisions: Vec::new(),
            resolution_scope: manifest(),
        };
        let value = serde_json::to_value(&snapshot).expect("serialize snapshot");
        assert!(value.get("resolution_scope").is_some());
        assert_eq!(value["intent"]["thread_id"], "thread");
    }

    #[test]
    fn legacy_origin_conversion_is_explicit() {
        assert_eq!(RunOrigin::from(RunRequestOrigin::A2A), RunOrigin::Subagent);
        assert_eq!(RunOrigin::from(RunRequestOrigin::Mcp), RunOrigin::Mcp);
        assert_eq!(
            RunRequestOrigin::from(RunOrigin::Mcp),
            RunRequestOrigin::Mcp
        );
        assert_eq!(
            RunRequestOrigin::from(RunOrigin::Internal),
            RunRequestOrigin::Internal
        );
    }
}
