//! Tool execution ports.
//!
//! Two concerns stay separate (tool-and-capability.md): a tool *implementation*
//! (`Tool` typed, `RawTool` schema-erased) and the *executor* port the loop
//! calls to run one resolved call. Where a call physically runs is an
//! implementation detail of whoever implements `ToolExecutor` — owned by the
//! orchestration layer above — and stays out of the neutral runtime contract.

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{Command as StateCommand, Store as StateStore};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroU16;
use std::sync::Arc;
use thiserror::Error;

pub use crate::llm::ToolCall;

tokio::task_local! {
    /// Runtime-owned context of the current durable tool invocation.
    ///
    /// A provider `ToolCall::call_id` only correlates model tool-use/result blocks and
    /// may be synthesized with response-local scope. Business tools that need an
    /// idempotency key must use this run/step-scoped identity instead.
    static TOOL_OPERATION_CONTEXT: ToolOperationContext;
    /// Immutable materialized State at executor entry. Stateful plugin tools
    /// read this snapshot and return commands; they never receive a mutable
    /// store or persistence handle.
    static TOOL_STATE_CONTEXT: Arc<StateStore>;
    /// Trusted lookup for a wrapper that delegates to another ordinary tool.
    /// Runtime owns the catalog; model-authored ids can only query it.
    static TOOL_EXECUTION_FACTS: Arc<dyn ToolExecutionFactsResolver>;
}

/// Stable runtime coordinates for one tool invocation.
///
/// This is execution context rather than tool input: providers and models cannot
/// author the Run/Thread coordinates or durable operation identity. The model
/// call id is carried only for protocol correlation; it must not replace the
/// Runtime-owned operation id at durable-effect boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOperationContext {
    /// Absent only for direct adapter/unit invocations outside the runtime. A
    /// capability broker must fail closed when it requires run-bound authority.
    pub run_id: Option<RunId>,
    /// Logical Thread containing the current Run. Absent only for direct
    /// adapter/unit invocations outside the Runtime execution loop.
    pub thread_id: Option<ThreadId>,
    pub operation_id: String,
    /// Provider/model tool-use correlation id. This may be synthesized or have
    /// response-local scope and is therefore not a durable idempotency key.
    pub call_id: Option<String>,
    /// Trusted Workspace ownership inherited from the current attempt. This is
    /// absent for legacy/direct unit invocations and is never model input.
    pub execution_scope: Option<awaken_tenancy::ExecutionScopeRef>,
}

/// Canonical, non-model-authored identity of one tool effect.
///
/// Unlike [`ToolCall::call_id`], this token is stable across Hand connection and
/// process replacement. Its fields are private so adapters cannot accidentally
/// rebuild durable identity from response-local protocol coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOperationToken {
    run_id: Option<RunId>,
    operation_id: String,
    execution_scope: Option<awaken_tenancy::ExecutionScopeRef>,
}

impl ToolOperationToken {
    /// Construct a token only from Runtime-owned execution context. Empty
    /// operation identities fail closed instead of falling back to a model call
    /// id at a durable-effect boundary.
    #[must_use]
    pub fn from_context(context: &ToolOperationContext) -> Option<Self> {
        (!context.operation_id.trim().is_empty()).then(|| Self {
            run_id: context.run_id.clone(),
            operation_id: context.operation_id.clone(),
            execution_scope: context.execution_scope.clone(),
        })
    }

    /// Derive the bounded ledger identity for one infrastructure scope. The
    /// domain label and complete tuple are fingerprinted so delimiters inside an
    /// opaque Workspace, Session, or operation id cannot create aliases.
    #[must_use]
    pub fn ledger_id(&self, infrastructure_scope: Option<&str>) -> String {
        let execution_scope = self
            .execution_scope
            .as_ref()
            .map(|scope| scope.0.0.as_str());
        let run_id = self.run_id.as_ref().map(|run_id| run_id.0.as_str());
        let fingerprint = crate::resolution::content_fingerprint(&(
            "tool-operation-v2",
            execution_scope,
            infrastructure_scope,
            run_id,
            self.operation_id.as_str(),
        ))
        .expect("tool operation identity components always serialize");
        format!("tool-op-v2:{fingerprint}")
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    #[must_use]
    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    #[must_use]
    pub fn execution_scope(&self) -> Option<&awaken_tenancy::ExecutionScopeRef> {
        self.execution_scope.as_ref()
    }
}

impl ToolOperationContext {
    /// Construct the durable coordinates for one runtime-owned tool operation
    /// without exposing the agent-contract Run id type to extension crates.
    pub fn for_run(run_id: impl Into<String>, operation_id: impl Into<String>) -> Self {
        Self {
            run_id: Some(RunId(run_id.into())),
            thread_id: None,
            operation_id: operation_id.into(),
            call_id: None,
            execution_scope: None,
        }
    }
}

/// Return the runtime-owned context of the tool invocation currently entering an
/// executor. Direct unit invocations have no runtime scope.
#[must_use]
pub fn current_tool_operation_context() -> Option<ToolOperationContext> {
    TOOL_OPERATION_CONTEXT.try_with(Clone::clone).ok()
}

/// Return the canonical effect token for the current Runtime-owned tool
/// invocation. Direct adapter calls and malformed empty contexts return `None`.
#[must_use]
pub fn current_tool_operation_token() -> Option<ToolOperationToken> {
    current_tool_operation_context()
        .as_ref()
        .and_then(ToolOperationToken::from_context)
}

/// Return the runtime-scoped identity of the tool invocation currently entering an
/// executor. Direct unit invocations have no runtime scope and therefore return
/// `None`; tools may use their call id as a test/legacy fallback in that case.
#[must_use]
pub fn current_tool_operation_id() -> Option<String> {
    current_tool_operation_context().map(|context| context.operation_id)
}

/// Read the immutable State snapshot captured at this tool invocation. Direct
/// adapter/unit calls have no Runtime State and therefore return `None`.
#[must_use]
pub fn current_tool_state() -> Option<Arc<StateStore>> {
    TOOL_STATE_CONTEXT.try_with(Clone::clone).ok()
}

/// Trusted policy resolved for one concrete ordinary tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionFacts {
    pub recovery: ToolRecoveryPolicy,
    pub concurrency: ToolConcurrency,
}

/// Read-only execution catalog available to compositional tools.
pub trait ToolExecutionFactsResolver: Send + Sync {
    fn resolve(&self, call: &ToolCall) -> Result<ToolExecutionFacts, ToolError>;
}

/// Resolve another ordinary tool through Runtime's current immutable catalog.
pub fn current_tool_execution_facts(call: &ToolCall) -> Result<ToolExecutionFacts, ToolError> {
    TOOL_EXECUTION_FACTS
        .try_with(|resolver| resolver.resolve(call))
        .unwrap_or_else(|_| {
            Err(ToolError::Execution(
                "tool execution catalog is unavailable outside Runtime".into(),
            ))
        })
}

/// Run one executor future with its durable runtime coordinates visible to the
/// called tool. This keeps execution authority out of provider protocol ids and
/// model-authored arguments.
pub async fn with_tool_operation_context<T>(
    context: ToolOperationContext,
    future: impl std::future::Future<Output = T>,
) -> T {
    TOOL_OPERATION_CONTEXT.scope(context, future).await
}

/// Run one tool future with a read-only materialized State snapshot. The tool
/// can only return [`StateCommand`] values on [`ToolOutput`]; Runtime remains
/// the sole writer and commits them with the result.
pub async fn with_tool_state_context<T>(
    state: StateStore,
    future: impl std::future::Future<Output = T>,
) -> T {
    TOOL_STATE_CONTEXT.scope(Arc::new(state), future).await
}

/// Scope one immutable execution catalog to a Runtime-owned invocation.
pub async fn with_tool_execution_facts<T>(
    resolver: Arc<dyn ToolExecutionFactsResolver>,
    future: impl std::future::Future<Output = T>,
) -> T {
    TOOL_EXECUTION_FACTS.scope(resolver, future).await
}

/// Compatibility helper for direct callers that only need durable operation
/// identity. Runtime execution uses [`with_tool_operation_context`] and always
/// supplies a run id.
pub async fn with_tool_operation_id<T>(
    operation_id: String,
    future: impl std::future::Future<Output = T>,
) -> T {
    with_tool_operation_context(
        ToolOperationContext {
            run_id: None,
            thread_id: None,
            operation_id,
            call_id: None,
            execution_scope: None,
        },
        future,
    )
    .await
}

/// What an implementation can safely do after the owner died while an invocation
/// was in flight. This is a trusted property of the executable tool, not a claim
/// supplied by the model or by an agent configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecoveryCapability {
    /// The external outcome cannot be determined and the call must not be replayed.
    #[default]
    NonRecoverable,
    /// Repeating the call is observationally equivalent to executing it once.
    ReplaySafe,
    /// Repeating the call with the same stable call id is idempotent.
    Idempotent,
    /// The call creates or addresses a durable request by stable identity; recovery
    /// reconnects to that request instead of creating another one.
    DurableRequest,
}

/// The recovery behavior selected for one tool in an executable agent snapshot.
/// `NeverReplay` is always legal; every other mode requires the matching trusted
/// [`ToolRecoveryCapability`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecoveryMode {
    #[default]
    NeverReplay,
    ReplaySafe,
    Idempotent,
    DurableRequest,
}

impl ToolRecoveryMode {
    /// Fail closed when configuration attempts to widen what the implementation
    /// actually guarantees. A deployment may always choose `NeverReplay`.
    #[must_use]
    pub const fn is_supported_by(self, capability: ToolRecoveryCapability) -> bool {
        matches!(
            (self, capability),
            (Self::NeverReplay, _)
                | (Self::ReplaySafe, ToolRecoveryCapability::ReplaySafe)
                | (Self::Idempotent, ToolRecoveryCapability::Idempotent)
                | (Self::DurableRequest, ToolRecoveryCapability::DurableRequest)
        )
    }
}

/// Per-tool operational recovery settings pinned into the resolved snapshot.
///
/// Fields are deliberately private: callers cannot bypass the non-zero attempt
/// budget enforced by the constructors and by Serde's `NonZeroU16` decoding.
///
/// ```compile_fail
/// use awaken_runtime_contract::tool::{ToolRecoveryMode, ToolRecoveryPolicy};
///
/// let _ = ToolRecoveryPolicy {
///     mode: ToolRecoveryMode::NeverReplay,
///     max_attempts: 0,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecoveryPolicy {
    #[serde(default)]
    mode: ToolRecoveryMode,
    #[serde(default = "default_max_attempts")]
    max_attempts: NonZeroU16,
}

const fn default_max_attempts() -> NonZeroU16 {
    match NonZeroU16::new(3) {
        Some(value) => value,
        None => unreachable!(),
    }
}

impl Default for ToolRecoveryPolicy {
    fn default() -> Self {
        Self {
            mode: ToolRecoveryMode::NeverReplay,
            max_attempts: default_max_attempts(),
        }
    }
}

impl ToolRecoveryPolicy {
    #[must_use]
    pub const fn new(mode: ToolRecoveryMode, max_attempts: NonZeroU16) -> Self {
        Self { mode, max_attempts }
    }

    /// Construct a recovery policy while making a zero attempt budget
    /// unrepresentable in the resulting domain value.
    pub fn try_new(mode: ToolRecoveryMode, max_attempts: u16) -> Result<Self, ToolRecoveryError> {
        let max_attempts = NonZeroU16::new(max_attempts).ok_or(ToolRecoveryError::ZeroAttempts)?;
        Ok(Self::new(mode, max_attempts))
    }

    #[must_use]
    pub const fn replay_safe() -> Self {
        Self {
            mode: ToolRecoveryMode::ReplaySafe,
            max_attempts: default_max_attempts(),
        }
    }

    #[must_use]
    pub const fn durable_request() -> Self {
        Self {
            mode: ToolRecoveryMode::DurableRequest,
            max_attempts: default_max_attempts(),
        }
    }

    #[must_use]
    pub const fn mode(&self) -> ToolRecoveryMode {
        self.mode
    }

    #[must_use]
    pub const fn max_attempts(&self) -> NonZeroU16 {
        self.max_attempts
    }

    pub fn validate(&self, capability: ToolRecoveryCapability) -> Result<(), ToolRecoveryError> {
        if !self.mode.is_supported_by(capability) {
            return Err(ToolRecoveryError::Unsupported {
                mode: self.mode,
                capability,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ToolRecoveryError {
    #[error("tool recovery max_attempts must be greater than zero")]
    ZeroAttempts,
    #[error("recovery mode {mode:?} exceeds executable capability {capability:?}")]
    Unsupported {
        mode: ToolRecoveryMode,
        capability: ToolRecoveryCapability,
    },
}

/// Neutral result of one tool invocation. `is_error` lets a tool return a
/// model-visible failure without aborting the run; `state` carries any state
/// transitions the tool wants staged onto the commit boundary (never a direct
/// store write — the same path hooks use).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<StateCommand>,
}

impl ToolOutput {
    pub fn ok(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::ok_blocks(call_id, vec![ContentBlock::text(content)])
    }

    pub fn ok_blocks(call_id: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self {
            call_id: call_id.into(),
            content,
            is_error: false,
            state: Vec::new(),
        }
    }

    pub fn error(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::error_blocks(call_id, vec![ContentBlock::text(content)])
    }

    pub fn error_blocks(call_id: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self {
            call_id: call_id.into(),
            content,
            is_error: true,
            state: Vec::new(),
        }
    }

    /// A derived plain-text view for policies that intentionally match, log, or
    /// spill text. The structured blocks remain the sole stored result.
    #[must_use]
    pub fn text(&self) -> String {
        extract_text(&self.content)
    }

    /// Stage state transitions to be committed with this tool's result.
    #[must_use]
    pub fn with_state(mut self, state: Vec<StateCommand>) -> Self {
        self.state = state;
        self
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    /// The executor proved that the request never crossed its dispatch boundary.
    /// An owner may reacquire that executor and retry the same call without
    /// replaying an external effect. Failures after dispatch remain `Execution`.
    #[error("tool executor unavailable before dispatch: {0}")]
    UnavailableBeforeDispatch(String),
    /// Includes non-retryable local executor-configuration rejection as well as
    /// failures after the dispatch boundary. An owner must not replace an
    /// executor merely because this variant was returned.
    #[error("tool execution failed: {0}")]
    Execution(String),
}

/// Materializes a model-visible tool result when the host needs to move a large
/// payload out of the transcript. The execution backends call this one neutral
/// port after a tool result exists and before it becomes durable; the host owns
/// the sandbox path, size policy, and preview format.
///
/// Implementations must be idempotent for the same `(run_id, call_id)`: recovery
/// may execute a replay-safe tool again, and both the Native and ACP executors
/// use the stable result address supplied here.
#[async_trait]
pub trait ToolOutputSpiller: Send + Sync {
    async fn spill(
        &self,
        run_id: &RunId,
        call_id: &str,
        content: String,
    ) -> Result<String, ToolError>;
}

/// Authoritative execution location for one tool implementation.
///
/// The safe extension default is `Brain`: MCP, Skills and orchestration tools
/// remain beside the model/runtime. Tools that touch the workload filesystem,
/// process or network must explicitly opt into `Sandbox`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionTarget {
    #[default]
    Brain,
    Sandbox,
}

/// Stable address of an external or runtime-visible resource used to decide
/// whether two tool calls may overlap. The namespace prevents unrelated
/// extensions from accidentally aliasing equal local keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolResource {
    pub namespace: String,
    pub key: String,
}

impl ToolResource {
    #[must_use]
    pub fn new(namespace: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            key: key.into(),
        }
    }
}

/// One prospective access made by a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResourceAccess {
    Read(ToolResource),
    Write(ToolResource),
}

impl ToolResourceAccess {
    fn parts(&self) -> (&ToolResource, ResourceAccessMode) {
        match self {
            Self::Read(resource) => (resource, ResourceAccessMode::Read),
            Self::Write(resource) => (resource, ResourceAccessMode::Write),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceAccessMode {
    Read,
    Write,
}

impl ResourceAccessMode {
    const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Read, Self::Read) => Self::Read,
            _ => Self::Write,
        }
    }

    const fn conflicts_with(self, other: Self) -> bool {
        matches!((self, other), (Self::Write, _) | (_, Self::Write))
    }
}

/// Execution-time concurrency intent for one concrete tool call.
///
/// This is intentionally independent from recovery: idempotent replay of one
/// operation says nothing about whether two different operations commute.
///
/// All variants compose through one conservative conflict algebra. Keeping the
/// public enum makes a tool author's intent visible without method-only
/// pseudo-constructors or a second scheduler-specific policy type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolConcurrency {
    /// The call must not overlap any other call in the same model step.
    Serial,
    /// Default: admit the call into the largest compatible execution wave.
    #[default]
    Parallel,
    /// Read/write claims over stable domain-resource identities.
    Resources(Vec<ToolResourceAccess>),
}

impl ToolConcurrency {
    fn canonical_resources(
        accesses: impl IntoIterator<Item = ToolResourceAccess>,
    ) -> Vec<ToolResourceAccess> {
        let mut canonical: std::collections::BTreeMap<ToolResource, ResourceAccessMode> =
            std::collections::BTreeMap::new();
        for access in accesses {
            let (resource, mode) = access.parts();
            canonical
                .entry(resource.clone())
                .and_modify(|current| *current = current.merge(mode))
                .or_insert(mode);
        }
        canonical
            .into_iter()
            .map(|(resource, mode)| match mode {
                ResourceAccessMode::Read => ToolResourceAccess::Read(resource),
                ResourceAccessMode::Write => ToolResourceAccess::Write(resource),
            })
            .collect()
    }

    /// Conservatively combine independent declarations. Neither side can
    /// widen the other: exclusive remains exclusive and resource claims union.
    #[must_use]
    pub fn narrowed_with(self, constraint: Self) -> Self {
        match (self, constraint) {
            (Self::Serial, _) | (_, Self::Serial) => Self::Serial,
            (Self::Parallel, Self::Parallel) => Self::Parallel,
            (Self::Parallel, Self::Resources(accesses))
            | (Self::Resources(accesses), Self::Parallel) => {
                Self::Resources(Self::canonical_resources(accesses))
            }
            (Self::Resources(left), Self::Resources(right)) => {
                Self::Resources(Self::canonical_resources(left.into_iter().chain(right)))
            }
        }
    }

    #[must_use]
    pub fn compatible_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Serial, _) | (_, Self::Serial) => false,
            (Self::Parallel, _) | (_, Self::Parallel) => true,
            (Self::Resources(left), Self::Resources(right)) => !left.iter().any(|left| {
                let (left_resource, left_mode) = left.parts();
                right.iter().any(|right| {
                    let (right_resource, right_mode) = right.parts();
                    left_resource == right_resource && left_mode.conflicts_with(right_mode)
                })
            }),
        }
    }
}

/// Schema-erased tool: the dynamic call boundary used by the runtime and by
/// MCP/server/client adapters. Concrete implementations live in
/// extension/adapter crates, never in neutral crates.
#[async_trait]
pub trait RawTool: Send + Sync {
    fn id(&self) -> &str;
    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Brain
    }
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::NonRecoverable
    }
    /// Optionally narrow the default parallel execution for this invocation.
    /// Stateful tools declare `Serial` or their exact resource accesses.
    fn concurrency(&self, _arguments: &serde_json::Value) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError>;
}

/// Invoke an ordinary tool while honoring the Run's cooperative cancellation.
/// This helper is intentionally tool-agnostic: extensions construct their own
/// [`ToolCall`] payloads and do not depend on one another's concrete tool crates.
pub async fn invoke_raw_tool(
    tool: &dyn RawTool,
    call: ToolCall,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<ToolOutput, ToolError> {
    match cancellation {
        Some(token) => {
            tokio::select! {
                result = tool.invoke(call) => result,
                _ = token.cancelled() => Err(ToolError::Execution("tool invocation cancelled".into())),
            }
        }
        None => tool.invoke(call).await,
    }
}

/// Preferred typed tool API. Authors implement this with concrete argument and
/// output types; the argument type is also the sole source of the model-visible
/// JSON Schema. An adapter erases the implementation into a `RawTool` for
/// execution.
#[async_trait]
pub trait Tool: Send + Sync {
    type Args: serde::de::DeserializeOwned + schemars::JsonSchema + Send;
    type Output: Serialize + Send;

    /// Stable model-visible identity. Keeping this on the implementation makes
    /// registration and execution share one authority instead of repeating a
    /// string in a catalog.
    const ID: &'static str;
    /// Model-visible purpose paired with [`Self::Args`] when the descriptor is
    /// generated.
    const DESCRIPTION: &'static str;

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::NonRecoverable
    }
    /// Typed source of the invocation's concurrency contract. Tool authors use
    /// their domain arguments directly; JSON inspection stays at the erased
    /// external-tool boundary.
    fn concurrency(&self, _args: &Self::Args) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }
    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError>;
}

/// Parse the dynamic wire arguments at the single typed-tool boundary.
///
/// JSON `null` has historically meant an omitted argument object, so it is
/// normalized to `{}` exactly once. Every typed tool and legacy `RawTool`
/// adapter must use this function rather than inventing its own null/error rules.
pub fn parse_tool_args<A: serde::de::DeserializeOwned>(
    arguments: serde_json::Value,
) -> Result<A, ToolError> {
    let raw = if arguments.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        arguments
    };
    serde_json::from_value(raw).map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

/// Parse arguments for a legacy [`RawTool`] that intentionally reports invalid
/// model input as a model-visible tool result instead of aborting the Run.
/// This preserves that explicit policy while sharing the same parser and error
/// wording as the typed-tool erasure adapter.
pub fn parse_tool_args_or_error_output<A: serde::de::DeserializeOwned>(
    call_id: &str,
    arguments: serde_json::Value,
) -> Result<A, ToolOutput> {
    parse_tool_args(arguments).map_err(|error| {
        let detail = match error {
            ToolError::InvalidArguments(detail) => detail,
            other => other.to_string(),
        };
        ToolOutput::error(call_id, format!("invalid arguments: {detail}"))
    })
}

/// Render typed tool output using the one model-visible representation rule.
pub fn render_tool_output<O: Serialize>(output: &O) -> Result<String, ToolError> {
    match serde_json::to_value(output).map_err(|error| ToolError::Execution(error.to_string()))? {
        serde_json::Value::String(text) => Ok(text),
        other => {
            serde_json::to_string(&other).map_err(|error| ToolError::Execution(error.to_string()))
        }
    }
}

/// The port the execution loop calls to run one already-authorized tool call.
/// Where the call runs is hidden behind this port and owned by its implementer
/// (the orchestration layer above), not the runtime core.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Trusted recovery capability of the concrete executor for `tool_id`.
    /// Remote/general executors fail closed unless they explicitly advertise one.
    fn recovery_capability(&self, _tool_id: &str) -> ToolRecoveryCapability {
        ToolRecoveryCapability::NonRecoverable
    }

    /// Concurrency contract of the concrete routed executor. The default admits
    /// maximum parallelism; stateful executors must explicitly narrow it.
    fn concurrency(&self, _tool_id: &str, _arguments: &serde_json::Value) -> ToolConcurrency {
        ToolConcurrency::Parallel
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError>;
}

/// One authoritative registry for schema-erased tool implementations.
///
/// Runtime composition, a realized local Environment, and a remote Hand all
/// need the same `tool id -> implementation` resolution rule. Duplicate ids are
/// retained as an explicit ambiguous slot and fail closed instead of depending
/// on insertion order.
#[derive(Clone, Default)]
pub struct RawToolRegistry {
    tools: HashMap<String, RawToolSlot>,
}

#[derive(Clone)]
enum RawToolSlot {
    Unique(Arc<dyn RawTool>),
    Ambiguous,
}

impl RawToolRegistry {
    /// Build a registry from executable tools. An id appearing more than once is
    /// permanently ambiguous in this registry, even if the same object repeats.
    pub fn new(tools: impl IntoIterator<Item = Arc<dyn RawTool>>) -> Self {
        let mut registry = Self::default();
        for tool in tools {
            registry.insert(tool);
        }
        registry
    }

    /// Register one tool, failing future resolution closed if the id already
    /// exists. Returning `false` lets composition roots surface the conflict.
    pub fn insert(&mut self, tool: Arc<dyn RawTool>) -> bool {
        use std::collections::hash_map::Entry;

        match self.tools.entry(tool.id().to_string()) {
            Entry::Vacant(entry) => {
                entry.insert(RawToolSlot::Unique(tool));
                true
            }
            Entry::Occupied(mut entry) => {
                entry.insert(RawToolSlot::Ambiguous);
                false
            }
        }
    }

    /// Resolve one unique implementation. Unknown and ambiguous ids both return
    /// `None`; [`Self::invoke`] preserves the more specific model-visible error.
    #[must_use]
    pub fn get(&self, tool_id: &str) -> Option<&Arc<dyn RawTool>> {
        match self.tools.get(tool_id) {
            Some(RawToolSlot::Unique(tool)) => Some(tool),
            Some(RawToolSlot::Ambiguous) | None => None,
        }
    }

    fn resolution_error(&self, tool_id: &str) -> ToolError {
        match self.tools.get(tool_id) {
            Some(RawToolSlot::Ambiguous) => {
                ToolError::Execution(format!("tool id `{tool_id}` is ambiguous"))
            }
            Some(RawToolSlot::Unique(_)) => unreachable!("unique tools resolve before errors"),
            None => ToolError::Unknown(tool_id.to_string()),
        }
    }
}

#[async_trait]
impl ToolExecutor for RawToolRegistry {
    fn recovery_capability(&self, tool_id: &str) -> ToolRecoveryCapability {
        self.get(tool_id)
            .map_or(ToolRecoveryCapability::NonRecoverable, |tool| {
                tool.recovery_capability()
            })
    }

    fn concurrency(&self, tool_id: &str, arguments: &serde_json::Value) -> ToolConcurrency {
        self.get(tool_id)
            .map_or_else(ToolConcurrency::default, |tool| tool.concurrency(arguments))
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let tool = self
            .get(&call.tool_id)
            .ok_or_else(|| self.resolution_error(&call.tool_id))?;
        tool.invoke(call.clone()).await
    }
}

#[cfg(test)]
mod tool_output_content_tests {
    use super::*;

    #[test]
    fn multimodal_output_round_trips_without_a_text_shadow() {
        // Cause/effect rule T1: ordered text + base64 image -> one serialized
        // ToolOutput whose canonical `content` round-trips byte-for-byte; the
        // plain-text view is derived and excludes image bytes. T2 text-only
        // constructors still produce exactly one Text block.
        let output = ToolOutput::ok_blocks(
            "call-1",
            vec![
                ContentBlock::text("review"),
                ContentBlock::image_base64("image/png", "iVBORw0KGgo="),
            ],
        );
        let encoded = serde_json::to_vec(&output).expect("serialize");
        let restored: ToolOutput = serde_json::from_slice(&encoded).expect("deserialize");
        assert_eq!(restored, output);
        assert_eq!(restored.text(), "review");
        assert_eq!(
            ToolOutput::ok("call-2", "done").content,
            vec![ContentBlock::text("done")]
        );
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct OptionalArgs {
        #[serde(default)]
        topic: Option<String>,
    }

    #[test]
    fn typed_argument_boundary_has_one_null_and_unknown_field_rule() {
        // Cause graph / decision table:
        // null -> {} -> optional args; exact object -> typed args;
        // missing required/unknown field -> InvalidArguments.
        assert_eq!(
            parse_tool_args::<OptionalArgs>(serde_json::Value::Null).unwrap(),
            OptionalArgs { topic: None }
        );
        assert_eq!(
            parse_tool_args::<OptionalArgs>(serde_json::json!({"topic": "tools"})).unwrap(),
            OptionalArgs {
                topic: Some("tools".into())
            }
        );
        assert!(matches!(
            parse_tool_args::<OptionalArgs>(serde_json::json!({"extra": true})),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn operation_context_is_scoped_to_one_executor_future() {
        // Cause-effect graph:
        // runtime scope present -> expose exact Run/Thread/durable-operation and
        // model-correlation coordinates;
        // nested future completes -> scope is removed; no runtime scope -> None.
        // Decision table: R1(outside)=None, R2(inside)=exact context,
        // R3(after completion)=None. This also proves there is one context source
        // rather than independent run-id and operation-id task locals.
        // Constraints/invariants: scope is future-local and removed on exit;
        // every helper reads the same context owner.
        assert_eq!(current_tool_operation_context(), None);
        let expected = ToolOperationContext {
            run_id: Some(RunId("run-7".into())),
            thread_id: Some(ThreadId("thread-4".into())),
            operation_id: "tool-batch:run-7:3:c1".into(),
            call_id: Some("c1".into()),
            execution_scope: None,
        };
        let seen = with_tool_operation_context(expected.clone(), async {
            (
                current_tool_operation_context(),
                current_tool_operation_id(),
                current_tool_operation_token(),
            )
        })
        .await;
        assert_eq!(seen.0, Some(expected.clone()));
        assert_eq!(seen.1.as_deref(), Some(expected.operation_id.as_str()));
        assert_eq!(
            seen.2.as_ref().map(ToolOperationToken::operation_id),
            Some(expected.operation_id.as_str())
        );
        assert_eq!(current_tool_operation_context(), None);
    }

    #[tokio::test]
    async fn direct_context_helpers_do_not_invent_runtime_correlation_coordinates() {
        // Cause-effect graph / decision table:
        // C1=legacy adapter knows a Run + durable operation; C2=direct caller
        // knows only a durable operation. R1 C1 -> preserve the supplied Run but
        // leave Thread/model-call/scope absent. R2 C2 -> leave every trusted
        // Runtime coordinate absent. Neither helper invents correlation data.
        // Effects: supplied legacy coordinates survive and every unknown axis
        // remains None. Constraints/invariants: helpers preserve, never infer.
        let run_context = ToolOperationContext::for_run("run-direct", "operation-direct");
        assert_eq!(run_context.run_id, Some(RunId("run-direct".into())), "R1");
        assert_eq!(run_context.thread_id, None, "R1");
        assert_eq!(run_context.call_id, None, "R1");
        assert_eq!(run_context.execution_scope, None, "R1");

        let operation_context = with_tool_operation_id("operation-only".into(), async {
            current_tool_operation_context().expect("R2 scoped operation context")
        })
        .await;
        assert_eq!(operation_context.run_id, None, "R2");
        assert_eq!(operation_context.thread_id, None, "R2");
        assert_eq!(operation_context.operation_id, "operation-only", "R2");
        assert_eq!(operation_context.call_id, None, "R2");
        assert_eq!(operation_context.execution_scope, None, "R2");
    }

    #[test]
    fn operation_token_ledger_identity_preserves_the_existing_durable_axes() {
        // Cause-effect graph / decision table:
        // C1=operation/run/workspace/infrastructure scope changes;
        // C2=logical Thread changes; C3=model call id changes.
        // R1 C1 -> a different ledger identity. R2 C2 or C3 alone -> the same
        // ledger identity because these new correlation fields must not create a
        // parallel idempotency key or alter the established operation token.
        // Constraints/invariants: only the established operation/run/workspace/
        // infrastructure axes own ledger identity; Thread/call are observational.
        let base = ToolOperationContext {
            run_id: Some(RunId("run-7".into())),
            thread_id: Some(ThreadId("thread-1".into())),
            operation_id: "operation-1".into(),
            call_id: Some("call-1".into()),
            execution_scope: Some(awaken_tenancy::ExecutionScopeRef(awaken_tenancy::ScopeId(
                "workspace-a".into(),
            ))),
        };
        let token = ToolOperationToken::from_context(&base).unwrap();
        let exact = token.ledger_id(Some("session-1"));
        assert_eq!(exact, token.ledger_id(Some("session-1")));

        let mut another_operation = base.clone();
        another_operation.operation_id = "operation-2".into();
        assert_ne!(
            exact,
            ToolOperationToken::from_context(&another_operation)
                .unwrap()
                .ledger_id(Some("session-1"))
        );

        let mut another_run = base.clone();
        another_run.run_id = Some(RunId("run-8".into()));
        assert_ne!(
            exact,
            ToolOperationToken::from_context(&another_run)
                .unwrap()
                .ledger_id(Some("session-1"))
        );

        let mut another_thread = base.clone();
        another_thread.thread_id = Some(ThreadId("thread-2".into()));
        assert_eq!(
            exact,
            ToolOperationToken::from_context(&another_thread)
                .unwrap()
                .ledger_id(Some("session-1"))
        );

        let mut another_call = base.clone();
        another_call.call_id = Some("call-2".into());
        assert_eq!(
            exact,
            ToolOperationToken::from_context(&another_call)
                .unwrap()
                .ledger_id(Some("session-1"))
        );

        let mut another_workspace = base;
        another_workspace.execution_scope = Some(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId("workspace-b".into()),
        ));
        assert_ne!(
            exact,
            ToolOperationToken::from_context(&another_workspace)
                .unwrap()
                .ledger_id(Some("session-1"))
        );
        assert_ne!(exact, token.ledger_id(Some("session-2")));
    }

    #[test]
    fn configuration_can_only_match_or_reduce_capability() {
        assert!(
            ToolRecoveryMode::NeverReplay.is_supported_by(ToolRecoveryCapability::DurableRequest)
        );
        assert!(
            ToolRecoveryMode::DurableRequest
                .is_supported_by(ToolRecoveryCapability::DurableRequest)
        );
        assert!(
            !ToolRecoveryMode::ReplaySafe.is_supported_by(ToolRecoveryCapability::NonRecoverable)
        );
        assert!(!ToolRecoveryMode::Idempotent.is_supported_by(ToolRecoveryCapability::ReplaySafe));
    }

    #[test]
    fn zero_attempt_budget_fails_closed() {
        assert_eq!(
            ToolRecoveryPolicy::try_new(ToolRecoveryMode::NeverReplay, 0),
            Err(ToolRecoveryError::ZeroAttempts)
        );
        assert!(
            serde_json::from_value::<ToolRecoveryPolicy>(serde_json::json!({
                "mode": "never_replay",
                "max_attempts": 0
            }))
            .is_err(),
            "persisted/wire zero attempt budgets never construct a policy"
        );
    }

    #[test]
    fn concurrency_claims_follow_the_resource_conflict_matrix() {
        // Decision table: same-resource read/read is compatible; any same-
        // resource pair containing a write conflicts; distinct resources do
        // not. Serial conflicts with every claim and Parallel with every
        // non-Serial claim. The runtime scheduler consumes only this rule.
        let shared = ToolResource::new("filesystem", "/workspace/a");
        let other = ToolResource::new("filesystem", "/workspace/b");
        let reads = ToolConcurrency::Resources(vec![ToolResourceAccess::Read(shared.clone())]);
        let writes = ToolConcurrency::Resources(vec![ToolResourceAccess::Write(shared.clone())]);
        let other_write = ToolConcurrency::Resources(vec![ToolResourceAccess::Write(other)]);

        assert_eq!(ToolConcurrency::default(), ToolConcurrency::Parallel);
        assert!(reads.compatible_with(&reads));
        assert!(!reads.compatible_with(&writes));
        assert!(!writes.compatible_with(&reads));
        assert!(!writes.compatible_with(&writes));
        assert!(writes.compatible_with(&other_write));
        assert!(ToolConcurrency::Parallel.compatible_with(&writes));
        assert!(!ToolConcurrency::Serial.compatible_with(&reads));
        assert!(!reads.compatible_with(&ToolConcurrency::Serial));
    }

    #[test]
    fn resource_claims_are_canonical_and_narrowing_never_widens() {
        let resource = ToolResource::new("filesystem", "/workspace/a");
        let duplicate = ToolConcurrency::Resources(vec![
            ToolResourceAccess::Read(resource.clone()),
            ToolResourceAccess::Write(resource.clone()),
            ToolResourceAccess::Read(resource.clone()),
        ]);
        let write = ToolConcurrency::Resources(vec![ToolResourceAccess::Write(resource)]);

        assert_eq!(
            duplicate.narrowed_with(ToolConcurrency::Parallel),
            write,
            "write dominates duplicate reads"
        );
        assert_eq!(
            ToolConcurrency::Parallel.narrowed_with(write.clone()),
            write
        );
        assert_eq!(
            ToolConcurrency::Serial.narrowed_with(ToolConcurrency::Parallel),
            ToolConcurrency::Serial
        );
    }
}

#[cfg(kani)]
#[kani::proof]
fn same_resource_conflict_is_symmetric_and_exactly_one_write_or_more() {
    let left_writes: bool = kani::any();
    let right_writes: bool = kani::any();
    // Prove the finite decision kernel, not Rust's heap/String machinery. The
    // public-algebra test above is the refinement check from one equal-resource
    // claim pair into this kernel; distinct-resource filtering is structural.
    let left = if left_writes {
        ResourceAccessMode::Write
    } else {
        ResourceAccessMode::Read
    };
    let right = if right_writes {
        ResourceAccessMode::Write
    } else {
        ResourceAccessMode::Read
    };

    assert_eq!(left.conflicts_with(right), right.conflicts_with(left));
    assert_eq!(left.conflicts_with(right), left_writes || right_writes);
}
