//! Projection of one externally answerable committed await target.

/// The tool a run awaits: its id, model-visible name/input, and whether it is
/// client-executed (projected as `agent.custom_tool_use`) or a built-in awaiting
/// confirmation (`agent.tool_use{ask}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

impl Pending {
    /// Classify the externally answerable value directly from the one committed
    /// [`awaken_agent_contract::agent::awaiting::ResumeTicket`]. Host execution,
    /// Session application, and protocol recovery must all use this decoder so
    /// warm and cold reads cannot disagree with the committed projection.
    pub fn from_resume_ticket(
        ticket: &awaken_agent_contract::agent::awaiting::ResumeTicket,
    ) -> Option<Self> {
        Self::from_await_target(ticket.target())
    }

    /// Rebuild the same externally answerable projection from a historical
    /// committed Awaiting fact after its active resume ticket has been consumed.
    /// Both live and cold paths share this decoder so they cannot classify the
    /// closed target differently.
    pub fn from_await_target(
        target: &awaken_agent_contract::agent::awaiting::AwaitTarget,
    ) -> Option<Self> {
        use awaken_agent_contract::agent::awaiting::{AwaitTarget, ToolAwaitReason};

        match target {
            AwaitTarget::ToolCall {
                reason: reason @ (ToolAwaitReason::Permission | ToolAwaitReason::ClientExecution),
                call_id,
                tool,
            } => Some(Self {
                tool_use_id: call_id.clone(),
                name: tool.tool_id.clone(),
                input: tool.arguments.clone(),
                client_executed: *reason == ToolAwaitReason::ClientExecution,
            }),
            AwaitTarget::RemoteInput { call_id, .. } => Some(Self {
                tool_use_id: call_id.clone(),
                name: "agent_input".to_string(),
                input: serde_json::json!({ "reason": target.reason().as_stream_str() }),
                client_executed: true,
            }),
            AwaitTarget::ToolCall {
                reason: ToolAwaitReason::ScheduledAction | ToolAwaitReason::Delegation,
                ..
            }
            | AwaitTarget::Pause(_) => None,
        }
    }
}
