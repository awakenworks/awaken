use crate::runtime::inference::transform::InferenceRequestTransform;
use crate::runtime::tool_call::ToolDescriptor;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Context window management policy.
///
/// Pure data struct used by [`ContextWindowPlugin`] to configure the
/// context window transform. Lives in the contract layer so plugin crates
/// can construct it without depending on `tirea-agentos`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextWindowPolicy {
    /// Model's total context window size in tokens.
    pub max_context_tokens: usize,
    /// Tokens reserved for model output.
    pub max_output_tokens: usize,
    /// Minimum number of recent messages to always preserve (never truncated).
    pub min_recent_messages: usize,
    /// Whether to enable prompt caching (Anthropic `cache_control: ephemeral`).
    pub enable_prompt_cache: bool,
    /// Token count threshold that triggers auto-compaction. `None` disables.
    /// Used by ContextManagerPlugin to decide when to compact history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocompact_threshold: Option<usize>,
}

impl Default for ContextWindowPolicy {
    fn default() -> Self {
        Self {
            max_context_tokens: 200_000,
            max_output_tokens: 16_384,
            min_recent_messages: 10,
            enable_prompt_cache: true,
            autocompact_threshold: None,
        }
    }
}

/// Inference-phase extension: system/session context and tool descriptors.
///
/// Populated by `AddSystemContext`, `AddSessionContext`, `ExcludeTool`,
/// `IncludeOnlyTools`, `AddRequestTransform` actions during `BeforeInference`.
#[derive(Default, Clone)]
pub struct InferenceContext {
    /// System context lines appended to the system prompt.
    pub system_context: Vec<String>,
    /// Session context messages injected before user messages.
    pub session_context: Vec<String>,
    /// Available tool descriptors (can be filtered by actions).
    pub tools: Vec<ToolDescriptor>,
    /// Request transforms registered by plugins. Applied in order after
    /// messages are assembled, before the request is sent to the LLM.
    pub request_transforms: Vec<Arc<dyn InferenceRequestTransform>>,
}

impl std::fmt::Debug for InferenceContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceContext")
            .field("system_context", &self.system_context)
            .field("session_context", &self.session_context)
            .field("tools", &self.tools)
            .field("request_transforms", &self.request_transforms.len())
            .finish()
    }
}
