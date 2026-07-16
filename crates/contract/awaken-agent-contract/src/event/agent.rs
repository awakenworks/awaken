//! The one neutral event vocabulary (ADR-0058, Axis 2).
//!
//! `AgentEvent` is the single shape every producer emits and every protocol
//! projects from. It replaces the three parallel enums that used to overlap
//! (`stream::event::Kind` live increments, `project::AgentEvent` committed
//! whole-units, and the content-lifecycle half of `audit::RunEvent`). It is a
//! **read/emit projection**, never stored truth — the message log stays canonical
//! (Axis 1).
//!
//! Two nested tiers encode producer authority (Axis 6):
//!
//! - [`Committed`] — authoritative whole-units and run lifecycle, produced only by
//!   the **fold** over committed messages/phase. The compiler forces every protocol
//!   to take a stance on each variant (exhaustive tier).
//! - [`Progress`] — best-effort, high-frequency increments, produced only by the
//!   live **stream**. A protocol opts in to just the increments it renders; adding
//!   one never fans out to every encoder (opt-in tier).
//!
//! No variant has two producers, so a consumer never has to ask whether a given
//! `RunFinished` is best-effort or authoritative — it can only be `Committed`.

use serde_json::Value;

use crate::agent::content::ContentBlock;
use crate::project::ToolDisposition;

/// One neutral event, tagged by its producer-authority tier. Carries no protocol
/// vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// An authoritative whole-unit or lifecycle fact, folded from committed truth.
    Committed(Committed),
    /// A best-effort live increment, streamed pre-commit.
    Progress(Progress),
}

/// The authoritative tier: whole-units and run lifecycle. Only the fold produces
/// these. Every protocol transcoder handles every variant (exhaustive).
#[derive(Debug, Clone, PartialEq)]
pub enum Committed {
    /// The run began (a run boundary).
    RunStarted,
    /// An assistant message's content (text — and, once folded, thinking blocks).
    AssistantMessage {
        id: String,
        content: Vec<ContentBlock>,
    },
    /// The assistant called a tool, with how it was dispatched.
    ToolCall {
        id: String,
        name: String,
        input: Value,
        disposition: ToolDisposition,
    },
    /// A tool produced a result.
    ToolResult {
        id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    /// The run parked awaiting a decision on the named pending tool.
    Waiting { pending_tool_use_id: Option<String> },
    /// A run-end continuation guard decided one round. `steered` is whether the
    /// guard injected steering; `detail` is the guard's opaque payload.
    Continuation { steered: bool, detail: Value },
    /// The run reached a natural or budget-exhausted terminus.
    RunFinished { exhausted: bool },
    /// The run ended on an execution fault. `code` is the fault's stable
    /// snake_case classification (e.g. `unauthorized`, `context_overflow`).
    RunFailed { code: String, message: String },
}

/// The best-effort tier: fine-grained increments. Only the live stream produces
/// these. A protocol renders only the increments it cares about (opt-in).
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// A fragment of assistant text.
    TextDelta { delta: String },
    /// A fragment of model reasoning (best-effort; the committed form is a folded
    /// thinking block — Axis 10b).
    ReasoningDelta { delta: String },
    /// A tool-argument fragment — always a de-accumulated **suffix** (the provider
    /// adapter owns de-accumulation, Axis 10).
    ToolCallDelta {
        id: String,
        name: String,
        args_delta: String,
    },
}

impl AgentEvent {
    /// Convenience: wrap a committed whole-unit.
    pub fn committed(c: Committed) -> Self {
        AgentEvent::Committed(c)
    }

    /// Convenience: wrap a live increment.
    pub fn progress(p: Progress) -> Self {
        AgentEvent::Progress(p)
    }
}
