//! Wire types for the `deployments` + `deployment-runs` resources
//! (`beta.deployments.*` / `beta.deploymentRuns.*`): `BetaManagedAgentsDeployment`
//! and `BetaManagedAgentsDeploymentRun`.
//!
//! Pure serde shapes. Every statically-known SDK union is decoded here; only
//! content blocks and rubric bodies reuse their existing neutral/typed owners.

use std::collections::BTreeMap;

use awaken_agent_contract::agent::content::ContentBlock;
use serde::{Deserialize, Serialize};

use crate::types::agent::AgentReference;
use crate::types::initial_event::{InitialEventClass, InitialEventSpec};
use crate::types::resource::ResourceInput;
use crate::types::session::{AgentRef, InboundEvent, OutcomeRubric};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum DeploymentInitialEvent {
    #[serde(rename = "user.message")]
    UserMessage { content: Vec<ContentBlock> },
    #[serde(rename = "system.message")]
    SystemMessage { content: Vec<ContentBlock> },
    #[serde(rename = "user.define_outcome")]
    UserDefineOutcome {
        description: String,
        rubric: OutcomeRubric,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
    },
}

impl InitialEventSpec for DeploymentInitialEvent {
    fn initial_event_class(&self) -> InitialEventClass {
        match self {
            Self::UserMessage { .. } => InitialEventClass::UserMessage,
            Self::SystemMessage { .. } => InitialEventClass::SystemMessage,
            Self::UserDefineOutcome { max_iterations, .. } => {
                InitialEventClass::UserDefineOutcome {
                    max_iterations: *max_iterations,
                }
            }
        }
    }
}

impl From<DeploymentInitialEvent> for InboundEvent {
    fn from(value: DeploymentInitialEvent) -> Self {
        match value {
            DeploymentInitialEvent::UserMessage { content } => InboundEvent::UserMessage {
                content,
                session_thread_id: None,
                model: None,
            },
            DeploymentInitialEvent::SystemMessage { content } => {
                InboundEvent::SystemMessage { content }
            }
            DeploymentInitialEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    Cron {
        expression: String,
        timezone: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_run_at: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        upcoming_runs_at: Vec<String>,
    },
}

impl Schedule {
    pub fn expression(&self) -> &str {
        match self {
            Schedule::Cron { expression, .. } => expression,
        }
    }

    pub fn timezone(&self) -> &str {
        match self {
            Schedule::Cron { timezone, .. } => timezone,
        }
    }

    pub fn with_runtime(&self, last_run_at: Option<String>, upcoming_runs_at: Vec<String>) -> Self {
        match self {
            Schedule::Cron {
                expression,
                timezone,
                ..
            } => Schedule::Cron {
                expression: expression.clone(),
                timezone: timezone.clone(),
                last_run_at,
                upcoming_runs_at,
            },
        }
    }
}

/// `DeploymentCreateParams` — the `POST /v1/deployments` body. `agent` is the
/// client input reference (id string or `{id, version?}`); the composite fields
/// the SDK models as unions (`initial_events`, `resources`, `schedule`) stay opaque
/// `Value`s.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentCreateParams {
    pub agent: AgentRef,
    pub environment_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub initial_events: Vec<DeploymentInitialEvent>,
    #[serde(default)]
    pub resources: Vec<ResourceInput>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub vault_ids: Vec<String>,
}

/// `DeploymentUpdateParams` — a partial update; every field replaces when present.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentUpdateParams {
    #[serde(default)]
    pub agent: Option<AgentRef>,
    #[serde(default)]
    pub environment_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub metadata: Option<Option<BTreeMap<String, Option<String>>>>,
    #[serde(default)]
    pub initial_events: Option<Vec<DeploymentInitialEvent>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub resources: Option<Option<Vec<ResourceInput>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub schedule: Option<Option<Schedule>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub vault_ids: Option<Option<Vec<String>>>,
}

/// `BetaManagedAgentsTriggerContext` — why a deployment run started. This surface
/// only mints manual runs; `Schedule` is modeled for wire completeness.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerContext {
    Manual,
    Schedule { scheduled_at: String },
}

/// `BetaManagedAgentsDeploymentPausedReason` — why a deployment is paused.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PausedReason {
    Manual,
    Error { error: PausedReasonError },
}

/// Error kinds that stop future scheduled fires until an operator unpauses the
/// deployment. This is the exact SDK paused-reason union; transient rate limits
/// and request validation failures deliberately do not appear here.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PausedReasonError {
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

/// Exact `BetaManagedAgentsDeploymentRun.error` tagged union.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunError {
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

impl RunError {
    #[must_use]
    pub fn paused_reason(&self) -> Option<PausedReasonError> {
        Some(match self {
            Self::EnvironmentArchivedError { .. } => PausedReasonError::EnvironmentArchivedError,
            Self::AgentArchivedError { .. } => PausedReasonError::AgentArchivedError,
            Self::EnvironmentNotFoundError { .. } => PausedReasonError::EnvironmentNotFoundError,
            Self::VaultNotFoundError { .. } => PausedReasonError::VaultNotFoundError,
            Self::VaultArchivedError { .. } => PausedReasonError::VaultArchivedError,
            Self::FileNotFoundError { .. } => PausedReasonError::FileNotFoundError,
            Self::MemoryStoreArchivedError { .. } => PausedReasonError::MemoryStoreArchivedError,
            Self::SkillNotFoundError { .. } => PausedReasonError::SkillNotFoundError,
            Self::SessionResourceNotFoundError { .. } => {
                PausedReasonError::SessionResourceNotFoundError
            }
            Self::WorkspaceArchivedError { .. } => PausedReasonError::WorkspaceArchivedError,
            Self::OrganizationDisabledError { .. } => PausedReasonError::OrganizationDisabledError,
            Self::UnknownError { .. } => PausedReasonError::UnknownError,
            Self::SelfHostedResourcesUnsupportedError { .. } => {
                PausedReasonError::SelfHostedResourcesUnsupportedError
            }
            Self::McpEgressBlockedError { .. } => PausedReasonError::McpEgressBlockedError,
            Self::SessionRateLimitedError { .. } | Self::SessionCreationRejectedError { .. } => {
                return None;
            }
        })
    }
}

/// `BetaManagedAgentsDeployment` — an agent bound to an environment with initial
/// events and a schedule.
#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub agent: AgentReference,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub description: Option<String>,
    pub environment_id: String,
    pub initial_events: Vec<DeploymentInitialEvent>,
    pub metadata: BTreeMap<String, String>,
    pub name: String,
    pub paused_reason: Option<PausedReason>,
    pub resources: Vec<ResourceInput>,
    pub schedule: Option<Schedule>,
    /// `"active"` | `"paused"`.
    pub status: &'static str,
    pub vault_ids: Vec<String>,
}

/// `BetaManagedAgentsDeploymentRun` — one triggered run of a deployment.
#[derive(Debug, Clone, Serialize)]
pub struct DeploymentRun {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub agent: AgentReference,
    pub created_at: String,
    pub deployment_id: String,
    pub error: Option<RunError>,
    pub session_id: Option<String>,
    pub trigger_context: TriggerContext,
}
