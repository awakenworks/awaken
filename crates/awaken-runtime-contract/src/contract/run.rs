//! Run activation contract types.

use serde::{Deserialize, Serialize};

use super::identity::RunOrigin;
use super::inference::InferenceOverride;
use super::message::Message;
use super::storage::{MessageSeqRange, RunRequestOrigin, StorageError};
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

fn require_non_empty(field: &str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(StorageError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

fn require_optional_non_empty(field: &str, value: Option<&str>) -> Result<(), StorageError> {
    if let Some(value) = value {
        require_non_empty(field, value)?;
    }
    Ok(())
}

fn validate_message_id_list(field: &str, ids: &[String]) -> Result<(), StorageError> {
    for (idx, id) in ids.iter().enumerate() {
        require_non_empty(&format!("{field}[{idx}]"), id)?;
    }
    Ok(())
}

fn validate_seq_range(field: &str, range: MessageSeqRange) -> Result<(), StorageError> {
    if range.from_seq == 0 || range.from_seq > range.to_seq {
        return Err(StorageError::Validation(format!(
            "{field} range must be non-empty and 1-based"
        )));
    }
    Ok(())
}

impl RunIntent {
    /// Validate model-level invariants that cannot be represented by the
    /// public compatibility fields alone.
    pub fn validate(&self) -> Result<(), StorageError> {
        require_non_empty("run intent thread_id", &self.thread_id)?;
        require_optional_non_empty("run intent agent_id", self.agent_id.as_deref())?;
        self.kind.validate()
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

impl RunKind {
    pub fn validate(&self) -> Result<(), StorageError> {
        match self {
            Self::NewIntent => Ok(()),
            Self::HitlResume { run_id } => require_non_empty("hitl resume run_id", run_id),
            Self::ContinuationFromRun { run_id } => {
                require_non_empty("continuation run_id", run_id)
            }
        }
    }
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

impl RunInputSnapshot {
    pub fn validate(&self) -> Result<(), StorageError> {
        require_non_empty("run input snapshot thread_id", &self.thread_id)?;
        if let Some(range) = self.range {
            validate_seq_range("run input snapshot", range)?;
        }
        validate_message_id_list(
            "run input snapshot trigger_message_ids",
            &self.trigger_message_ids,
        )?;
        validate_message_id_list(
            "run input snapshot selected_message_ids",
            &self.selected_message_ids,
        )?;
        require_optional_non_empty(
            "run input snapshot context_policy",
            self.context_policy.as_deref(),
        )?;
        require_optional_non_empty(
            "run input snapshot compacted_snapshot_id",
            self.compacted_snapshot_id.as_deref(),
        )?;
        Ok(())
    }
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

impl RunTraceContext {
    pub fn validate(&self) -> Result<(), StorageError> {
        require_optional_non_empty("run trace parent_run_id", self.parent_run_id.as_deref())?;
        require_optional_non_empty(
            "run trace parent_thread_id",
            self.parent_thread_id.as_deref(),
        )?;
        require_optional_non_empty("run trace dispatch_id", self.dispatch_id.as_deref())?;
        require_optional_non_empty("run trace session_id", self.session_id.as_deref())?;
        require_optional_non_empty(
            "run trace transport_request_id",
            self.transport_request_id.as_deref(),
        )?;
        require_optional_non_empty("run trace correlation_id", self.correlation_id.as_deref())?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunActivationSnapshot {
    pub intent: RunIntent,
    pub input: RunInputSnapshot,
    pub options: RunOptions,
    pub trace: RunTraceContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeded_decisions: Vec<(String, ToolCallResume)>,
    /// Opaque id of the resolved registry binding for this activation. The
    /// server owns the referenced content; the runtime treats it as opaque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_id: Option<String>,
}

impl RunActivationSnapshot {
    pub fn validate(&self) -> Result<(), StorageError> {
        self.intent.validate()?;
        self.input.validate()?;
        self.trace.validate()?;
        if self.intent.thread_id != self.input.thread_id {
            return Err(StorageError::Validation(format!(
                "run activation intent.thread_id '{}' must match input.thread_id '{}'",
                self.intent.thread_id, self.input.thread_id
            )));
        }
        for (idx, (call_id, _)) in self.seeded_decisions.iter().enumerate() {
            require_non_empty(
                &format!("run activation seeded_decisions[{idx}].call_id"),
                call_id,
            )?;
        }
        require_optional_non_empty(
            "run activation resolution_id",
            self.resolution_id.as_deref(),
        )?;
        Ok(())
    }
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

    #[test]
    fn activation_snapshot_carries_resolution_id() {
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
            resolution_id: Some("resolution-1".into()),
        };
        let value = serde_json::to_value(&snapshot).expect("serialize snapshot");
        assert_eq!(value["resolution_id"], "resolution-1");
        assert_eq!(value["intent"]["thread_id"], "thread");
    }

    #[test]
    fn activation_snapshot_validate_rejects_thread_mismatch() {
        let snapshot = RunActivationSnapshot {
            intent: RunIntent::new("thread-a"),
            input: RunInputSnapshot {
                thread_id: "thread-b".into(),
                ..Default::default()
            },
            options: RunOptions::default(),
            trace: RunTraceContext::default(),
            seeded_decisions: Vec::new(),
            resolution_id: None,
        };

        let err = snapshot.validate().unwrap_err();

        assert!(matches!(err, StorageError::Validation(message)
            if message.contains("intent.thread_id") && message.contains("input.thread_id")));
    }

    #[test]
    fn activation_snapshot_validate_rejects_empty_resume_run_id() {
        let snapshot = RunActivationSnapshot {
            intent: RunIntent {
                agent_id: Some("agent-a".into()),
                thread_id: "thread".into(),
                kind: RunKind::HitlResume { run_id: " ".into() },
            },
            input: RunInputSnapshot {
                thread_id: "thread".into(),
                ..Default::default()
            },
            options: RunOptions::default(),
            trace: RunTraceContext::default(),
            seeded_decisions: Vec::new(),
            resolution_id: None,
        };

        let err = snapshot.validate().unwrap_err();

        assert!(matches!(err, StorageError::Validation(message)
            if message.contains("hitl resume run_id")));
    }

    #[test]
    fn activation_snapshot_validate_rejects_invalid_input_range() {
        let snapshot = RunActivationSnapshot {
            intent: RunIntent::new("thread"),
            input: RunInputSnapshot {
                thread_id: "thread".into(),
                range: Some(MessageSeqRange {
                    from_seq: 0,
                    to_seq: 1,
                }),
                ..Default::default()
            },
            options: RunOptions::default(),
            trace: RunTraceContext::default(),
            seeded_decisions: Vec::new(),
            resolution_id: None,
        };

        let err = snapshot.validate().unwrap_err();

        assert!(matches!(err, StorageError::Validation(message)
            if message.contains("range")));
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
