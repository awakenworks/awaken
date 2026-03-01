use crate::runtime::tool_call::ToolDescriptor;

/// Inference-phase extension: system/session context and tool descriptors.
///
/// Populated by `AddSystemContext`, `AddSessionContext`, `ExcludeTool`,
/// `IncludeOnlyTools` actions during `BeforeInference`.
#[derive(Debug, Default, Clone)]
pub struct InferenceContext {
    /// System context lines appended to the system prompt.
    pub system_context: Vec<String>,
    /// Session context messages injected before user messages.
    pub session_context: Vec<String>,
    /// Available tool descriptors (can be filtered by actions).
    pub tools: Vec<ToolDescriptor>,
}
