use super::tool_exec::ParallelToolExecutor;
use super::AgentLoopError;
use crate::contracts::runtime::plugin::agent::NoOpBehavior;
use crate::contracts::runtime::plugin::composite_agent::CompositeBehavior;
use crate::contracts::runtime::plugin::AgentBehavior;
use crate::contracts::runtime::ToolExecutor;
use crate::contracts::runtime::tool_call::{Tool, ToolDescriptor};
use crate::contracts::RunContext;
use async_trait::async_trait;
use genai::chat::ChatOptions;
use genai::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// Retry strategy for LLM inference calls.
#[derive(Debug, Clone)]
pub struct LlmRetryPolicy {
    /// Max attempts per model candidate (must be >= 1).
    pub max_attempts_per_model: usize,
    /// Initial backoff for retries in milliseconds.
    pub initial_backoff_ms: u64,
    /// Max backoff cap in milliseconds.
    pub max_backoff_ms: u64,
    /// Retry stream startup failures before any output is emitted.
    pub retry_stream_start: bool,
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts_per_model: 2,
            initial_backoff_ms: 250,
            max_backoff_ms: 2_000,
            retry_stream_start: true,
        }
    }
}

/// Input context passed to per-step tool providers.
pub struct StepToolInput<'a> {
    /// Current run context at step boundary.
    pub state: &'a RunContext,
}

/// Tool snapshot resolved for one step.
#[derive(Clone, Default)]
pub struct StepToolSnapshot {
    /// Concrete tool map used for this step.
    pub tools: HashMap<String, Arc<dyn Tool>>,
    /// Tool descriptors exposed to plugins/LLM for this step.
    pub descriptors: Vec<ToolDescriptor>,
}

impl StepToolSnapshot {
    /// Build a step snapshot from a concrete tool map.
    pub fn from_tools(tools: HashMap<String, Arc<dyn Tool>>) -> Self {
        let descriptors = tools
            .values()
            .map(|tool| tool.descriptor().clone())
            .collect();
        Self { tools, descriptors }
    }
}

/// Provider that resolves the tool snapshot for each step.
#[async_trait]
pub trait StepToolProvider: Send + Sync {
    /// Resolve tool map + descriptors for the current step.
    async fn provide(&self, input: StepToolInput<'_>) -> Result<StepToolSnapshot, AgentLoopError>;
}

/// Boxed stream of LLM chat events.
pub type LlmEventStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<genai::chat::ChatStreamEvent, genai::Error>> + Send>,
>;

/// Abstraction over LLM inference backends.
///
/// The agent loop calls this trait for both non-streaming (`exec_chat_response`)
/// and streaming (`exec_chat_stream_events`) inference.  The default
/// implementation ([`GenaiLlmExecutor`]) delegates to `genai::Client`.
#[async_trait]
pub trait LlmExecutor: Send + Sync {
    /// Run a non-streaming chat completion.
    async fn exec_chat_response(
        &self,
        model: &str,
        chat_req: genai::chat::ChatRequest,
        options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse>;

    /// Run a streaming chat completion, returning a boxed event stream.
    async fn exec_chat_stream_events(
        &self,
        model: &str,
        chat_req: genai::chat::ChatRequest,
        options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<LlmEventStream>;

    /// Stable label for logging / debug output.
    fn name(&self) -> &'static str;
}

/// Default LLM executor backed by `genai::Client`.
#[derive(Clone)]
pub struct GenaiLlmExecutor {
    client: Client,
}

impl GenaiLlmExecutor {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl std::fmt::Debug for GenaiLlmExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenaiLlmExecutor").finish()
    }
}

#[async_trait]
impl LlmExecutor for GenaiLlmExecutor {
    async fn exec_chat_response(
        &self,
        model: &str,
        chat_req: genai::chat::ChatRequest,
        options: Option<&ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        self.client.exec_chat(model, chat_req, options).await
    }

    async fn exec_chat_stream_events(
        &self,
        model: &str,
        chat_req: genai::chat::ChatRequest,
        options: Option<&ChatOptions>,
    ) -> genai::Result<LlmEventStream> {
        let resp = self
            .client
            .exec_chat_stream(model, chat_req, options)
            .await?;
        Ok(Box::pin(resp.stream))
    }

    fn name(&self) -> &'static str {
        "genai_client"
    }
}

/// Static provider that always returns the same tool map.
#[derive(Clone, Default)]
pub struct StaticStepToolProvider {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl StaticStepToolProvider {
    pub fn new(tools: HashMap<String, Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl StepToolProvider for StaticStepToolProvider {
    async fn provide(&self, _input: StepToolInput<'_>) -> Result<StepToolSnapshot, AgentLoopError> {
        Ok(StepToolSnapshot::from_tools(self.tools.clone()))
    }
}

// =========================================================================
// Agent — the sole interface the loop sees
// =========================================================================

/// The sole interface the agent loop sees.
///
/// `Agent` encapsulates all runtime configuration, execution strategies,
/// and agent behavior into a single trait. The loop calls methods on this trait
/// to obtain LLM settings, tool execution strategies, and phase-hook behavior.
///
/// # Three-Layer Architecture
///
/// - **Loop**: pure engine — calls `Agent` methods, manages lifecycle
/// - **Agent**: complete agent unit — provides config + behavior
/// - **AgentOS**: assembly — resolves definitions into `Agent` instances
///
/// The default implementation is [`BaseAgent`].
pub trait Agent: Send + Sync {
    // --- Identity ---

    /// Unique identifier for this agent.
    fn id(&self) -> &str;

    // --- LLM Configuration ---

    /// Model identifier (e.g., "gpt-4", "claude-3-opus").
    fn model(&self) -> &str;

    /// System prompt for the LLM.
    fn system_prompt(&self) -> &str;

    /// Loop-budget hint (core loop does not enforce this directly).
    fn max_rounds(&self) -> usize;

    /// Chat options for the LLM.
    fn chat_options(&self) -> Option<&ChatOptions>;

    /// Fallback model ids used when the primary model fails.
    fn fallback_models(&self) -> &[String];

    /// Retry policy for LLM inference failures.
    fn llm_retry_policy(&self) -> &LlmRetryPolicy;

    // --- Execution Strategies ---

    /// Tool execution strategy (parallel, sequential, or custom).
    fn tool_executor(&self) -> Arc<dyn ToolExecutor>;

    /// Optional per-step tool provider.
    ///
    /// When `None`, the loop uses a static provider derived from the tool map.
    fn step_tool_provider(&self) -> Option<Arc<dyn StepToolProvider>> {
        None
    }

    /// Optional LLM executor override.
    ///
    /// When `None`, the loop uses [`GenaiLlmExecutor`] with `Client::default()`.
    fn llm_executor(&self) -> Option<Arc<dyn LlmExecutor>> {
        None
    }

    // --- Behavior ---

    /// The agent behavior (phase hooks) dispatched by the loop.
    fn behavior(&self) -> &dyn AgentBehavior;

}

impl std::fmt::Debug for dyn Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("id", &self.id())
            .field("model", &self.model())
            .field("max_rounds", &self.max_rounds())
            .field("behavior", &self.behavior().id())
            .finish()
    }
}

// =========================================================================
// BaseAgent — the standard Agent implementation
// =========================================================================

/// Standard [`Agent`] implementation.
///
/// Bundles all configuration and behavior for running an agent loop.
/// Constructed by `AgentOS` from an `AgentDefinition`, or directly for tests.
#[derive(Clone)]
pub struct BaseAgent {
    /// Unique identifier for this agent.
    pub id: String,
    /// Model identifier (e.g., "gpt-4", "claude-3-opus").
    pub model: String,
    /// System prompt for the LLM.
    pub system_prompt: String,
    /// Optional loop-budget hint (core loop does not enforce this directly).
    pub max_rounds: usize,
    /// Tool execution strategy (parallel, sequential, or custom).
    pub tool_executor: Arc<dyn ToolExecutor>,
    /// Chat options for the LLM.
    pub chat_options: Option<ChatOptions>,
    /// Fallback model ids used when the primary model fails.
    pub fallback_models: Vec<String>,
    /// Retry policy for LLM inference failures.
    pub llm_retry_policy: LlmRetryPolicy,
    /// Agent behavior (declarative model).
    pub behavior: Arc<dyn AgentBehavior>,
    /// Optional per-step tool provider.
    pub step_tool_provider: Option<Arc<dyn StepToolProvider>>,
    /// Optional LLM executor override.
    pub llm_executor: Option<Arc<dyn LlmExecutor>>,
}

impl Default for BaseAgent {
    fn default() -> Self {
        Self {
            id: "default".to_string(),
            model: "gpt-4o-mini".to_string(),
            system_prompt: String::new(),
            max_rounds: 10,
            tool_executor: Arc::new(ParallelToolExecutor::streaming()),
            chat_options: Some(
                ChatOptions::default()
                    .with_capture_usage(true)
                    .with_capture_reasoning_content(true)
                    .with_capture_tool_calls(true),
            ),
            fallback_models: Vec::new(),
            llm_retry_policy: LlmRetryPolicy::default(),
            behavior: Arc::new(NoOpBehavior),
            step_tool_provider: None,
            llm_executor: None,
        }
    }
}

impl std::fmt::Debug for BaseAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseAgent")
            .field("id", &self.id)
            .field("model", &self.model)
            .field(
                "system_prompt",
                &format!("[{} chars]", self.system_prompt.len()),
            )
            .field("max_rounds", &self.max_rounds)
            .field("tool_executor", &self.tool_executor.name())
            .field("chat_options", &self.chat_options)
            .field("fallback_models", &self.fallback_models)
            .field("llm_retry_policy", &self.llm_retry_policy)
            .field("behavior", &self.behavior.id())
            .field(
                "step_tool_provider",
                &self.step_tool_provider.as_ref().map(|_| "<set>"),
            )
            .field(
                "llm_executor",
                &self
                    .llm_executor
                    .as_ref()
                    .map(|executor| executor.name())
                    .unwrap_or("genai_client(default)"),
            )
            .finish()
    }
}

impl Agent for BaseAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn max_rounds(&self) -> usize {
        self.max_rounds
    }

    fn chat_options(&self) -> Option<&ChatOptions> {
        self.chat_options.as_ref()
    }

    fn fallback_models(&self) -> &[String] {
        &self.fallback_models
    }

    fn llm_retry_policy(&self) -> &LlmRetryPolicy {
        &self.llm_retry_policy
    }

    fn tool_executor(&self) -> Arc<dyn ToolExecutor> {
        self.tool_executor.clone()
    }

    fn step_tool_provider(&self) -> Option<Arc<dyn StepToolProvider>> {
        self.step_tool_provider.clone()
    }

    fn llm_executor(&self) -> Option<Arc<dyn LlmExecutor>> {
        self.llm_executor.clone()
    }

    fn behavior(&self) -> &dyn AgentBehavior {
        self.behavior.as_ref()
    }
}

impl BaseAgent {
    tirea_contract::impl_shared_agent_builder_methods!();

    /// Set tool executor strategy.
    #[must_use]
    pub fn with_tool_executor(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.tool_executor = executor;
        self
    }

    /// Set static tool map (wraps in [`StaticStepToolProvider`]).
    ///
    /// Prefer passing tools directly to [`run_loop`] / [`run_loop_stream`];
    /// use this only when you need to set tools via `step_tool_provider`.
    #[must_use]
    pub fn with_tools(self, tools: HashMap<String, Arc<dyn Tool>>) -> Self {
        self.with_step_tool_provider(Arc::new(StaticStepToolProvider::new(tools)))
    }

    /// Set per-step tool provider.
    #[must_use]
    pub fn with_step_tool_provider(mut self, provider: Arc<dyn StepToolProvider>) -> Self {
        self.step_tool_provider = Some(provider);
        self
    }

    /// Set LLM executor.
    #[must_use]
    pub fn with_llm_executor(mut self, executor: Arc<dyn LlmExecutor>) -> Self {
        self.llm_executor = Some(executor);
        self
    }

    /// Set the agent behavior (declarative model), replacing any existing behavior.
    ///
    /// The loop dispatches all phase hooks exclusively through this behavior.
    #[must_use]
    pub fn with_behavior(mut self, behavior: Arc<dyn AgentBehavior>) -> Self {
        self.behavior = behavior;
        self
    }

    /// Add a behavior, composing with any existing behavior via [`CompositeBehavior`].
    ///
    /// If the current behavior is [`NoOpBehavior`], it is replaced.
    /// Otherwise the current and new behaviors are wrapped in a [`CompositeBehavior`].
    #[must_use]
    pub fn add_behavior(mut self, behavior: Arc<dyn AgentBehavior>) -> Self {
        if self.behavior.id() == "noop" {
            self.behavior = behavior;
        } else {
            let id = format!("{}+{}", self.behavior.id(), behavior.id());
            self.behavior = Arc::new(CompositeBehavior::new(id, vec![self.behavior, behavior]));
        }
        self
    }

    /// Check if any behavior is configured.
    pub fn has_behavior(&self) -> bool {
        self.behavior.id() != "noop"
    }
}
