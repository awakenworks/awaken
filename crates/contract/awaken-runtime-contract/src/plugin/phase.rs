//! Phase-hook vocabulary: the points a hook observes in one model/tool step, the
//! per-phase context (illegal phase/data pairs made unrepresentable), the
//! request-only injection key, and the `PhaseHook` trait itself.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{
    Command as StateCommand, MergePolicy, Scope, StateKey, Store,
};
use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolOutput};

/// The phases a hook can observe in one model/tool step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhaseHookPoint {
    StepStart,
    BeforeInference,
    AfterInference,
    /// After one tool call produced its output (fires once per executed call, and
    /// again when an approved pending call is replayed on resume). The executed
    /// call and its output are carried on [`PhaseContext`] via [`PhaseKind::AfterTool`].
    /// Folds the former separate `ToolOutcomeHook` into the phase-hook model (ADR-0055).
    AfterTool,
    StepEnd,
}

/// The executed tool call and its output, carried by [`PhaseKind::AfterTool`] so a
/// phase hook can react to a tool result (the data the former `ToolOutcomeHook`
/// received directly).
#[derive(Debug, Clone, PartialEq)]
pub struct AfterToolContext {
    pub call: ToolCall,
    pub output: ToolOutput,
}

/// Which phase a hook is being invoked at, carrying exactly the data valid at that
/// phase. Only [`PhaseKind::AfterTool`] carries a call/output, so a StepStart hook
/// cannot be handed a tool result — the illegal combination is unrepresentable
/// (ADR-0055), replacing the former `point` + `Option<after_tool>` pair.
#[derive(Debug, Clone, PartialEq)]
pub enum PhaseKind {
    StepStart,
    BeforeInference,
    AfterInference,
    AfterTool(AfterToolContext),
    StepEnd,
}

impl PhaseKind {
    /// The lightweight subscription discriminant for this phase (the axis a
    /// `CapabilityBound` and `hooks_for` key on).
    #[must_use]
    pub fn point(&self) -> PhaseHookPoint {
        match self {
            PhaseKind::StepStart => PhaseHookPoint::StepStart,
            PhaseKind::BeforeInference => PhaseHookPoint::BeforeInference,
            PhaseKind::AfterInference => PhaseHookPoint::AfterInference,
            PhaseKind::AfterTool(_) => PhaseHookPoint::AfterTool,
            PhaseKind::StepEnd => PhaseHookPoint::StepEnd,
        }
    }
}

/// Context passed to a phase hook. Immutable data; a hook returns state commands
/// rather than mutating anything directly.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseContext {
    pub run_id: RunId,
    pub step: usize,
    pub kind: PhaseKind,
}

/// The run-scoped state a `BeforeInference` hook writes to inject **request-only**
/// context (recalled memories, a compaction summary): the kernel reads it at
/// request assembly and prepends the flattened blocks to that inference, never
/// committing them to the transcript (ADR-0055). Keyed by contributing plugin id
/// and `Commutative`-merged, so several producers coexist and each replays across
/// steps and a resumed run from committed state — request-only-ness is a property
/// of *this key*, not of an overloaded message field (resolving the former
/// dual-meaning `HookReaction.messages`).
pub struct ContextMessages;

impl StateKey for ContextMessages {
    const KEY: &'static str = "context_messages";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Commutative;
    type Value = BTreeMap<String, Vec<Message>>;
}

/// What a hook stages back into the loop: durable state commands plus committed
/// messages. `messages` are **committed** reminder messages an `AfterTool` hook
/// appends to the transcript (so they reach the next inference and replay
/// deterministically). Request-only context is *not* a message here — a
/// `BeforeInference` hook writes it to the [`ContextMessages`] state key, which the
/// kernel reads and prepends to the request.
#[derive(Debug, Default, Clone)]
pub struct HookReaction {
    pub state: Vec<StateCommand>,
    pub messages: Vec<Message>,
}

impl HookReaction {
    /// A reaction that only stages state (the common case).
    pub fn state(state: Vec<StateCommand>) -> Self {
        Self {
            state,
            messages: Vec::new(),
        }
    }

    /// A reaction that only appends committed reminder messages (an `AfterTool`
    /// hook's transcript contribution).
    pub fn messages(messages: Vec<Message>) -> Self {
        Self {
            state: Vec::new(),
            messages,
        }
    }
}

/// A phase hook: behavior contributed by a plugin at one phase point. Async so a
/// real hook can consult an external system (e.g. run a selection sub-agent). It
/// stages state commands; a `BeforeInference` hook injects request-only context
/// (e.g. recalled memories) by writing the [`ContextMessages`] state key, which
/// the kernel reads and prepends. `conversation` is the transcript at this point,
/// so a `BeforeInference` hook can select context relevant to the user's message.
/// `state` is the run's read-only materialized state, so a hook whose work is
/// once-per-run (recall, compaction) gates on its own run-scoped state key and
/// replays across steps and resume instead of caching by `run_id` (ADR-0055).
#[async_trait]
pub trait PhaseHook: Send + Sync {
    fn point(&self) -> PhaseHookPoint;
    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        conversation: &[Message],
        state: &Store,
    ) -> HookReaction;
}
