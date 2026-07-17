//! The one neutral event vocabulary (ADR-0058, Axis 2).
//!
//! `AgentEvent` is the single shape every producer emits and every protocol
//! projects from. It replaces the three parallel enums that used to overlap
//! (`stream::event::Kind` live increments, `project::AgentEvent` committed
//! whole-units, and the content-lifecycle half of `audit::RunEvent`). It is a
//! **read/emit projection**, never stored truth — the message log stays canonical
//! (Axis 1).
//!
//! Two nested tiers name what a consumer must do with each event — its *shape*,
//! not our storage:
//!
//! - [`Fact`] — a discrete, complete event: something that definitively happened
//!   (a whole message, a finished tool call, a run-lifecycle transition). Produced
//!   only by the **fold** over committed messages/phase. The compiler forces every
//!   protocol to take a stance on each variant (exhaustive tier).
//! - [`Delta`] — a streaming fragment of content still being produced. Best-effort,
//!   high-frequency, from the live **stream**, never durable truth. A protocol opts
//!   in to just the fragments it renders (opt-in tier).
//!
//! No variant has two producers, so a consumer never has to ask whether a given
//! `RunFinished` is best-effort or authoritative — it can only be a `Fact`.

use serde_json::Value;

use crate::agent::content::ContentBlock;

/// How a tool call was dispatched, as seen at fold time. The only place the "who
/// runs the tool" distinction is carried; each transcoder maps it to its own
/// tool-part shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDisposition {
    /// The tool ran server-side; a [`Fact::ToolResult`] follows.
    Executed,
    /// A client-executed tool the run parked on; the client runs it and returns
    /// the result.
    PendingClient,
    /// A built-in tool the run parked on, awaiting a permission decision.
    PendingBuiltin,
}

/// One neutral event, tagged by its producer-authority tier. Carries no protocol
/// vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A discrete complete event — something that definitively happened, folded
    /// from committed truth.
    Fact(Fact),
    /// A streaming fragment of content still being produced, best-effort.
    Delta(Delta),
}

/// The discrete-event tier: whole units and run lifecycle. Only the fold produces
/// these. Every protocol transcoder handles every variant (exhaustive).
#[derive(Debug, Clone, PartialEq)]
pub enum Fact {
    /// The run began (a run boundary).
    RunStarted,
    /// An assistant message's content (text blocks only; reasoning is a separate
    /// `AssistantThinking` marker).
    AssistantMessage {
        id: String,
        content: Vec<ContentBlock>,
    },
    /// The turn produced extended-thinking (reasoning) content — a contentless
    /// forward-progress marker, emitted before the answer. A protocol adapter
    /// renders it as a contentless reasoning event; the reasoning text itself is
    /// not on any answer-facing wire.
    AssistantThinking,
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

/// The streaming-fragment tier: fine-grained content increments. Only the live
/// stream produces these. A protocol renders only the fragments it cares about
/// (opt-in).
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    /// A fragment of assistant text.
    TextDelta { delta: String },
    /// A fragment of model reasoning (best-effort; the completed form is a folded
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
    /// Convenience: wrap a discrete complete event.
    pub fn fact(f: Fact) -> Self {
        AgentEvent::Fact(f)
    }

    /// Convenience: wrap a streaming fragment.
    pub fn delta(d: Delta) -> Self {
        AgentEvent::Delta(d)
    }
}
