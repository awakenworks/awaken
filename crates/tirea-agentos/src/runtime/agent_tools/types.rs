//! Sub-agent types for agent run orchestration.
//!
//! These types model the persisted state of sub-agent runs
//! (created by `agent_run` / `agent_stop` / `agent_output` tools).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tirea_state::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentRemoteProtocol {
    A2a,
}

/// Durable execution reference for a delegated run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubAgentExecutionRef {
    /// Local child run persisted in the thread store.
    Local { thread_id: String },
    /// Remote delegated run tracked by protocol-specific identifiers.
    Remote {
        protocol: SubAgentRemoteProtocol,
        target_id: String,
        remote_context_id: Option<String>,
        remote_run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mirror_thread_id: Option<String>,
    },
}

impl SubAgentExecutionRef {
    #[must_use]
    pub fn local(thread_id: impl Into<String>) -> Self {
        Self::Local {
            thread_id: thread_id.into(),
        }
    }

    #[must_use]
    pub fn remote_a2a(
        target_id: impl Into<String>,
        remote_context_id: Option<String>,
        remote_run_id: impl Into<String>,
        mirror_thread_id: Option<String>,
    ) -> Self {
        Self::Remote {
            protocol: SubAgentRemoteProtocol::A2a,
            target_id: target_id.into(),
            remote_context_id,
            remote_run_id: remote_run_id.into(),
            mirror_thread_id,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn local_thread_id(&self) -> Option<&str> {
        match self {
            Self::Local { thread_id } => Some(thread_id.as_str()),
            Self::Remote { .. } => None,
        }
    }

    #[must_use]
    pub fn with_mirror_thread_id(self, mirror_thread_id: impl Into<String>) -> Self {
        match self {
            Self::Local { .. } => self,
            Self::Remote {
                protocol,
                target_id,
                remote_context_id,
                remote_run_id,
                ..
            } => Self::Remote {
                protocol,
                target_id,
                remote_context_id,
                remote_run_id,
                mirror_thread_id: Some(mirror_thread_id.into()),
            },
        }
    }

    #[must_use]
    pub fn output_thread_id(&self) -> Option<&str> {
        match self {
            Self::Local { thread_id } => Some(thread_id.as_str()),
            Self::Remote {
                mirror_thread_id, ..
            } => mirror_thread_id.as_deref(),
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn target_id(&self) -> Option<&str> {
        match self {
            Self::Local { .. } => None,
            Self::Remote { target_id, .. } => Some(target_id.as_str()),
        }
    }
}

impl From<String> for SubAgentExecutionRef {
    fn from(thread_id: String) -> Self {
        Self::local(thread_id)
    }
}

impl From<&str> for SubAgentExecutionRef {
    fn from(thread_id: &str) -> Self {
        Self::local(thread_id.to_string())
    }
}

/// Status of a sub-agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubAgentStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl SubAgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// Lightweight sub-agent metadata — no embedded Thread, no cached output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgent {
    /// Durable execution locator for the sub-agent run.
    #[serde(flatten)]
    pub execution: SubAgentExecutionRef,
    /// Parent caller run id (from caller runtime `run_id`), if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Target agent id.
    pub agent_id: String,
    /// Current run status.
    pub status: SubAgentStatus,
    /// Error message (if the run failed or was force-stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Persisted sub-agent state at `state["sub_agents"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "sub_agents", action = "SubAgentAction", scope = "thread")]
pub struct SubAgentState {
    /// Sub-agent runs keyed by `run_id`.
    #[tirea(default = "HashMap::new()")]
    pub runs: HashMap<String, SubAgent>,
}

/// Internal lifecycle action for `SubAgentState` reducer.
#[derive(Serialize, Deserialize)]
pub enum SubAgentAction {
    /// Set status of a sub-agent run (used by recovery plugin).
    SetStatus {
        run_id: String,
        status: SubAgentStatus,
        error: Option<String>,
    },
}

impl SubAgentState {
    fn reduce(&mut self, action: SubAgentAction) {
        match action {
            SubAgentAction::SetStatus {
                run_id,
                status,
                error,
            } => {
                if let Some(sub) = self.runs.get_mut(&run_id) {
                    sub.status = status;
                    sub.error = error;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_ref_local_thread_id_round_trips() {
        let execution = SubAgentExecutionRef::local("child-thread");
        assert_eq!(execution.local_thread_id(), Some("child-thread"));
        assert_eq!(execution.target_id(), None);
    }

    #[test]
    fn execution_ref_remote_a2a_carries_target_locator() {
        let execution = SubAgentExecutionRef::remote_a2a(
            "remote-worker",
            Some("ctx-1".to_string()),
            "task-1",
            Some("sub-agent-run-1".to_string()),
        );
        assert_eq!(execution.local_thread_id(), None);
        assert_eq!(execution.output_thread_id(), Some("sub-agent-run-1"));
        assert_eq!(execution.target_id(), Some("remote-worker"));
    }
}
