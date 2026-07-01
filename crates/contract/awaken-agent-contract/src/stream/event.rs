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
    /// A run-end guard decided the run's continuation at a natural-end boundary
    /// (run-end continuation guard). `steered` is true when the guard fed another
    /// turn, false when it let the run end. `detail` is opaque to the kernel — an
    /// extension's own classification, forwarded verbatim so the host can project
    /// it without the runtime learning the extension's vocabulary.
    Continuation {
        steered: bool,
        detail: serde_json::Value,
    },
    RunFinished,
}
