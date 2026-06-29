use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub run_id: crate::agent::run::Id,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    RunStarted,
    OutputText {
        text: String,
    },
    /// A tool call surfaced live as the assistant turn produced it, before the
    /// turn is committed. Best-effort progress (G10/G13).
    ToolCall {
        call_id: String,
        tool_id: String,
        arguments: serde_json::Value,
    },
    Waiting {
        reason: String,
    },
    RunFinished,
}
