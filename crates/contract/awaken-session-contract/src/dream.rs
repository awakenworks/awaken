//! Neutral Coordinator Dream request and projection vocabulary.

use serde::{Deserialize, Serialize};

/// Typed durable Coordinator-owned Dream process. Execution usage deliberately
/// remains absent because the linked ordinary Session owns that fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamProcessRecord {
    #[serde(alias = "id")]
    pub process_id: String,
    pub workspace_id: String,
    pub status: DreamStatus,
    pub source_memory_store_id: String,
    pub session_ids: Vec<String>,
    pub model: DreamModelConfig,
    pub request_guidance: Option<String>,
    pub agent_selection: DreamAgentSelectionRecord,
    pub result_memory_store_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_file_ids: Vec<String>,
    #[serde(default)]
    pub cleanup_pending: bool,
    pub created_at: u64,
    pub ended_at: Option<u64>,
    pub archived_at: Option<u64>,
    pub error: Option<DreamProcessFailure>,
    #[serde(default)]
    pub policy_key: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamAgentSelectionRecord {
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamProcessFailure {
    pub kind: String,
    pub message: String,
}

/// Typed durable automatic Dream policy and scheduling cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamPolicyRecord {
    pub workspace_id: String,
    pub memory_store_id: String,
    pub config: DreamPolicyConfig,
    pub next_due_ms: u64,
    pub last_completed_cutoff_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDreamAgentOverride {
    pub workspace_id: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DreamProcessStoreError {
    #[error("Dream process store failure: {0}")]
    Storage(String),
}

/// Coordinator persistence port for cross-Session Dream processes and the
/// atomic scheduled-occurrence claim. It is deliberately separate from Memory.
pub trait DreamProcessStore: Send + Sync {
    fn dream_processes(&self) -> Result<Vec<DreamProcessRecord>, DreamProcessStoreError>;
    fn compare_and_swap_dream_process(
        &self,
        expected: Option<&DreamProcessRecord>,
        record: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError>;
    fn dream_agent_overrides(
        &self,
    ) -> Result<Vec<WorkspaceDreamAgentOverride>, DreamProcessStoreError>;
    fn set_dream_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), DreamProcessStoreError>;
    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamProcessStoreError>;
    fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&DreamPolicyRecord>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamProcessStoreError>;
    fn claim_dream_policy(
        &self,
        expected_policy: &DreamPolicyRecord,
        policy: DreamPolicyRecord,
        process: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DreamInput {
    MemoryStore { memory_store_id: String },
    Sessions { session_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DreamOutput {
    pub memory_store_id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamModelSpeed {
    Standard,
    Fast,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DreamModelConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<DreamModelSpeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamPolicyConfig {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub min_new_sessions: usize,
    pub max_sessions: usize,
    pub model: DreamModelConfig,
    pub instructions: Option<String>,
}

impl Default for DreamPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 24 * 60 * 60,
            min_new_sessions: 5,
            max_sessions: 100,
            model: DreamModelConfig {
                id: "claude-sonnet-5".into(),
                speed: None,
            },
            instructions: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum DreamModelInput {
    Id(String),
    Config(DreamModelConfig),
}

impl DreamModelInput {
    #[must_use]
    pub fn into_config(self) -> DreamModelConfig {
        match self {
            Self::Id(id) => DreamModelConfig { id, speed: None },
            Self::Config(config) => config,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl DreamStatus {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamUsage {
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DreamError {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dream {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub ended_at: Option<String>,
    pub error: Option<DreamError>,
    pub inputs: Vec<DreamInput>,
    pub instructions: Option<String>,
    pub model: DreamModelConfig,
    pub outputs: Vec<DreamOutput>,
    pub session_id: Option<String>,
    pub status: DreamStatus,
    pub usage: DreamUsage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DreamCreateParams {
    pub inputs: Vec<DreamInput>,
    pub model: DreamModelInput,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DreamListParams {
    #[serde(rename = "created_at[gt]")]
    pub created_after: Option<String>,
    #[serde(rename = "created_at[lt]")]
    pub created_before: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default)]
    pub statuses: Vec<DreamStatus>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub page: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DreamPage {
    pub data: Vec<Dream>,
    pub next_page: Option<String>,
}
