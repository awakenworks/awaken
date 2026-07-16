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
    /// A live increment of a tool call's input, as the model streams it, before the
    /// run commits. `args_delta` is the NEW fragment only — the provider adapter
    /// de-accumulates its own cumulative snapshots, so a consumer forwards it
    /// directly with no diffing. Best-effort progress (G10/G13); the committed call
    /// (parsed input) comes from the fold, not this.
    ToolCallDelta {
        call_id: String,
        tool_id: String,
        args_delta: String,
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
    /// The run ended on an execution fault. Emitted before the terminal
    /// `RunFinished` so hosts keep one close signal; `code` is the fault's
    /// stable snake_case classification. Best-effort like every stream event —
    /// the committed `Phase` remains the authority.
    RunFailed {
        code: String,
        message: String,
    },
    RunFinished,
}
