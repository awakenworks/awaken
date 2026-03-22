use serde::{Deserialize, Serialize};

/// Dynamic agent spec overlay applied during handoff.
///
/// Each field, when `Some`, overrides the base agent configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOverlay {
    /// Override the system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Override the model ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whitelist of allowed tool IDs (None = all tools allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Explicit tool IDs to exclude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_tools: Option<Vec<String>>,
}
