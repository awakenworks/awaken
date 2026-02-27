//! Shared agent contracts for conversation state, runtime protocol, extension SPI, and storage.
#![allow(missing_docs)]

/// Builder methods for pure-data fields shared by both BaseAgent and AgentDefinition.
///
/// Covers: id, model, system_prompt, max_rounds, chat_options, fallback_models,
/// llm_retry_policy.
#[macro_export]
macro_rules! impl_shared_agent_builder_methods {
    () => {
        /// Create a new instance with the given model id.
        pub fn new(model: impl Into<String>) -> Self {
            Self {
                model: model.into(),
                ..Default::default()
            }
        }

        /// Create a new instance with explicit id and model.
        pub fn with_id(id: impl Into<String>, model: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                model: model.into(),
                ..Default::default()
            }
        }

        /// Set system prompt.
        #[must_use]
        pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
            self.system_prompt = prompt.into();
            self
        }

        /// Set max rounds.
        #[must_use]
        pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
            self.max_rounds = max_rounds;
            self
        }

        /// Set chat options.
        #[must_use]
        pub fn with_chat_options(mut self, options: ChatOptions) -> Self {
            self.chat_options = Some(options);
            self
        }

        /// Set fallback model ids to try after the primary model.
        #[must_use]
        pub fn with_fallback_models(mut self, models: Vec<String>) -> Self {
            self.fallback_models = models;
            self
        }

        /// Add a single fallback model id.
        #[must_use]
        pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
            self.fallback_models.push(model.into());
            self
        }

        /// Set LLM retry policy.
        #[must_use]
        pub fn with_llm_retry_policy(mut self, policy: LlmRetryPolicy) -> Self {
            self.llm_retry_policy = policy;
            self
        }
    };
}

/// Builder methods for runtime instance fields (plugins).
///
/// Only used by BaseAgent (loop layer) which directly holds plugin and
/// runtime instances. AgentDefinition uses id-based references instead.
#[macro_export]
macro_rules! impl_loop_config_builder_methods {
    () => {
        /// Set legacy plugins.
        #[must_use]
        pub fn with_plugins(mut self, plugins: Vec<Arc<dyn AgentPlugin>>) -> Self {
            self.plugins = plugins;
            self
        }

        /// Add a single legacy plugin.
        #[must_use]
        pub fn with_plugin(mut self, plugin: Arc<dyn AgentPlugin>) -> Self {
            self.plugins.push(plugin);
            self
        }
    };
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub mod io;
pub mod runtime;
pub mod storage;
pub mod thread;
pub mod transport;

/// Per-run configuration — a business alias for the generic `SealedState` container.
pub type RunConfig = tirea_state::SealedState;

/// Error type for `RunConfig` operations.
pub type RunConfigError = tirea_state::SealedStateError;

// thread
pub use thread::{
    gen_message_id, CheckpointReason, Message, MessageMetadata, Role, Thread, ThreadChangeSet,
    ThreadMetadata, ToolCall, Version, Visibility,
};

// io
pub use io::{AgentEvent, ResumeDecisionAction, RunRequest, RuntimeInput, RuntimeOutput, ToolCallDecision};

// runtime plugin/tool-call/lifecycle
pub use runtime::{
    ActivityContext, ActivityManager, AfterInferenceContext, AfterToolExecuteContext, AgentBehavior,
    AgentPlugin, AnyStateAction, BeforeInferenceContext, BeforeToolExecuteContext,
    CompositeBehavior, DecisionReplayPolicy, NoOpBehavior, Phase, PhaseEffect, PhaseOutput,
    PhasePolicy, PluginPhaseContext, ReadOnlyContext, RunAction, RunContext, RunDelta,
    RunEndContext, RunStartContext, StateEffect, StateSpec, StepContext, StepEndContext,
    StepOutcome, StepStartContext, StoppedReason, StreamResult, SuspendTicket, Suspension,
    SuspensionResponse, TerminationReason, TokenUsage, ToolCallAction, ToolCallContext,
    ToolCallOutcome, ToolContext, ToolExecution, ToolExecutionRequest, ToolExecutionResult,
    ToolExecutor, ToolExecutorError, ToolProgressState, validate_effect,
    TOOL_PROGRESS_ACTIVITY_TYPE,
};

// storage
pub use storage::{
    paginate_in_memory, Committed, MessagePage, MessageQuery, MessageWithCursor, SortOrder,
    ThreadHead, ThreadListPage, ThreadListQuery, ThreadReader, ThreadStore, ThreadStoreError,
    ThreadSync, ThreadWriter, VersionPrecondition,
};

// transport
pub use transport::{Identity, Transcoder};
