use crate::runtime::inference::{InferenceContext, LLMResponse, MessagingContext};
use crate::runtime::run::RunIdentity;
use crate::runtime::run::{FlowControl, RunAction};
use crate::runtime::state::{AnyStateAction, SerializedStateAction};
use crate::runtime::tool_call::gate::ToolGate;
use crate::runtime::tool_call::{ToolCallContext, ToolDescriptor, ToolResult};
use crate::thread::Message;
use crate::RunPolicy;
use serde_json::Value;
use std::sync::Arc;
use tirea_state::{State, TireaResult, TrackedPatch};

use super::types::{StepOutcome, ToolCallAction};

/// Step context — mutable state passed through all phases.
///
/// This is the primary interface for the runtime to maintain per-step state.
/// Unlike the old `Extensions`-based design, all phase-relevant data lives
/// in explicit typed fields, making every read and write site visible at the
/// type level.
///
/// The loop sets `gate` before each tool phase and `llm_response` after each
/// inference call. Plugins do not write to `StepContext` directly; they return
/// typed [`ActionSet`](super::action_set::ActionSet) values that the loop
/// applies via `match`.
pub struct StepContext<'a> {
    // === Execution Context ===
    ctx: ToolCallContext<'a>,

    // === Identity ===
    thread_id: &'a str,

    // === Thread Messages ===
    messages: &'a [Arc<Message>],

    /// Number of messages that existed before the current run started.
    initial_message_count: usize,

    // === Step scope: inference ===
    /// Tools and prompt context for the current inference call.
    /// Persists across reset (tools are carried over).
    pub inference: InferenceContext,

    // === Step scope: LLM response ===
    /// Set by the loop after each inference call. `None` before inference.
    pub llm_response: Option<LLMResponse>,

    // === ToolCall scope ===
    /// Set by the loop before `BeforeToolExecute`; `None` outside tool phases.
    pub gate: Option<ToolGate>,

    // === AfterToolExecute accumulation ===
    /// Reminders and user messages produced during tool execution.
    pub messaging: MessagingContext,

    // === Run scope: flow control ===
    /// Set by plugins to request run termination.
    pub flow: FlowControl,

    // === Pending state changes ===
    /// State actions accumulated during a phase; reduced to patches by the loop.
    pub pending_state_actions: Vec<AnyStateAction>,

    // === Pending patches (output) ===
    /// Reduced patches ready for the thread store.
    pub pending_patches: Vec<TrackedPatch>,

    // === Pending serialized actions (intent log) ===
    /// Serialized actions captured during this step for persistence.
    pub(crate) pending_serialized_state_actions: Vec<SerializedStateAction>,
}

impl<'a> StepContext<'a> {
    /// Create a new step context.
    pub fn new(
        ctx: ToolCallContext<'a>,
        thread_id: &'a str,
        messages: &'a [Arc<Message>],
        tools: Vec<ToolDescriptor>,
    ) -> Self {
        Self {
            ctx,
            thread_id,
            messages,
            initial_message_count: 0,
            inference: InferenceContext {
                tools,
                ..Default::default()
            },
            llm_response: None,
            gate: None,
            messaging: MessagingContext::default(),
            flow: FlowControl::default(),
            pending_state_actions: Vec::new(),
            pending_patches: Vec::new(),
            pending_serialized_state_actions: Vec::new(),
        }
    }

    // =========================================================================
    // Execution context access
    // =========================================================================

    pub fn ctx(&self) -> &ToolCallContext<'a> {
        &self.ctx
    }

    pub fn thread_id(&self) -> &str {
        self.thread_id
    }

    pub fn messages(&self) -> &[Arc<Message>] {
        self.messages
    }

    /// Number of messages that existed before the current run started.
    pub fn initial_message_count(&self) -> usize {
        self.initial_message_count
    }

    /// Set the initial message count (called by the loop after construction).
    pub fn set_initial_message_count(&mut self, count: usize) {
        self.initial_message_count = count;
    }

    pub fn state_of<T: State>(&self) -> T::Ref<'_> {
        self.ctx.state_of::<T>()
    }

    pub fn state<T: State>(&self, path: &str) -> T::Ref<'_> {
        self.ctx.state::<T>(path)
    }

    pub fn run_policy(&self) -> &RunPolicy {
        self.ctx.run_policy()
    }

    pub fn run_identity(&self) -> &RunIdentity {
        self.ctx.run_identity()
    }

    pub fn snapshot(&self) -> Value {
        self.ctx.snapshot()
    }

    pub fn snapshot_of<T: State>(&self) -> TireaResult<T> {
        self.ctx.snapshot_of::<T>()
    }

    pub fn snapshot_at<T: State>(&self, path: &str) -> TireaResult<T> {
        self.ctx.snapshot_at::<T>(path)
    }

    /// Reset step-specific state for a new step.
    ///
    /// Preserves `inference.tools` across resets.
    pub fn reset(&mut self) {
        let tools = std::mem::take(&mut self.inference.tools);
        self.inference = InferenceContext {
            tools,
            ..Default::default()
        };
        self.llm_response = None;
        self.gate = None;
        self.messaging = MessagingContext::default();
        self.flow = FlowControl::default();
        self.pending_state_actions.clear();
        self.pending_patches.clear();
        self.pending_serialized_state_actions.clear();
    }

    // =========================================================================
    // Tool gate read-only accessors
    // =========================================================================

    pub fn tool_name(&self) -> Option<&str> {
        self.gate.as_ref().map(|g| g.name.as_str())
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.gate.as_ref().map(|g| g.id.as_str())
    }

    pub fn tool_idempotency_key(&self) -> Option<&str> {
        self.tool_call_id()
    }

    pub fn tool_args(&self) -> Option<&Value> {
        self.gate.as_ref().map(|g| &g.args)
    }

    pub fn tool_result(&self) -> Option<&ToolResult> {
        self.gate.as_ref().and_then(|g| g.result.as_ref())
    }

    pub fn tool_blocked(&self) -> bool {
        self.gate.as_ref().map(|g| g.blocked).unwrap_or(false)
    }

    pub fn tool_pending(&self) -> bool {
        self.gate.as_ref().map(|g| g.pending).unwrap_or(false)
    }

    // =========================================================================
    // State output
    // =========================================================================

    /// Push a reduced patch to the output queue.
    pub fn emit_patch(&mut self, patch: TrackedPatch) {
        self.pending_patches.push(patch);
    }

    /// Push a state action for deferred reduction.
    pub fn emit_state_action(&mut self, action: AnyStateAction) {
        self.pending_state_actions.push(action);
    }

    /// Push a serialized action for intent-log persistence.
    pub fn emit_serialized_state_action(&mut self, action: SerializedStateAction) {
        self.pending_serialized_state_actions.push(action);
    }

    /// Drain and return all accumulated serialized actions.
    pub fn take_pending_serialized_state_actions(&mut self) -> Vec<SerializedStateAction> {
        std::mem::take(&mut self.pending_serialized_state_actions)
    }

    // =========================================================================
    // Flow control read
    // =========================================================================

    /// Effective run-level action for current step.
    pub fn run_action(&self) -> RunAction {
        self.flow.run_action.clone().unwrap_or(RunAction::Continue)
    }

    /// Current tool action derived from gate state.
    pub fn tool_action(&self) -> ToolCallAction {
        if let Some(gate) = &self.gate {
            if gate.blocked {
                return ToolCallAction::Block {
                    reason: gate.block_reason.clone().unwrap_or_default(),
                };
            }
            if gate.pending {
                if let Some(ticket) = gate.suspend_ticket.as_ref() {
                    return ToolCallAction::suspend(ticket.clone());
                }
                return ToolCallAction::Block {
                    reason: "invalid pending tool state: missing suspend ticket".to_string(),
                };
            }
        }
        ToolCallAction::Proceed
    }

    // =========================================================================
    // Step Outcome
    // =========================================================================

    /// Get the step outcome based on current state.
    pub fn result(&self) -> StepOutcome {
        if let Some(gate) = &self.gate {
            if gate.pending {
                if let Some(ticket) = gate.suspend_ticket.as_ref() {
                    return StepOutcome::Pending(Box::new(ticket.clone()));
                }
                return StepOutcome::Continue;
            }
        }

        if let Some(llm) = &self.llm_response {
            if let Ok(result) = &llm.outcome {
                if result.tool_calls.is_empty() && !result.text.is_empty() {
                    return StepOutcome::Complete;
                }
            }
        }

        StepOutcome::Continue
    }
}
