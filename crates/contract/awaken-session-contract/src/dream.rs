//! Neutral Coordinator Dream request and projection vocabulary.

use serde::{Deserialize, Serialize};

/// Public authoring limits for the research-preview Dream API. The Managed
/// adapter, Awaken capability projection, and Console all consume this one
/// contract rather than copying model or validation lists.
pub const DREAM_MAX_INSTRUCTIONS_CHARS: usize = 4096;
pub const DREAM_MAX_SESSIONS: usize = 100;
pub const DREAM_SUPPORTED_MODELS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
];

/// Typed durable Coordinator-owned Dream process. Execution usage deliberately
/// remains absent because the linked ordinary Session owns that fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DreamProcessRecord {
    #[serde(alias = "id")]
    pub process_id: String,
    pub workspace_id: String,
    pub status: DreamStatus,
    pub source_memory_store_id: String,
    pub session_ids: Vec<String>,
    pub model: DreamModelConfig,
    pub request_guidance: Option<String>,
    pub agent_id: String,
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

#[derive(Deserialize)]
struct StoredDreamProcessRecord {
    #[serde(alias = "id")]
    process_id: String,
    workspace_id: String,
    status: DreamStatus,
    source_memory_store_id: String,
    session_ids: Vec<String>,
    model: DreamModelConfig,
    request_guidance: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    agent_selection: Option<LegacyDreamAgentSelection>,
    result_memory_store_id: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    transcript_file_ids: Vec<String>,
    #[serde(default)]
    cleanup_pending: bool,
    created_at: u64,
    ended_at: Option<u64>,
    archived_at: Option<u64>,
    error: Option<DreamProcessFailure>,
    #[serde(default)]
    policy_key: Option<(String, String)>,
}

#[derive(Deserialize)]
struct LegacyDreamAgentSelection {
    agent_id: String,
}

impl<'de> Deserialize<'de> for DreamProcessRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let stored = StoredDreamProcessRecord::deserialize(deserializer)?;
        let agent_id = stored
            .agent_id
            .or_else(|| stored.agent_selection.map(|legacy| legacy.agent_id))
            .ok_or_else(|| serde::de::Error::missing_field("agent_id"))?;
        Ok(Self {
            process_id: stored.process_id,
            workspace_id: stored.workspace_id,
            status: stored.status,
            source_memory_store_id: stored.source_memory_store_id,
            session_ids: stored.session_ids,
            model: stored.model,
            request_guidance: stored.request_guidance,
            agent_id,
            result_memory_store_id: stored.result_memory_store_id,
            session_id: stored.session_id,
            transcript_file_ids: stored.transcript_file_ids,
            cleanup_pending: stored.cleanup_pending,
            created_at: stored.created_at,
            ended_at: stored.ended_at,
            archived_at: stored.archived_at,
            error: stored.error,
            policy_key: stored.policy_key,
        })
    }
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

/// Awaken extension projection for the opt-in automatic policy of one
/// Workspace-owned MemoryStore. Absence projects the disabled effective default
/// without creating a durable row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DreamPolicy {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub memory_store_id: String,
    #[serde(flatten)]
    pub config: DreamPolicyConfig,
    pub next_due_at: Option<String>,
    pub last_completed_cutoff_at: Option<String>,
}

/// Neutral application failure vocabulary consumed by protocol adapters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DreamPolicyApplicationError {
    #[error("{0}")]
    BadRequest(String),
    #[error("Dream policy was not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
}

/// Application port driven by the Awaken policy protocol adapter.
///
/// Workspace selection is resolved and authorized at the trusted process boundary; the
/// protocol adapter only forwards that opaque ownership coordinate.
pub trait DreamPolicyApplication: Send + Sync {
    fn policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<DreamPolicy, DreamPolicyApplicationError>;

    fn set_policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config: DreamPolicyConfig,
    ) -> Result<(), DreamPolicyApplicationError>;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> DreamProcessRecord {
        DreamProcessRecord {
            process_id: "dream-1".into(),
            workspace_id: "workspace".into(),
            status: DreamStatus::Pending,
            source_memory_store_id: "memory".into(),
            session_ids: vec!["session".into()],
            model: DreamModelConfig {
                id: "claude-sonnet-5".into(),
                speed: None,
            },
            request_guidance: None,
            agent_id: "awaken_builtin_dream_agent".into(),
            result_memory_store_id: None,
            session_id: None,
            transcript_file_ids: Vec::new(),
            cleanup_pending: false,
            created_at: 1,
            ended_at: None,
            archived_at: None,
            error: None,
            policy_key: None,
        }
    }

    #[test]
    fn dream_agent_id_has_one_current_shape_and_reads_legacy_records() {
        // Cause/effect decision table: C1 current record -> E1 direct agent_id
        // only; C2 legacy agent_selection wrapper -> E2 decode to the same
        // canonical record; C3 neither form -> E3 reject corrupt persistence.
        let expected = record();
        let current = serde_json::to_value(&expected).expect("R1");
        assert_eq!(current["agent_id"], expected.agent_id, "R1");
        assert!(current.get("agent_selection").is_none(), "R1");

        let mut legacy = current.clone();
        legacy.as_object_mut().unwrap().remove("agent_id");
        legacy["agent_selection"] = serde_json::json!({"agent_id": "awaken_builtin_dream_agent"});
        assert_eq!(
            serde_json::from_value::<DreamProcessRecord>(legacy).expect("R2"),
            expected,
            "R2"
        );

        let mut invalid = current;
        invalid.as_object_mut().unwrap().remove("agent_id");
        assert!(
            serde_json::from_value::<DreamProcessRecord>(invalid).is_err(),
            "R3"
        );
    }
}
