use std::collections::BTreeMap;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_resource_contract::ResourceAccess;
use serde::{Deserialize, Serialize};

fn agent_kind() -> DeploymentAgentKind {
    DeploymentAgentKind::Agent
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeploymentAgentKind {
    Agent,
}

/// Immutable executable Agent coordinate frozen when a Deployment is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentAgent {
    pub id: String,
    #[serde(rename = "type", default = "agent_kind")]
    kind: DeploymentAgentKind,
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

/// Durable initial intent applied whenever this Deployment creates a Session.
/// It deliberately contains only the closed subset admitted for repeatable
/// launches; interactive Session events remain outside this aggregate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum DeploymentSeedEvent {
    // Before this enum became typed, Deployment records stored accepted
    // Managed API JSON verbatim. Dotted aliases read those historical rows;
    // canonical writes remain protocol-independent snake_case domain data.
    #[serde(rename = "user_message", alias = "user.message")]
    UserMessage { content: Vec<ContentBlock> },
    #[serde(rename = "system_message", alias = "system.message")]
    SystemMessage { content: Vec<ContentBlock> },
    #[serde(rename = "define_outcome", alias = "user.define_outcome")]
    DefineOutcome {
        description: String,
        rubric: DeploymentOutcomeRubric,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeploymentOutcomeRubric {
    Text { content: String },
    File { file_id: String },
}

/// Secret-free Resource intent frozen into a Deployment. Repository bearer
/// material is intentionally absent: repeatable launches resolve credentials by
/// durable reference at the Session boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeploymentResource {
    File {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<ResourceAccess>,
    },
    GithubRepository {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<DeploymentRepositoryCheckout>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeploymentRepositoryCheckout {
    Branch { name: String },
    Commit { sha: String },
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
    pub initial_events: Vec<DeploymentSeedEvent>,
    pub resources: Vec<DeploymentResource>,
    pub schedule: Option<DeploymentSchedule>,
    pub vault_ids: Vec<String>,
    #[serde(default)]
    pub budget_max_list_cost_minor: Option<u64>,
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
    pub initial_events: Vec<DeploymentSeedEvent>,
    pub resources: Vec<DeploymentResource>,
    pub schedule: Option<DeploymentSchedule>,
    pub vault_ids: Vec<String>,
    pub budget_max_list_cost_minor: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateDeploymentCommand {
    pub agent: Option<AgentSelector>,
    pub environment_id: Option<String>,
    pub name: Option<String>,
    pub description: Option<FieldUpdate<String>>,
    pub metadata: Option<MetadataUpdate>,
    pub initial_events: Option<Vec<DeploymentSeedEvent>>,
    pub resources: Option<FieldUpdate<Vec<DeploymentResource>>>,
    pub schedule: Option<FieldUpdate<DeploymentSchedule>>,
    pub vault_ids: Option<FieldUpdate<Vec<String>>>,
    pub budget_max_list_cost_minor: Option<FieldUpdate<u64>>,
}

/// An explicit change to an optional aggregate field. Absence from the command
/// means "leave unchanged"; this enum distinguishes clearing from replacement
/// without leaking a protocol's `Option<Option<T>>` encoding into the domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldUpdate<T> {
    Clear,
    Replace(T),
}

/// Metadata has patch semantics rather than whole-value replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataUpdate {
    Clear,
    Patch(BTreeMap<String, Option<String>>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentLaunch {
    pub deployment_id: String,
    pub deployment_run_id: String,
    pub workspace_id: String,
    pub agent: DeploymentAgent,
    pub environment_id: String,
    pub metadata: BTreeMap<String, String>,
    pub initial_events: Vec<DeploymentSeedEvent>,
    pub resources: Vec<DeploymentResource>,
    pub vault_ids: Vec<String>,
    pub budget_max_list_cost_minor: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum DeploymentLaunchOutcome {
    Created { session_id: String },
    Failed { error: DeploymentRunFailure },
    Unavailable { message: String },
}

#[cfg(test)]
mod tests {
    use super::DeploymentSeedEvent;
    use serde_json::{Value, json};

    #[test]
    fn deployment_seed_events_read_managed_names_and_serialize_as_domain_data() {
        // Historical rows contain the Managed API's `{domain}.{action}` names.
        // They must remain readable without making that public vocabulary the
        // canonical encoding of the protocol-independent domain type.
        let cases = [
            (
                json!({"type":"user.message","content":[{"type":"text","text":"go"}]}),
                "user_message",
            ),
            (
                json!({"type":"system.message","content":[{"type":"text","text":"policy"}]}),
                "system_message",
            ),
            (
                json!({
                    "type":"user.define_outcome",
                    "description":"produce the report",
                    "rubric":{"type":"text","content":"complete and sourced"},
                    "max_iterations":3
                }),
                "define_outcome",
            ),
        ];

        for (input, expected_type) in cases {
            let event: DeploymentSeedEvent =
                serde_json::from_value(input).expect("historical Managed event must decode");
            let stored = serde_json::to_value(event).expect("Deployment event must encode");
            assert_eq!(stored["type"], Value::String(expected_type.into()));
        }
    }

    #[test]
    fn deployment_seed_events_round_trip_canonical_snake_case() {
        // Cause/effect migration table:
        // official dotted record -> migration restore; canonical snake_case
        // record -> exact restore; every new write -> canonical snake_case.
        let cases = [
            (
                json!({"type":"user_message","content":[{"type":"text","text":"go"}]}),
                "user_message",
            ),
            (
                json!({"type":"system_message","content":[{"type":"text","text":"policy"}]}),
                "system_message",
            ),
            (
                json!({
                    "type":"define_outcome",
                    "description":"produce the report",
                    "rubric":{"type":"text","content":"complete and sourced"}
                }),
                "define_outcome",
            ),
        ];

        for (input, expected_type) in cases {
            let event: DeploymentSeedEvent =
                serde_json::from_value(input).expect("transitional record must remain restorable");
            let rewritten = serde_json::to_value(event).expect("Deployment event must encode");
            assert_eq!(rewritten["type"], Value::String(expected_type.into()));
        }
    }
}
