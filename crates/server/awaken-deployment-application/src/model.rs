use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn agent_kind() -> String {
    "agent".to_string()
}

/// Immutable executable Agent coordinate frozen when a Deployment is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentAgent {
    pub id: String,
    #[serde(rename = "type", default = "agent_kind")]
    kind: String,
    pub version: u64,
}

impl DeploymentAgent {
    #[must_use]
    pub fn new(id: impl Into<String>, version: u64) -> Self {
        Self {
            id: id.into(),
            kind: agent_kind(),
            version,
        }
    }
}

/// Protocol-independent selector admitted by the public edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSelector {
    pub id: String,
    pub version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentSchedule {
    Cron {
        expression: String,
        timezone: String,
        #[serde(default)]
        last_run_at: Option<String>,
        #[serde(default)]
        upcoming_runs_at: Vec<String>,
    },
}

impl DeploymentSchedule {
    #[must_use]
    pub fn expression(&self) -> &str {
        match self {
            Self::Cron { expression, .. } => expression,
        }
    }

    #[must_use]
    pub fn timezone(&self) -> &str {
        match self {
            Self::Cron { timezone, .. } => timezone,
        }
    }

    #[must_use]
    pub fn with_runtime(&self, last_run_at: Option<String>, upcoming_runs_at: Vec<String>) -> Self {
        match self {
            Self::Cron {
                expression,
                timezone,
                ..
            } => Self::Cron {
                expression: expression.clone(),
                timezone: timezone.clone(),
                last_run_at,
                upcoming_runs_at,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    Active,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentPauseError {
    EnvironmentArchivedError,
    AgentArchivedError,
    EnvironmentNotFoundError,
    VaultNotFoundError,
    FileNotFoundError,
    SessionResourceNotFoundError,
    WorkspaceArchivedError,
    OrganizationDisabledError,
    MemoryStoreArchivedError,
    SkillNotFoundError,
    VaultArchivedError,
    UnknownError,
    SelfHostedResourcesUnsupportedError,
    McpEgressBlockedError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentPauseReason {
    Manual,
    Error { error: DeploymentPauseError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentRunFailure {
    EnvironmentArchivedError { message: String },
    AgentArchivedError { message: String },
    EnvironmentNotFoundError { message: String },
    VaultNotFoundError { message: String },
    VaultArchivedError { message: String },
    FileNotFoundError { message: String },
    MemoryStoreArchivedError { message: String },
    SkillNotFoundError { message: String },
    SessionResourceNotFoundError { message: String },
    WorkspaceArchivedError { message: String },
    OrganizationDisabledError { message: String },
    SessionRateLimitedError { message: String },
    SessionCreationRejectedError { message: String },
    UnknownError { message: String },
    SelfHostedResourcesUnsupportedError { message: String },
    McpEgressBlockedError { message: String },
}

impl DeploymentRunFailure {
    #[must_use]
    pub fn pause_error(&self) -> Option<DeploymentPauseError> {
        Some(match self {
            Self::EnvironmentArchivedError { .. } => DeploymentPauseError::EnvironmentArchivedError,
            Self::AgentArchivedError { .. } => DeploymentPauseError::AgentArchivedError,
            Self::EnvironmentNotFoundError { .. } => DeploymentPauseError::EnvironmentNotFoundError,
            Self::VaultNotFoundError { .. } => DeploymentPauseError::VaultNotFoundError,
            Self::VaultArchivedError { .. } => DeploymentPauseError::VaultArchivedError,
            Self::FileNotFoundError { .. } => DeploymentPauseError::FileNotFoundError,
            Self::MemoryStoreArchivedError { .. } => DeploymentPauseError::MemoryStoreArchivedError,
            Self::SkillNotFoundError { .. } => DeploymentPauseError::SkillNotFoundError,
            Self::SessionResourceNotFoundError { .. } => {
                DeploymentPauseError::SessionResourceNotFoundError
            }
            Self::WorkspaceArchivedError { .. } => DeploymentPauseError::WorkspaceArchivedError,
            Self::OrganizationDisabledError { .. } => {
                DeploymentPauseError::OrganizationDisabledError
            }
            Self::UnknownError { .. } => DeploymentPauseError::UnknownError,
            Self::SelfHostedResourcesUnsupportedError { .. } => {
                DeploymentPauseError::SelfHostedResourcesUnsupportedError
            }
            Self::McpEgressBlockedError { .. } => DeploymentPauseError::McpEgressBlockedError,
            Self::SessionRateLimitedError { .. } | Self::SessionCreationRejectedError { .. } => {
                return None;
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeploymentTrigger {
    Manual,
    Schedule { scheduled_at: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    #[serde(default)]
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
    pub workspace_id: String,
    pub agent: DeploymentAgent,
    pub environment_id: String,
    pub name: String,
    pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub initial_events: Vec<Value>,
    pub resources: Vec<Value>,
    pub schedule: Option<DeploymentSchedule>,
    pub vault_ids: Vec<String>,
    pub status: DeploymentStatus,
    pub paused_reason: Option<DeploymentPauseReason>,
    pub archived_at: Option<String>,
    pub last_run_at: Option<String>,
    pub next_fire_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentRunRecord {
    pub created_at: String,
    pub deployment_id: String,
    pub workspace_id: String,
    pub agent: DeploymentAgent,
    pub trigger: DeploymentTrigger,
    pub session_id: Option<String>,
    pub error: Option<DeploymentRunFailure>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentView {
    pub id: String,
    pub record: DeploymentRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentRunView {
    pub id: String,
    pub record: DeploymentRunRecord,
}

#[derive(Debug, Clone)]
pub struct CreateDeploymentCommand {
    pub workspace_id: String,
    pub agent: AgentSelector,
    pub environment_id: String,
    pub name: String,
    pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub initial_events: Vec<Value>,
    pub resources: Vec<Value>,
    pub schedule: Option<DeploymentSchedule>,
    pub vault_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateDeploymentCommand {
    pub agent: Option<AgentSelector>,
    pub environment_id: Option<String>,
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub metadata: Option<Option<BTreeMap<String, Option<String>>>>,
    pub initial_events: Option<Vec<Value>>,
    pub resources: Option<Option<Vec<Value>>>,
    pub schedule: Option<Option<DeploymentSchedule>>,
    pub vault_ids: Option<Option<Vec<String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentLaunch {
    pub deployment_id: String,
    pub deployment_run_id: String,
    pub workspace_id: String,
    pub agent: DeploymentAgent,
    pub environment_id: String,
    pub metadata: BTreeMap<String, String>,
    pub initial_events: Vec<Value>,
    pub resources: Vec<Value>,
    pub vault_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum DeploymentLaunchOutcome {
    Created { session_id: String },
    Failed { error: DeploymentRunFailure },
    Unavailable { message: String },
}
