//! Anthropic Dreams research-preview wire vocabulary.

use serde::{Deserialize, Serialize};

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
