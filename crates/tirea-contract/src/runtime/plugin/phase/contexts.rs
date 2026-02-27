use super::{Phase, RunAction, StepContext, SuspendTicket, ToolCallAction};
use crate::runtime::run::TerminationReason;
use crate::runtime::llm::StreamResult;
use crate::runtime::ToolCallResume;
use crate::thread::Message;
use crate::runtime::tool_call::ToolResult;
use crate::RunConfig;
use serde_json::Value;
use std::sync::Arc;
use tirea_state::State;

/// Shared read access available to all phase contexts.
pub trait PhaseContext {
    fn phase(&self) -> Phase;
    fn thread_id(&self) -> &str;
    fn messages(&self) -> &[Arc<Message>];
    fn run_config(&self) -> &RunConfig;
    fn config_value(&self, key: &str) -> Option<&Value> {
        self.run_config().value(key)
    }
    fn state_of<T: State>(&self) -> T::Ref<'_>;
    fn snapshot(&self) -> Value;
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

            fn run_config(&self) -> &RunConfig {
                self.step.run_config()
            }

            fn state_of<T: State>(&self) -> T::Ref<'_> {
                self.step.state_of::<T>()
            }

            fn snapshot(&self) -> Value {
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
    /// Append a system context line.
    pub fn add_system_context(&mut self, text: impl Into<String>) {
        self.step.system(text);
    }

    /// Append a session message.
    pub fn add_session_message(&mut self, text: impl Into<String>) {
        self.step.thread(text);
    }

    /// Exclude tool by id.
    pub fn exclude_tool(&mut self, tool_id: &str) {
        self.step.exclude(tool_id);
    }

    /// Keep only listed tools.
    pub fn include_only(&mut self, tool_ids: &[&str]) {
        self.step.include_only(tool_ids);
    }

    /// Terminate current run as behavior-requested before inference.
    pub fn terminate_behavior_requested(&mut self) {
        self.step.set_run_action(RunAction::Terminate(
            TerminationReason::BehaviorRequested,
        ));
    }

    /// Request run termination with a specific reason.
    pub fn request_termination(&mut self, reason: TerminationReason) {
        self.step
            .set_run_action(RunAction::Terminate(reason));
    }
}

pub struct AfterInferenceContext<'s, 'a> {
    step: &'s mut StepContext<'a>,
}
impl_phase_context!(AfterInferenceContext, Phase::AfterInference);

impl<'s, 'a> AfterInferenceContext<'s, 'a> {
    pub fn response_opt(&self) -> Option<&StreamResult> {
        self.step.response.as_ref()
    }

    pub fn response(&self) -> &StreamResult {
        self.step
            .response
            .as_ref()
            .expect("AfterInferenceContext.response() requires response to be set")
    }

    /// Request run termination with a specific reason after inference has completed.
    pub fn request_termination(&mut self, reason: TerminationReason) {
        self.step
            .set_run_action(RunAction::Terminate(reason));
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
        let call_id = self.tool_call_id()?;
        self.step.ctx().resume_input_for(call_id).ok().flatten()
    }

    pub fn decision(&self) -> ToolCallAction {
        self.step.tool_action()
    }

    pub fn set_decision(&mut self, decision: ToolCallAction) {
        match decision {
            ToolCallAction::Proceed => self.step.allow(),
            ToolCallAction::Suspend(ticket) => self.step.suspend(*ticket),
            ToolCallAction::Block { reason } => self.step.block(reason),
        }
    }

    pub fn block(&mut self, reason: impl Into<String>) {
        self.step.block(reason);
    }

    /// Explicitly allow tool execution.
    ///
    /// This clears any previous block/suspend state set by earlier plugins.
    pub fn allow(&mut self) {
        self.step.allow();
    }

    /// Override current call result directly from plugin logic.
    ///
    /// Useful for resumed frontend interactions where the external payload
    /// should become the tool result without executing a backend tool.
    pub fn set_tool_result(&mut self, result: ToolResult) {
        self.step.set_tool_result(result);
    }

    pub fn suspend(&mut self, ticket: SuspendTicket) {
        self.step.suspend(ticket);
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
            .tool_result()
            .expect("AfterToolExecuteContext.tool_result() requires tool result")
    }

    pub fn add_system_reminder(&mut self, text: impl Into<String>) {
        self.step.reminder(text);
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
