use crate::runtime::inference::{InferenceError, StreamResult};
use crate::runtime::run::RunIdentity;
use crate::runtime::run::{RunAction, TerminationReason};
use crate::runtime::tool_call::gate::{SuspendTicket, ToolCallAction};
use crate::runtime::tool_call::{ToolCallResume, ToolResult};
use crate::thread::Message;
use crate::RunPolicy;
use serde_json::Value;
use std::sync::Arc;
use tirea_state::State;

use super::step::StepContext;
use super::types::Phase;

/// Shared read access available to all phase contexts.
pub trait PhaseContext {
    fn phase(&self) -> Phase;
    fn thread_id(&self) -> &str;
    fn messages(&self) -> &[Arc<Message>];
    fn run_policy(&self) -> &RunPolicy;
    fn run_identity(&self) -> &RunIdentity;
    fn state_of<T: State>(&self) -> T::Ref<'_>;
    fn snapshot(&self) -> serde_json::Value;
}

macro_rules! impl_phase_context {
    ($name:ident, $phase:expr) => {
        impl<'s, 'a> $name<'s, 'a> {
            pub fn new(step: &'s mut StepContext<'a>) -> Self {
                Self { step }
            }

            #[cfg(feature = "test-support")]
            pub fn step_mut_for_tests(&mut self) -> &mut StepContext<'a> {
                self.step
            }
        }

        impl<'s, 'a> PhaseContext for $name<'s, 'a> {
            fn phase(&self) -> Phase {
                $phase
            }

            fn thread_id(&self) -> &str {
                self.step.thread_id()
            }

            fn messages(&self) -> &[Arc<Message>] {
                self.step.messages()
            }

            fn run_policy(&self) -> &RunPolicy {
                self.step.run_policy()
            }

            fn run_identity(&self) -> &RunIdentity {
                self.step.run_identity()
            }

            fn state_of<T: State>(&self) -> T::Ref<'_> {
                self.step.state_of::<T>()
            }

            fn snapshot(&self) -> serde_json::Value {
                self.step.snapshot()
            }
        }
    };
}

pub struct RunStartContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(RunStartContext, Phase::RunStart);

pub struct StepStartContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(StepStartContext, Phase::StepStart);

pub struct BeforeInferenceContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(BeforeInferenceContext, Phase::BeforeInference);

impl<'s, 'a> BeforeInferenceContext<'s, 'a> {
    /// Append a context message with the given key.
    pub fn add_context_message(&mut self, key: impl Into<String>, content: impl Into<String>) {
        self.step
            .inference
            .context_messages
            .push(crate::runtime::inference::ContextMessage {
                key: key.into(),
                role: crate::thread::Role::System,
                content: content.into(),
                visibility: crate::thread::Visibility::Internal,
                cooldown_turns: 0,
                target: Default::default(),
                consume_after_emit: false,
            });
    }

    /// Append a session message.
    pub fn add_session_message(&mut self, text: impl Into<String>) {
        self.step.inference.context_messages.push(
            crate::runtime::inference::ContextMessage::session_text(text.into()),
        );
    }

    /// Exclude tool by id.
    pub fn exclude_tool(&mut self, tool_id: &str) {
        self.step.inference.tools.retain(|t| t.id != tool_id);
    }

    /// Keep only listed tools.
    pub fn include_only(&mut self, tool_ids: &[&str]) {
        self.step
            .inference
            .tools
            .retain(|t| tool_ids.contains(&t.id.as_str()));
    }

    /// Terminate current run as behavior-requested before inference.
    pub fn terminate_behavior_requested(&mut self) {
        self.step.flow.run_action =
            Some(RunAction::Terminate(TerminationReason::BehaviorRequested));
    }

    /// Request run termination with a specific reason.
    pub fn request_termination(&mut self, reason: TerminationReason) {
        self.step.flow.run_action = Some(RunAction::Terminate(reason));
    }
}

pub struct AfterInferenceContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(AfterInferenceContext, Phase::AfterInference);

impl<'s, 'a> AfterInferenceContext<'s, 'a> {
    pub fn response_opt(&self) -> Option<&StreamResult> {
        self.step
            .llm_response
            .as_ref()
            .and_then(|r| r.outcome.as_ref().ok())
    }

    pub fn response(&self) -> &StreamResult {
        self.step
            .llm_response
            .as_ref()
            .expect("AfterInferenceContext.response() requires LLMResponse to be set")
            .outcome
            .as_ref()
            .expect("AfterInferenceContext.response() requires a successful outcome")
    }

    pub fn inference_error(&self) -> Option<&InferenceError> {
        self.step
            .llm_response
            .as_ref()
            .and_then(|r| r.outcome.as_ref().err())
    }

    /// Request run termination with a specific reason after inference has completed.
    pub fn request_termination(&mut self, reason: TerminationReason) {
        self.step.flow.run_action = Some(RunAction::Terminate(reason));
    }
}

pub struct BeforeToolExecuteContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(BeforeToolExecuteContext, Phase::BeforeToolExecute);

impl<'s, 'a> BeforeToolExecuteContext<'s, 'a> {
    pub fn tool_name(&self) -> Option<&str> {
        self.step.tool_name()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.step.tool_call_id()
    }

    pub fn tool_args(&self) -> Option<&Value> {
        self.step.tool_args()
    }

    /// Resume payload attached to current tool call, if present.
    pub fn resume_input(&self) -> Option<ToolCallResume> {
        let gate = self.step.gate.as_ref()?;
        self.step.ctx().resume_input_for(&gate.id).ok().flatten()
    }

    pub fn decision(&self) -> ToolCallAction {
        self.step.tool_action()
    }

    pub fn set_decision(&mut self, decision: ToolCallAction) {
        if let Some(gate) = self.step.gate.as_mut() {
            match decision {
                ToolCallAction::Proceed => {
                    gate.blocked = false;
                    gate.block_reason = None;
                    gate.pending = false;
                    gate.suspend_ticket = None;
                }
                ToolCallAction::Suspend(ticket) => {
                    gate.blocked = false;
                    gate.block_reason = None;
                    gate.pending = true;
                    gate.suspend_ticket = Some(*ticket);
                }
                ToolCallAction::Block { reason } => {
                    gate.blocked = true;
                    gate.block_reason = Some(reason);
                    gate.pending = false;
                    gate.suspend_ticket = None;
                }
            }
        }
    }

    pub fn block(&mut self, reason: impl Into<String>) {
        if let Some(gate) = self.step.gate.as_mut() {
            gate.blocked = true;
            gate.block_reason = Some(reason.into());
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }

    /// Explicitly allow tool execution.
    ///
    /// This clears any previous block/suspend state set by earlier plugins.
    pub fn allow(&mut self) {
        if let Some(gate) = self.step.gate.as_mut() {
            gate.blocked = false;
            gate.block_reason = None;
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }

    /// Override current call result directly from plugin logic.
    ///
    /// Useful for resumed frontend interactions where the external payload
    /// should become the tool result without executing a backend tool.
    pub fn set_tool_result(&mut self, result: ToolResult) {
        if let Some(gate) = self.step.gate.as_mut() {
            gate.result = Some(result);
        }
    }

    pub fn suspend(&mut self, ticket: SuspendTicket) {
        if let Some(gate) = self.step.gate.as_mut() {
            gate.blocked = false;
            gate.block_reason = None;
            gate.pending = true;
            gate.suspend_ticket = Some(ticket);
        }
    }
}

pub struct AfterToolExecuteContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(AfterToolExecuteContext, Phase::AfterToolExecute);

impl<'s, 'a> AfterToolExecuteContext<'s, 'a> {
    pub fn tool_name(&self) -> Option<&str> {
        self.step.tool_name()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.step.tool_call_id()
    }

    pub fn tool_result(&self) -> &ToolResult {
        self.step
            .gate
            .as_ref()
            .and_then(|g| g.result.as_ref())
            .expect("AfterToolExecuteContext.tool_result() requires tool result")
    }

    pub fn add_system_reminder(&mut self, text: impl Into<String>) {
        self.step.messaging.add_system_reminder(text);
    }

    pub fn add_user_message(&mut self, text: impl Into<String>) {
        self.step.messaging.add_user_message(text);
    }

    pub fn add_message(&mut self, message: crate::runtime::inference::ContextMessage) {
        self.step.messaging.push(message);
    }
}

pub struct StepEndContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(StepEndContext, Phase::StepEnd);

pub struct RunEndContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(RunEndContext, Phase::RunEnd);
