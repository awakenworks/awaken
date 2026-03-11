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

/// Declares the [`StateSpec`] types managed by a plugin behavior.
///
/// Generates `register_lattice_paths` and `register_state_scopes`
/// implementations for [`AgentBehavior`], so plugin authors only need to
/// list their state types once instead of implementing two methods.
///
/// # Example
///
/// ```ignore
/// #[async_trait]
/// impl AgentBehavior for MyPlugin {
///     fn id(&self) -> &str { "my_plugin" }
///     tirea_contract::declare_plugin_states!(MyState, MyOtherState);
/// }
/// ```
#[macro_export]
macro_rules! declare_plugin_states {
    ($($state:ty),+ $(,)?) => {
        fn register_lattice_paths(&self, registry: &mut ::tirea_state::LatticeRegistry) {
            $(<$state as ::tirea_state::State>::register_lattice(registry);)+
        }

        fn register_state_scopes(
            &self,
            registry: &mut $crate::runtime::state::StateScopeRegistry,
        ) {
            $(registry.register::<$state>(<$state as ::tirea_state::StateSpec>::SCOPE);)+
        }

        fn register_state_action_deserializers(
            &self,
            registry: &mut $crate::runtime::state::StateActionDeserializerRegistry,
        ) {
            $(registry.register::<$state>();)+
        }
    };
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub mod io;
pub mod runtime;
pub mod scope;
pub mod storage;
pub mod thread;
pub mod transport;

/// Per-run scope and execution policy.
pub type RunPolicy = runtime::RunPolicy;

// thread
pub use thread::{
    gen_message_id, CheckpointReason, Message, MessageMetadata, Role, RunMeta, Thread,
    ThreadChangeSet, ThreadMetadata, ToolCall, Version, Visibility,
};

// io
pub use io::{
    AgentEvent, ResumeDecisionAction, RunRequest, RuntimeInput, RuntimeOutput, ToolCallDecision,
};

// runtime plugin/tool-call/lifecycle
pub use runtime::{
    build_read_only_context_from_step, reduce_state_actions, Action, ActivityContext,
    ActivityManager, AfterInferenceContext, AfterToolExecuteContext, AgentBehavior, AnyStateAction,
    BeforeInferenceContext, BeforeToolExecuteContext, DecisionReplayPolicy, Extensions,
    NoOpBehavior, Phase, PhaseContext, PhasePolicy, ReadOnlyContext, RunAction, RunContext,
    RunDelta, RunEndContext, RunStartContext, ScopeContext, SerializedStateAction,
    StateActionDecodeError, StateActionDeserializerRegistry, StateScope, StateScopeRegistry,
    StateSpec, StepContext, StepEndContext, StepOutcome, StepStartContext, StoppedReason,
    StreamResult, SuspendTicket, Suspension, SuspensionResponse, TerminationReason, TokenUsage,
    ToolCallAction, ToolCallContext, ToolCallOutcome, ToolCallProgressSink, ToolCallProgressState,
    ToolCallProgressStatus, ToolCallProgressUpdate, ToolExecution, ToolExecutionEffect,
    ToolExecutionRequest, ToolExecutionResult, ToolExecutor, ToolExecutorError, ToolGate,
    ToolProgressState, TOOL_CALL_PROGRESS_ACTIVITY_TYPE, TOOL_CALL_PROGRESS_SCHEMA,
    TOOL_CALL_PROGRESS_TYPE, TOOL_PROGRESS_ACTIVITY_TYPE, TOOL_PROGRESS_ACTIVITY_TYPE_LEGACY,
};

// storage
pub use storage::{
    paginate_in_memory, paginate_mailbox_entries, paginate_runs_in_memory, Committed, MailboxEntry,
    MailboxEntryStatus, MailboxPage, MailboxQuery, MailboxReader, MailboxStore, MailboxStoreError,
    MailboxInterrupt, MailboxReceiver, MailboxState, MailboxWriter, MessagePage, MessageQuery,
    MessageWithCursor, ReceiveOutcome, RunOrigin, RunPage, RunQuery, RunReader, RunRecord,
    RunStore, RunStoreError, RunWriter, SortOrder, ThreadHead, ThreadListPage, ThreadListQuery,
    ThreadReader, ThreadStore,
    ThreadStoreError, ThreadSync, ThreadWriter, VersionPrecondition,
};

// transport
pub use transport::{Identity, Transcoder};
