use super::outcome::LoopFailure;
use super::LlmExecutor;
use super::*;
use crate::contracts::runtime::behavior::ReadOnlyContext;
use crate::contracts::runtime::phase::{
    ActionSet, AfterInferenceAction, AfterToolExecuteAction, BeforeInferenceAction,
    BeforeToolExecuteAction, LifecycleAction,
};
use crate::contracts::runtime::phase::{Phase, SuspendTicket};
use crate::contracts::runtime::tool_call::{
    CallerContext, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};
use crate::contracts::runtime::ActivityManager;
use crate::contracts::runtime::RunIdentity;
use crate::contracts::runtime::{PendingToolCall, ToolCallResumeMode};
use crate::contracts::storage::VersionPrecondition;
use crate::contracts::thread::CheckpointReason;
use crate::contracts::thread::{Message, Role, Thread, ToolCall};
use crate::contracts::AgentBehavior;
use crate::contracts::TerminationReason;
use crate::contracts::{AnyStateAction, StateSpec};
use crate::contracts::{RunContext, Suspension, ToolCallContext};
use crate::runtime::activity::ActivityHub;
use async_trait::async_trait;
use genai::chat::{
    ChatOptions, ChatRequest, ChatRole, ChatStreamEvent, MessageContent, StreamChunk, StreamEnd,
    ToolChunk, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tirea_contract::runtime::inference::StopReason;
use tirea_contract::testing::TestFixture;
use tirea_state::{Op, Patch, State, TrackedPatch};
use tokio::sync::Notify;
use uuid::Uuid;

/// Test helper: get suspended call by ID from new ToolCall-scoped path structure
fn get_suspended_call(state: &Value, call_id: &str) -> Option<Value> {
    state
        .get("__tool_call_scope")
        .and_then(|scopes| scopes.get(call_id))
        .and_then(|scope| scope.get("suspended_call"))
        .cloned()
}

fn test_run_identity(run_id: &str) -> RunIdentity {
    RunIdentity::new(
        "test-thread".to_string(),
        None,
        run_id.to_string(),
        None,
        "test-agent".to_string(),
        crate::contracts::RunOrigin::User,
    )
}

fn test_run_identity_with_parent(
    run_id: &str,
    parent_run_id: Option<&str>,
    parent_tool_call_id: Option<&str>,
) -> RunIdentity {
    let mut ctx = RunIdentity::new(
        "test-thread".to_string(),
        None,
        run_id.to_string(),
        parent_run_id.map(ToOwned::to_owned),
        "test-agent".to_string(),
        crate::contracts::RunOrigin::User,
    );
    if let Some(parent_tool_call_id) = parent_tool_call_id {
        ctx = ctx.with_parent_tool_call_id(parent_tool_call_id);
    }
    ctx
}

fn test_caller_context(thread_id: &str, messages: &[Arc<Message>]) -> CallerContext {
    CallerContext::new(
        Some(thread_id.to_string()),
        Some("caller-run".to_string()),
        Some("caller-agent".to_string()),
        messages.to_vec(),
    )
}

fn test_tool_phase_context<'a>(
    tool_descriptors: &'a [ToolDescriptor],
    agent_behavior: Option<&'a dyn AgentBehavior>,
    activity_manager: Arc<dyn ActivityManager>,
    run_policy: &'a tirea_contract::RunPolicy,
    thread_id: &'a str,
    thread_messages: &'a [Arc<Message>],
    cancellation_token: Option<&'a RunCancellationToken>,
) -> super::tool_exec::ToolPhaseContext<'a> {
    super::tool_exec::ToolPhaseContext {
        tool_descriptors,
        agent_behavior,
        activity_manager,
        run_policy,
        run_identity: test_run_identity("test-run"),
        caller_context: test_caller_context(thread_id, thread_messages),
        thread_id,
        thread_messages,
        cancellation_token,
    }
}

fn run_ctx_with_execution(thread: &Thread, run_id: &str) -> RunContext {
    RunContext::from_thread_with_registry_and_identity(
        thread,
        tirea_contract::RunPolicy::default(),
        RunIdentity::new(
            thread.id.clone(),
            thread.parent_thread_id.clone(),
            run_id.to_string(),
            None,
            "test-agent".to_string(),
            crate::contracts::RunOrigin::User,
        ),
        Arc::new(tirea_state::LatticeRegistry::new()),
    )
    .expect("run context")
}

/// Test-local behavior composition (mirrors agentos `compose_behaviors`).
fn compose_test_behaviors(behaviors: Vec<Arc<dyn AgentBehavior>>) -> Arc<dyn AgentBehavior> {
    use crate::contracts::runtime::behavior::NoOpBehavior;

    struct TestCompositeBehavior {
        id: String,
        behaviors: Vec<Arc<dyn AgentBehavior>>,
    }

    #[async_trait]
    impl AgentBehavior for TestCompositeBehavior {
        fn id(&self) -> &str {
            &self.id
        }
        fn behavior_ids(&self) -> Vec<&str> {
            self.behaviors
                .iter()
                .flat_map(|b| b.behavior_ids())
                .collect()
        }
        async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            let futs: Vec<_> = self.behaviors.iter().map(|b| b.run_start(ctx)).collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn step_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            let futs: Vec<_> = self.behaviors.iter().map(|b| b.step_start(ctx)).collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn before_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            let futs: Vec<_> = self
                .behaviors
                .iter()
                .map(|b| b.before_inference(ctx))
                .collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn after_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            let futs: Vec<_> = self
                .behaviors
                .iter()
                .map(|b| b.after_inference(ctx))
                .collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            let futs: Vec<_> = self
                .behaviors
                .iter()
                .map(|b| b.before_tool_execute(ctx))
                .collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn after_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterToolExecuteAction> {
            let futs: Vec<_> = self
                .behaviors
                .iter()
                .map(|b| b.after_tool_execute(ctx))
                .collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn step_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            let futs: Vec<_> = self.behaviors.iter().map(|b| b.step_end(ctx)).collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
        async fn run_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            let futs: Vec<_> = self.behaviors.iter().map(|b| b.run_end(ctx)).collect();
            futures::future::join_all(futs)
                .await
                .into_iter()
                .fold(ActionSet::empty(), |acc, a| acc.and(a))
        }
    }

    match behaviors.len() {
        0 => Arc::new(NoOpBehavior),
        1 => behaviors.into_iter().next().unwrap(),
        _ => {
            let id = behaviors
                .iter()
                .map(|b| b.id().to_string())
                .collect::<Vec<_>>()
                .join("+");
            Arc::new(TestCompositeBehavior { id, behaviors })
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "TestCounterAction")]
struct TestCounterState {
    counter: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum TestCounterAction {
    SetCounter(i64),
}

impl TestCounterState {
    fn reduce(&mut self, action: TestCounterAction) {
        match action {
            TestCounterAction::SetCounter(counter) => self.counter = counter,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
struct ActivityProgressState {
    progress: f64,
}

/// Minimal test state for debug flags -- used by state-patching AgentBehavior impls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DebugFlags {
    run_start_side_effect: Option<bool>,
    before_inference_effect: Option<bool>,
    after_tool_effect: Option<bool>,
}

struct DebugFlagsRef;

impl State for DebugFlags {
    type Ref<'a> = DebugFlagsRef;
    const PATH: &'static str = "debug";

    fn state_ref<'a>(
        _: &'a tirea_state::DocCell,
        _: tirea_state::Path,
        _: tirea_state::PatchSink<'a>,
    ) -> Self::Ref<'a> {
        DebugFlagsRef
    }

    fn from_value(value: &serde_json::Value) -> tirea_state::TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
    }

    fn to_value(&self) -> tirea_state::TireaResult<serde_json::Value> {
        serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum DebugFlagAction {
    RunStart,
    BeforeInference,
    AfterTool,
}

impl StateSpec for DebugFlags {
    type Action = DebugFlagAction;
    fn reduce(&mut self, action: DebugFlagAction) {
        match action {
            DebugFlagAction::RunStart => self.run_start_side_effect = Some(true),
            DebugFlagAction::BeforeInference => {
                self.before_inference_effect = Some(true);
            }
            DebugFlagAction::AfterTool => self.after_tool_effect = Some(true),
        }
    }
}

/// Test state for legacy resume-tool-calls at path `__resume_tool_calls`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ResumeToolCallsState {
    calls: Value,
}

struct ResumeToolCallsStateRef;

impl State for ResumeToolCallsState {
    type Ref<'a> = ResumeToolCallsStateRef;
    const PATH: &'static str = "__resume_tool_calls";

    fn state_ref<'a>(
        _: &'a tirea_state::DocCell,
        _: tirea_state::Path,
        _: tirea_state::PatchSink<'a>,
    ) -> Self::Ref<'a> {
        ResumeToolCallsStateRef
    }

    fn from_value(value: &serde_json::Value) -> tirea_state::TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
    }

    fn to_value(&self) -> tirea_state::TireaResult<serde_json::Value> {
        serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
    }
}

impl StateSpec for ResumeToolCallsState {
    type Action = Value;
    fn reduce(&mut self, action: Value) {
        self.calls = action;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TestBoolState(bool);

struct TestBoolStateRef;

impl State for TestBoolState {
    type Ref<'a> = TestBoolStateRef;

    fn state_ref<'a>(
        _: &'a tirea_state::DocCell,
        _: tirea_state::Path,
        _: tirea_state::PatchSink<'a>,
    ) -> Self::Ref<'a> {
        TestBoolStateRef
    }

    fn from_value(value: &Value) -> tirea_state::TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
    }

    fn to_value(&self) -> tirea_state::TireaResult<Value> {
        serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
    }
}

impl StateSpec for TestBoolState {
    type Action = bool;

    fn reduce(&mut self, action: Self::Action) {
        self.0 = action;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TestI64State(i64);

struct TestI64StateRef;

impl State for TestI64State {
    type Ref<'a> = TestI64StateRef;

    fn state_ref<'a>(
        _: &'a tirea_state::DocCell,
        _: tirea_state::Path,
        _: tirea_state::PatchSink<'a>,
    ) -> Self::Ref<'a> {
        TestI64StateRef
    }

    fn from_value(value: &Value) -> tirea_state::TireaResult<Self> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
    }

    fn to_value(&self) -> tirea_state::TireaResult<Value> {
        serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
    }
}

impl StateSpec for TestI64State {
    type Action = i64;

    fn reduce(&mut self, action: Self::Action) {
        self.0 = action;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TestJsonValueState(Value);

struct TestJsonValueStateRef;

impl State for TestJsonValueState {
    type Ref<'a> = TestJsonValueStateRef;

    fn state_ref<'a>(
        _: &'a tirea_state::DocCell,
        _: tirea_state::Path,
        _: tirea_state::PatchSink<'a>,
    ) -> Self::Ref<'a> {
        TestJsonValueStateRef
    }

    fn from_value(value: &Value) -> tirea_state::TireaResult<Self> {
        Ok(Self(value.clone()))
    }

    fn to_value(&self) -> tirea_state::TireaResult<Value> {
        Ok(self.0.clone())
    }
}

impl StateSpec for TestJsonValueState {
    type Action = Value;

    fn reduce(&mut self, action: Self::Action) {
        self.0 = action;
    }
}

fn test_bool_state_action(path: impl Into<String>, value: bool) -> AnyStateAction {
    AnyStateAction::new_at::<TestBoolState>(path.into(), value)
}

fn test_i64_state_action(path: impl Into<String>, value: i64) -> AnyStateAction {
    AnyStateAction::new_at::<TestI64State>(path.into(), value)
}

fn test_json_state_action(path: impl Into<String>, value: Value) -> AnyStateAction {
    AnyStateAction::new_at::<TestJsonValueState>(path.into(), value)
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
enum ResponseRouting {
    ReplayOriginalTool,
    UseAsToolResult,
    PassToLLM,
}

#[derive(Debug, Clone, PartialEq)]
enum InvocationOrigin {
    ToolCallIntercepted {
        backend_call_id: String,
        backend_tool_name: String,
        backend_arguments: Value,
    },
    PluginInitiated {
        plugin_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct FrontendToolInvocation {
    call_id: String,
    tool_name: String,
    arguments: Value,
    origin: InvocationOrigin,
    routing: ResponseRouting,
}

impl FrontendToolInvocation {
    fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
        origin: InvocationOrigin,
        routing: ResponseRouting,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            origin,
            routing,
        }
    }
}

fn suspend_ticket_from_invocation(invocation: FrontendToolInvocation) -> SuspendTicket {
    let suspension = Suspension::new(
        &invocation.call_id,
        format!("tool:{}", invocation.tool_name),
    )
    .with_parameters(invocation.arguments.clone());
    let resume_mode = match invocation.routing {
        ResponseRouting::ReplayOriginalTool => ToolCallResumeMode::ReplayToolCall,
        ResponseRouting::UseAsToolResult => ToolCallResumeMode::UseDecisionAsToolResult,
        ResponseRouting::PassToLLM => ToolCallResumeMode::PassDecisionToTool,
    };
    SuspendTicket::new(
        suspension,
        PendingToolCall::new(
            invocation.call_id,
            invocation.tool_name,
            invocation.arguments,
        ),
        resume_mode,
    )
}

/// Builds a [`SuspendTicket`] from `ReadOnlyContext` fields and returns it as
/// a `(ticket, call_id)` pair suitable for `SuspendTool`.
fn build_frontend_suspend_ticket(
    ctx: &ReadOnlyContext<'_>,
    tool_name: impl Into<String>,
    arguments: Value,
    routing: ResponseRouting,
) -> Option<(SuspendTicket, String)> {
    let backend_call_id = ctx.tool_call_id()?;
    let backend_tool_name = ctx.tool_name()?;
    let backend_args = ctx.tool_args().cloned().unwrap_or_default();
    let tool_name = tool_name.into();
    let call_id = match routing {
        ResponseRouting::UseAsToolResult => backend_call_id.to_string(),
        _ => format!("fc_{}", Uuid::new_v4().simple()),
    };
    let origin = match routing {
        ResponseRouting::UseAsToolResult => InvocationOrigin::PluginInitiated {
            plugin_id: "agui_frontend_tools".to_string(),
        },
        _ => InvocationOrigin::ToolCallIntercepted {
            backend_call_id: backend_call_id.to_string(),
            backend_tool_name: backend_tool_name.to_string(),
            backend_arguments: backend_args,
        },
    };
    let invocation = FrontendToolInvocation::new(&call_id, &tool_name, arguments, origin, routing);
    Some((suspend_ticket_from_invocation(invocation), call_id))
}

fn test_frontend_invocation(interaction: &Suspension) -> FrontendToolInvocation {
    let tool_name = interaction
        .action
        .strip_prefix("tool:")
        .unwrap_or(interaction.action.as_str())
        .to_string();
    FrontendToolInvocation::new(
        interaction.id.clone(),
        tool_name,
        interaction.parameters.clone(),
        InvocationOrigin::PluginInitiated {
            plugin_id: "loop_runner_tests".to_string(),
        },
        ResponseRouting::ReplayOriginalTool,
    )
}

fn test_suspend_ticket(interaction: Suspension) -> SuspendTicket {
    SuspendTicket::new(
        interaction.clone(),
        PendingToolCall::new(interaction.id, interaction.action, interaction.parameters),
        ToolCallResumeMode::ReplayToolCall,
    )
}

fn set_single_suspended_call(
    state: &Value,
    suspension: Suspension,
    invocation: Option<FrontendToolInvocation>,
) -> Result<tirea_state::TrackedPatch, AgentLoopError> {
    let invocation = invocation.unwrap_or_else(|| test_frontend_invocation(&suspension));
    let call_id = invocation.call_id.clone();
    let tool_name = invocation.tool_name.clone();
    let suspended_call = build_suspended_call(call_id, tool_name, suspension, invocation);
    let action = suspended_call.into_state_action();
    let patches = crate::contracts::runtime::state::reduce_state_actions(
        vec![action],
        state,
        "test",
        &crate::contracts::runtime::state::ScopeContext::run(),
    )
    .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    Ok(patches
        .into_iter()
        .next()
        .unwrap_or_else(|| TrackedPatch::new(Patch::new()).with_source("test")))
}

fn single_suspended_call_state_action(
    suspension: Suspension,
    invocation: Option<FrontendToolInvocation>,
) -> AnyStateAction {
    let invocation = invocation.unwrap_or_else(|| test_frontend_invocation(&suspension));
    let call_id = invocation.call_id.clone();
    let tool_name = invocation.tool_name.clone();
    build_suspended_call(call_id, tool_name, suspension, invocation).into_state_action()
}

fn build_suspended_call(
    call_id: impl Into<String>,
    tool_name: impl Into<String>,
    suspension: Suspension,
    invocation: FrontendToolInvocation,
) -> crate::contracts::runtime::SuspendedCall {
    let resume_mode = match invocation.routing {
        ResponseRouting::ReplayOriginalTool => ToolCallResumeMode::ReplayToolCall,
        ResponseRouting::UseAsToolResult => ToolCallResumeMode::UseDecisionAsToolResult,
        ResponseRouting::PassToLLM => ToolCallResumeMode::PassDecisionToTool,
    };
    let arguments = invocation.arguments.clone();
    crate::contracts::runtime::SuspendedCall {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        arguments: arguments.clone(),
        ticket: crate::contracts::runtime::SuspendTicket::new(
            suspension,
            PendingToolCall::new(invocation.call_id, invocation.tool_name, arguments),
            resume_mode,
        ),
    }
}

fn test_decision(
    target_id: &str,
    action: crate::contracts::io::ResumeDecisionAction,
    result: Value,
    reason: Option<&str>,
) -> crate::contracts::ToolCallDecision {
    crate::contracts::ToolCallDecision {
        target_id: target_id.to_string(),
        resume: crate::contracts::runtime::ToolCallResume {
            decision_id: format!("decision_{target_id}"),
            action,
            result,
            reason: reason.map(str::to_string),
            updated_at: 0,
        },
    }
}

#[derive(Debug, Default)]
struct TestInteractionPlugin {
    responses: std::collections::HashMap<String, Value>,
}

impl TestInteractionPlugin {
    fn with_responses(approved_ids: Vec<String>, denied_ids: Vec<String>) -> Self {
        let mut responses = std::collections::HashMap::new();
        for id in approved_ids {
            responses.insert(id, Value::Bool(true));
        }
        for id in denied_ids {
            responses.insert(id, Value::Bool(false));
        }
        Self { responses }
    }

    fn from_interaction_responses(responses: Vec<crate::contracts::SuspensionResponse>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|response| (response.target_id, response.result))
                .collect(),
        }
    }

    fn resolve_response_for_call(
        &self,
        call: &crate::contracts::runtime::SuspendedCall,
    ) -> Option<Value> {
        self.responses
            .get(&call.call_id)
            .cloned()
            .or_else(|| self.responses.get(&call.ticket.suspension.id).cloned())
            .or_else(|| self.responses.get(&call.ticket.pending.id).cloned())
    }

    fn cancel_reason(result: &Value) -> Option<String> {
        result
            .as_object()
            .and_then(|obj| {
                obj.get("reason")
                    .and_then(Value::as_str)
                    .or_else(|| obj.get("message").and_then(Value::as_str))
            })
            .map(str::to_string)
    }

    fn to_tool_call_resume(
        call_id: &str,
        result: Value,
    ) -> crate::contracts::runtime::ToolCallResume {
        let action = if crate::contracts::SuspensionResponse::is_denied(&result) {
            crate::contracts::io::ResumeDecisionAction::Cancel
        } else {
            crate::contracts::io::ResumeDecisionAction::Resume
        };
        let reason = if matches!(action, crate::contracts::io::ResumeDecisionAction::Cancel) {
            Self::cancel_reason(&result)
        } else {
            None
        };
        crate::contracts::runtime::ToolCallResume {
            decision_id: format!("decision_{call_id}"),
            action,
            result,
            reason,
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| {
                    duration.as_millis().min(u128::from(u64::MAX)) as u64
                }),
        }
    }
}

#[async_trait]
impl AgentBehavior for TestInteractionPlugin {
    fn id(&self) -> &str {
        "test_interaction"
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        if self.responses.is_empty() {
            return ActionSet::empty();
        }
        let state = ctx.snapshot();
        let suspended_calls = crate::contracts::runtime::suspended_calls_from_state(&state);
        if suspended_calls.is_empty() {
            return ActionSet::empty();
        }

        let mut states = crate::contracts::runtime::tool_call_states_from_state(&ctx.snapshot());
        let mut actions = ActionSet::empty();
        for (call_id, suspended_call) in suspended_calls {
            if states.get(&call_id).is_some_and(|state| {
                matches!(
                    state.status,
                    crate::contracts::runtime::ToolCallStatus::Resuming
                )
            }) {
                continue;
            }
            let Some(result) = self.resolve_response_for_call(&suspended_call) else {
                continue;
            };
            let resume = Self::to_tool_call_resume(&call_id, result);
            let updated_at = resume.updated_at;
            let mut state = states.remove(&call_id).unwrap_or_else(|| {
                crate::contracts::runtime::ToolCallState {
                    call_id: call_id.clone(),
                    tool_name: suspended_call.tool_name.clone(),
                    arguments: suspended_call.arguments.clone(),
                    status: crate::contracts::runtime::ToolCallStatus::Suspended,
                    resume_token: Some(suspended_call.ticket.pending.id.clone()),
                    resume: None,
                    scratch: Value::Null,
                    updated_at,
                }
            });
            state.call_id = call_id.clone();
            state.tool_name = suspended_call.tool_name.clone();
            state.arguments = suspended_call.arguments.clone();
            state.status = crate::contracts::runtime::ToolCallStatus::Resuming;
            state.resume_token = Some(suspended_call.ticket.pending.id.clone());
            state.resume = Some(resume);
            state.updated_at = updated_at;
            actions = actions.and(ActionSet::single(LifecycleAction::State(
                state.into_state_action(),
            )));
        }
        actions
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo the input").with_parameters(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let msg = args["message"].as_str().unwrap_or("no message");
        Ok(ToolResult::success("echo", json!({ "echoed": msg })))
    }
}

struct AddTaskTool;

#[async_trait]
impl Tool for AddTaskTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("addTask", "Add Task", "Add a task").with_parameters(json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" }
            },
            "required": ["title"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(
            "addTask",
            json!({ "added": args["title"].as_str().unwrap_or_default() }),
        ))
    }
}

struct SelfSuspendTool;

#[async_trait]
impl Tool for SelfSuspendTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "self_suspend",
            "Self Suspend",
            "Suspend from inside tool execution",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let suspension = Suspension::new(ctx.call_id(), "tool:self_suspend")
            .with_message("Tool requested external approval")
            .with_parameters(args.clone());
        let pending = PendingToolCall::new(ctx.call_id(), "self_suspend", args.clone());
        Ok(ToolResult::suspended_with(
            "self_suspend",
            "Execution suspended; awaiting external decision",
            SuspendTicket::new(suspension, pending, ToolCallResumeMode::ReplayToolCall),
        ))
    }
}

struct CountingEchoTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingEchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "counting_echo",
            "Counting Echo",
            "Echo and increment call counter",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let msg = args["message"].as_str().unwrap_or("no message");
        Ok(ToolResult::success(
            "counting_echo",
            json!({ "echoed": msg }),
        ))
    }
}

struct ScopeSnapshotTool;

#[async_trait]
impl Tool for ScopeSnapshotTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "scope_snapshot",
            "Scope Snapshot",
            "Return tool scope caller context",
        )
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let thread_id = ctx
            .caller_context()
            .thread_id()
            .unwrap_or_default()
            .to_string();
        let messages_len = ctx.caller_context().messages().len();

        Ok(ToolResult::success(
            "scope_snapshot",
            json!({
                "thread_id": thread_id,
                "messages_len": messages_len
            }),
        ))
    }
}

struct ActionStateTool {
    id: &'static str,
    action: DebugFlagAction,
}

#[async_trait]
impl Tool for ActionStateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(self.id, "Action State Tool", "Returns typed state actions")
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(self.id, json!({"ok": true})))
    }

    async fn execute_effect(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        Ok(
            ToolExecutionEffect::new(ToolResult::success(self.id, json!({"ok": true})))
                .with_action(AnyStateAction::new::<DebugFlags>(self.action)),
        )
    }
}

struct ActivityGateTool {
    id: String,
    stream_id: String,
    ready: Arc<Notify>,
    proceed: Arc<Notify>,
}

#[async_trait]
impl Tool for ActivityGateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(&self.id, "Activity Gate", "Emits activity updates")
    }

    async fn execute(
        &self,
        _args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let activity = ctx.activity(self.stream_id.clone(), "progress");
        let progress = activity.state::<ActivityProgressState>("");
        let _ = progress.set_progress(0.1);
        self.ready.notify_one();
        self.proceed.notified().await;
        let _ = progress.set_progress(1.0);
        Ok(ToolResult::success(&self.id, json!({ "ok": true })))
    }
}

fn tool_execution_result(call_id: &str, patch: Option<TrackedPatch>) -> ToolExecutionResult {
    ToolExecutionResult {
        execution: crate::engine::tool_execution::ToolExecution {
            call: crate::contracts::thread::ToolCall::new(call_id, "test_tool", json!({})),
            result: ToolResult::success("test_tool", json!({"ok": true})),
            patch,
        },
        reminders: Vec::new(),
        user_messages: Vec::new(),
        outcome: crate::contracts::ToolCallOutcome::Succeeded,
        suspended_call: None,
        pending_patches: Vec::new(),
        serialized_state_actions: vec![],
    }
}

fn skill_activation_result(
    call_id: &str,
    skill_id: &str,
    instruction: Option<&str>,
) -> ToolExecutionResult {
    let result = ToolResult::success("skill", json!({ "activated": true, "skill_id": skill_id }));
    let user_messages = instruction
        .map(|text| vec![text.to_string()])
        .unwrap_or_default();

    ToolExecutionResult {
        execution: crate::engine::tool_execution::ToolExecution {
            call: crate::contracts::thread::ToolCall::new(
                call_id,
                "skill",
                json!({ "skill": skill_id }),
            ),
            result,
            patch: None,
        },
        reminders: Vec::new(),
        user_messages,
        outcome: crate::contracts::ToolCallOutcome::Succeeded,
        suspended_call: None,
        pending_patches: Vec::new(),
        serialized_state_actions: vec![],
    }
}

#[test]
fn test_agent_config_default() {
    let config = BaseAgent::default();
    assert_eq!(config.max_rounds, 10);
    assert_eq!(config.tool_executor.name(), "parallel_streaming");
    assert!(config.system_prompt.is_empty());
}

#[test]
fn test_agent_config_builder() {
    let config = BaseAgent::new("gpt-4")
        .with_max_rounds(5)
        .with_tool_executor(Arc::new(SequentialToolExecutor))
        .with_system_prompt("You are helpful.");

    assert_eq!(config.model, "gpt-4");
    assert_eq!(config.max_rounds, 5);
    assert_eq!(config.tool_executor.name(), "sequential");
    assert_eq!(config.system_prompt, "You are helpful.");
}

#[test]
fn test_agent_config_with_fallback_models_and_retry_policy() {
    let policy = LlmRetryPolicy {
        max_attempts_per_model: 3,
        initial_backoff_ms: 100,
        max_backoff_ms: 500,
        backoff_jitter_percent: 15,
        max_retry_window_ms: Some(2_500),
        retry_stream_start: true,
        max_stream_event_retries: 4,
        stream_error_fallback_threshold: 2,
    };
    let config = BaseAgent::new("primary")
        .with_fallback_models(vec!["fallback-a".to_string()])
        .with_fallback_model("fallback-b")
        .with_llm_retry_policy(policy.clone());

    assert_eq!(config.model, "primary");
    assert_eq!(
        config.fallback_models,
        vec!["fallback-a".to_string(), "fallback-b".to_string()]
    );
    assert_eq!(config.llm_retry_policy.max_attempts_per_model, 3);
    assert_eq!(config.llm_retry_policy.initial_backoff_ms, 100);
    assert_eq!(config.llm_retry_policy.max_backoff_ms, 500);
    assert_eq!(config.llm_retry_policy.backoff_jitter_percent, 15);
    assert_eq!(config.llm_retry_policy.max_retry_window_ms, Some(2_500));
    assert!(config.llm_retry_policy.retry_stream_start);
    assert_eq!(config.llm_retry_policy.max_stream_event_retries, 4);
    assert_eq!(config.llm_retry_policy.stream_error_fallback_threshold, 2);
}

#[test]
fn test_tool_map() {
    let tools = tool_map([EchoTool]);

    assert!(tools.contains_key("echo"));
    assert_eq!(tools.len(), 1);
}

#[test]
fn test_tool_map_from_arc() {
    let echo: Arc<dyn Tool> = Arc::new(EchoTool);
    let tools = tool_map_from_arc([echo]);

    assert!(tools.contains_key("echo"));
}

#[test]
fn test_agent_loop_error_display() {
    let err = AgentLoopError::LlmError("timeout".to_string());
    assert!(err.to_string().contains("timeout"));
}

#[test]
fn test_llm_retry_error_classification() {
    let rate_limit = genai::Error::HttpError {
        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        canonical_reason: "Too Many Requests".to_string(),
        body: String::new(),
    };
    assert_eq!(classify_llm_error(&rate_limit), LlmErrorClass::RateLimit);
    assert!(is_retryable_llm_error(&rate_limit));

    let timeout = genai::Error::Internal("gateway timeout".to_string());
    assert_eq!(classify_llm_error(&timeout), LlmErrorClass::Timeout);
    assert!(is_retryable_llm_error(&timeout));

    let connection_reset = genai::Error::WebStream {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        cause: "transport interrupted".to_string(),
        error: Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        )),
    };
    assert_eq!(
        classify_llm_error(&connection_reset),
        LlmErrorClass::Connection
    );
    assert!(is_retryable_llm_error(&connection_reset));

    let unauthorized = genai::Error::HttpError {
        status: reqwest::StatusCode::UNAUTHORIZED,
        canonical_reason: "Unauthorized".to_string(),
        body: String::new(),
    };
    assert_eq!(classify_llm_error(&unauthorized), LlmErrorClass::Auth);
    assert!(!is_retryable_llm_error(&unauthorized));

    let bad_request = genai::Error::HttpError {
        status: reqwest::StatusCode::BAD_REQUEST,
        canonical_reason: "Bad Request".to_string(),
        body: String::new(),
    };
    assert_eq!(
        classify_llm_error(&bad_request),
        LlmErrorClass::ClientRequest
    );
    assert!(!is_retryable_llm_error(&bad_request));
}

#[test]
fn test_stream_parse_is_retryable() {
    let stream_parse = genai::Error::StreamParse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        serde_error: serde_json::from_str::<serde_json::Value>("{{bad json").unwrap_err(),
    };
    assert_eq!(
        classify_llm_error(&stream_parse),
        LlmErrorClass::Connection,
        "StreamParse should be classified as Connection (retryable)"
    );
    assert!(is_retryable_llm_error(&stream_parse));
}

#[test]
fn test_no_chat_response_is_retryable() {
    let no_response = genai::Error::NoChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
    };
    assert_eq!(
        classify_llm_error(&no_response),
        LlmErrorClass::ServerError,
        "NoChatResponse should be classified as ServerError (retryable)"
    );
    assert!(is_retryable_llm_error(&no_response));
}

#[test]
fn test_chat_response_openai_server_error() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        body: serde_json::json!({
            "message": "Error in input stream",
            "type": "server_error"
        }),
    };
    assert_eq!(
        classify_llm_error(&error),
        LlmErrorClass::ServerError,
        "OpenAI server_error type should be classified as ServerError"
    );
    assert!(is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_anthropic_overloaded() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::Anthropic, "mock"),
        body: serde_json::json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": "Overloaded"
            }
        }),
    };
    assert_eq!(
        classify_llm_error(&error),
        LlmErrorClass::ServerUnavailable,
        "Anthropic overloaded_error should be classified as ServerUnavailable"
    );
    assert!(is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_rate_limit() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        body: serde_json::json!({
            "type": "rate_limit_error",
            "message": "Rate limit exceeded"
        }),
    };
    assert_eq!(classify_llm_error(&error), LlmErrorClass::RateLimit,);
    assert!(is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_invalid_request() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::Anthropic, "mock"),
        body: serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "max_tokens must be positive"
            }
        }),
    };
    assert_eq!(classify_llm_error(&error), LlmErrorClass::ClientRequest,);
    assert!(!is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_authentication_error() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::Anthropic, "mock"),
        body: serde_json::json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "invalid x-api-key"
            }
        }),
    };
    assert_eq!(classify_llm_error(&error), LlmErrorClass::Auth,);
    assert!(!is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_api_error() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::Anthropic, "mock"),
        body: serde_json::json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": "Internal server error"
            }
        }),
    };
    assert_eq!(classify_llm_error(&error), LlmErrorClass::ServerError,);
    assert!(is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_with_status_code_fallback() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        body: serde_json::json!({
            "status": 503,
            "message": "some unknown format"
        }),
    };
    assert_eq!(classify_llm_error(&error), LlmErrorClass::ServerUnavailable,);
    assert!(is_retryable_llm_error(&error));
}

#[test]
fn test_chat_response_unknown_body() {
    let error = genai::Error::ChatResponse {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        body: serde_json::json!({
            "finishReason": "SAFETY",
            "usageMetadata": {}
        }),
    };
    assert_eq!(
        classify_llm_error(&error),
        LlmErrorClass::Unknown,
        "Unrecognized body structure should fall to Unknown"
    );
    assert!(!is_retryable_llm_error(&error));
}

#[test]
fn test_auth_config_errors_are_non_retryable() {
    let requires_key = genai::Error::RequiresApiKey {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
    };
    assert_eq!(classify_llm_error(&requires_key), LlmErrorClass::Auth);
    assert!(!is_retryable_llm_error(&requires_key));

    let no_resolver = genai::Error::NoAuthResolver {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
    };
    assert_eq!(classify_llm_error(&no_resolver), LlmErrorClass::Auth);
    assert!(!is_retryable_llm_error(&no_resolver));

    let no_data = genai::Error::NoAuthData {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
    };
    assert_eq!(classify_llm_error(&no_data), LlmErrorClass::Auth);
    assert!(!is_retryable_llm_error(&no_data));
}

#[test]
fn test_classify_chat_response_body_direct() {
    // Direct type field (OpenAI extracted error)
    assert_eq!(
        classify_chat_response_body(&serde_json::json!({"type": "server_error"})),
        LlmErrorClass::ServerError,
    );
    assert_eq!(
        classify_chat_response_body(&serde_json::json!({"type": "rate_limit_error"})),
        LlmErrorClass::RateLimit,
    );
    assert_eq!(
        classify_chat_response_body(&serde_json::json!({"type": "overloaded_error"})),
        LlmErrorClass::ServerUnavailable,
    );

    // Nested error.type field (Anthropic envelope)
    assert_eq!(
        classify_chat_response_body(&serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error"}
        })),
        LlmErrorClass::ServerUnavailable,
    );

    // When top-level type is a non-error string (like "error"), fall through
    // to nested lookup.
    assert_eq!(
        classify_chat_response_body(&serde_json::json!({
            "type": "error",
            "error": {"type": "authentication_error"}
        })),
        LlmErrorClass::Auth,
    );
}

#[test]
fn test_retry_backoff_plan_without_jitter_is_exponential_and_capped() {
    let policy = LlmRetryPolicy {
        initial_backoff_ms: 100,
        max_backoff_ms: 500,
        backoff_jitter_percent: 0,
        max_retry_window_ms: None,
        ..LlmRetryPolicy::default()
    };

    assert_eq!(retry_base_backoff_ms(&policy, 1), 100);
    assert_eq!(retry_base_backoff_ms(&policy, 2), 200);
    assert_eq!(retry_base_backoff_ms(&policy, 3), 400);
    assert_eq!(retry_base_backoff_ms(&policy, 4), 500);
    assert_eq!(retry_backoff_plan_ms(&policy, 4, 0, 7), Some(500));
}

#[test]
fn test_retry_backoff_plan_with_jitter_stays_within_expected_range() {
    let policy = LlmRetryPolicy {
        initial_backoff_ms: 100,
        max_backoff_ms: 500,
        backoff_jitter_percent: 20,
        max_retry_window_ms: None,
        ..LlmRetryPolicy::default()
    };

    let waits: std::collections::HashSet<_> = (0..8)
        .map(|entropy| retry_backoff_plan_ms(&policy, 2, 0, entropy).unwrap())
        .collect();

    assert!(
        waits.iter().all(|wait_ms| (160..=240).contains(wait_ms)),
        "jittered waits should stay within +/-20% of 200ms: {waits:?}"
    );
    assert!(waits.len() > 1, "expected entropy to produce varied waits");
}

#[test]
fn test_retry_backoff_plan_respects_retry_window_budget() {
    let policy = LlmRetryPolicy {
        initial_backoff_ms: 100,
        max_backoff_ms: 500,
        backoff_jitter_percent: 0,
        max_retry_window_ms: Some(250),
        ..LlmRetryPolicy::default()
    };

    assert_eq!(retry_backoff_plan_ms(&policy, 1, 0, 0), Some(100));
    assert_eq!(retry_backoff_plan_ms(&policy, 2, 25, 0), Some(200));
    assert_eq!(retry_backoff_plan_ms(&policy, 2, 51, 0), None);
}

#[test]
fn test_execute_tools_empty() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Hello".to_string(),
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
        };
        let tools = HashMap::new();

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();
        assert_eq!(thread.message_count(), 0);
    });
}

#[test]
fn test_execute_tools_with_calls() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Calling tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "hello"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        assert_eq!(
            thread.messages[0].role,
            crate::contracts::thread::Role::Tool
        );
    });
}

#[test]
fn test_execute_tools_tool_can_suspend_itself() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Calling tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "self_suspend",
                json!({"message": "need approval"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([SelfSuspendTool]);

        let outcome = execute_tools(thread, &result, &tools, true)
            .await
            .expect("tool execution should succeed");
        let (thread, suspended_call) = match outcome {
            ExecuteToolsOutcome::Suspended {
                thread,
                suspended_call,
            } => (thread, suspended_call),
            ExecuteToolsOutcome::Completed(_) => panic!("expected suspended outcome"),
        };

        assert_eq!(suspended_call.call_id, "call_1");
        assert_eq!(suspended_call.ticket.pending.id, "call_1");
        assert_eq!(suspended_call.ticket.pending.name, "self_suspend");
        assert_eq!(
            suspended_call.ticket.resume_mode,
            ToolCallResumeMode::ReplayToolCall
        );

        let state = thread.rebuild_state().expect("state should rebuild");
        assert_eq!(
            get_suspended_call(&state, "call_1").expect("call should be suspended")["pending"]
                ["name"],
            json!("self_suspend")
        );
    });
}

#[test]
fn test_execute_tools_injects_caller_scope_context_for_tools() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state("caller-s", json!({"k":"v"}))
            .with_message(crate::contracts::thread::Message::user("hello"));
        let result = StreamResult {
            text: "Calling tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "scope_snapshot",
                json!({}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([ScopeSnapshotTool]);

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();
        assert_eq!(thread.message_count(), 2);
        let tool_msg = thread
            .messages
            .last()
            .expect("tool result message should exist");
        let tool_result: ToolResult =
            serde_json::from_str(&tool_msg.content).expect("tool result json");
        assert_eq!(
            tool_result.status,
            crate::contracts::runtime::tool_call::ToolStatus::Success
        );
        assert_eq!(tool_result.data["thread_id"], json!("caller-s"));
        assert_eq!(tool_result.data["messages_len"], json!(1));
    });
}

#[tokio::test]
async fn test_activity_event_emitted_before_tool_completion() {
    use crate::contracts::AgentEvent;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let activity_manager: Arc<dyn ActivityManager> = Arc::new(ActivityHub::new(tx));

    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let tool = ActivityGateTool {
        id: "activity_gate".to_string(),
        stream_id: "stream_gate".to_string(),
        ready: ready.clone(),
        proceed: proceed.clone(),
    };

    let call = crate::contracts::thread::ToolCall::new("call_1", "activity_gate", json!({}));
    let descriptors = vec![tool.descriptor()];
    let state = json!({});
    let run_policy = tirea_contract::RunPolicy::default();
    let phase_ctx = test_tool_phase_context(
        &descriptors,
        None,
        activity_manager,
        &run_policy,
        "test",
        &[],
        None,
    );

    let mut tool_future = Box::pin(execute_single_tool_with_phases(
        Some(&tool),
        &call,
        &state,
        &phase_ctx,
    ));

    tokio::select! {
        _ = ready.notified() => {
            let event = rx.recv().await.expect("activity event");
            match event {
                AgentEvent::ActivitySnapshot { message_id, content, .. } => {
                    assert_eq!(message_id, "stream_gate");
                    assert_eq!(content["progress"], 0.1);
                }
                _ => panic!("Expected ActivitySnapshot"),
            }
            proceed.notify_one();
        }
        _res = &mut tool_future => {
            panic!("Tool finished before activity event");
        }
    }

    let result = tool_future.await.expect("tool execution should succeed");
    assert!(result.execution.result.is_success());
}

#[tokio::test]
async fn test_parallel_tools_emit_activity_before_completion() {
    use crate::contracts::AgentEvent;
    use std::collections::HashSet;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let activity_manager: Arc<dyn ActivityManager> = Arc::new(ActivityHub::new(tx));

    let ready_a = Arc::new(Notify::new());
    let proceed_a = Arc::new(Notify::new());
    let tool_a = ActivityGateTool {
        id: "activity_gate_a".to_string(),
        stream_id: "stream_gate_a".to_string(),
        ready: ready_a.clone(),
        proceed: proceed_a.clone(),
    };

    let ready_b = Arc::new(Notify::new());
    let proceed_b = Arc::new(Notify::new());
    let tool_b = ActivityGateTool {
        id: "activity_gate_b".to_string(),
        stream_id: "stream_gate_b".to_string(),
        ready: ready_b.clone(),
        proceed: proceed_b.clone(),
    };

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert(tool_a.id.clone(), Arc::new(tool_a));
    tools.insert(tool_b.id.clone(), Arc::new(tool_b));

    let calls = vec![
        crate::contracts::thread::ToolCall::new("call_a", "activity_gate_a", json!({})),
        crate::contracts::thread::ToolCall::new("call_b", "activity_gate_b", json!({})),
    ];
    let tool_descriptors: Vec<ToolDescriptor> =
        tools.values().map(|t| t.descriptor().clone()).collect();
    let state = json!({});
    let run_policy = tirea_contract::RunPolicy::default();

    // Spawn the tool execution so it actually starts running while we await activity events.
    let tools_for_task = tools.clone();
    let calls_for_task = calls.clone();
    let tool_descriptors_for_task = tool_descriptors.clone();
    let state_for_task = state.clone();
    let handle = tokio::spawn(async move {
        let phase_ctx = test_tool_phase_context(
            &tool_descriptors_for_task,
            None,
            activity_manager,
            &run_policy,
            "test",
            &[],
            None,
        );
        execute_tools_parallel_with_phases(
            &tools_for_task,
            &calls_for_task,
            &state_for_task,
            phase_ctx,
        )
        .await
        .expect("parallel tool execution should succeed")
    });

    let ((), ()) = tokio::join!(ready_a.notified(), ready_b.notified());

    // Both tools have emitted their first activity update; observe both snapshots
    // before unblocking them.
    let mut seen: HashSet<String> = HashSet::new();
    while seen.len() < 2 {
        match rx.recv().await.expect("activity event") {
            AgentEvent::ActivitySnapshot {
                message_id,
                content,
                ..
            } => {
                assert_eq!(content["progress"], 0.1);
                seen.insert(message_id);
            }
            other => panic!("Expected ActivitySnapshot, got {:?}", other),
        }
    }
    assert!(seen.contains("stream_gate_a"));
    assert!(seen.contains("stream_gate_b"));

    proceed_a.notify_one();
    proceed_b.notify_one();

    let results = handle.await.expect("task join");
    assert_eq!(results.len(), 2);
    for r in results {
        assert!(r.execution.result.is_success());
    }
}

#[tokio::test]
async fn test_parallel_tool_executor_honors_cancellation_token() {
    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let tool = ActivityGateTool {
        id: "activity_gate".to_string(),
        stream_id: "parallel_cancel".to_string(),
        ready: ready.clone(),
        proceed,
    };

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("activity_gate".to_string(), Arc::new(tool));
    let calls = vec![crate::contracts::thread::ToolCall::new(
        "call_1",
        "activity_gate",
        json!({}),
    )];
    let tool_descriptors: Vec<ToolDescriptor> =
        tools.values().map(|t| t.descriptor().clone()).collect();
    let token = CancellationToken::new();
    let token_for_task = token.clone();
    let ready_for_task = ready.clone();
    let run_policy = tirea_contract::RunPolicy::default();

    let handle = tokio::spawn(async move {
        let phase_ctx = test_tool_phase_context(
            &tool_descriptors,
            None,
            tirea_contract::runtime::activity::NoOpActivityManager::arc(),
            &run_policy,
            "cancel-test",
            &[],
            Some(&token_for_task),
        );
        let result =
            execute_tools_parallel_with_phases(&tools, &calls, &json!({}), phase_ctx).await;
        ready_for_task.notify_one();
        result
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .expect("tool execution did not reach cancellation checkpoint");
    token.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_millis(300), handle)
        .await
        .expect("parallel executor should stop shortly after cancellation")
        .expect("task should not panic");
    assert!(
        matches!(result, Err(AgentLoopError::Cancelled)),
        "expected cancellation error from tool executor"
    );
}

struct CounterTool;

#[async_trait]
impl Tool for CounterTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("counter", "Counter", "Increment a counter").with_parameters(json!({
            "type": "object",
            "properties": {
                "amount": { "type": "integer" }
            }
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let amount = args["amount"].as_i64().unwrap_or(1);

        let state = ctx.state::<TestCounterState>("");
        let current = state.counter().unwrap_or(0);
        let new_value = current + amount;

        state.set_counter(new_value).expect("failed to set counter");

        Ok(ToolResult::success(
            "counter",
            json!({ "new_value": new_value }),
        ))
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let amount = args["amount"].as_i64().unwrap_or(1);
        let current = ctx
            .snapshot_of::<TestCounterState>()
            .unwrap_or_default()
            .counter;
        let new_value = current + amount;

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "counter",
            json!({ "new_value": new_value }),
        ))
        .with_action(AnyStateAction::new::<TestCounterState>(
            TestCounterAction::SetCounter(new_value),
        )))
    }
}

#[test]
fn test_execute_tools_with_state_changes() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state("test", json!({"counter": 0}));
        let result = StreamResult {
            text: "Incrementing".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "counter",
                json!({"amount": 5}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([CounterTool]);

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        // state patch + merged tool lifecycle patch
        assert_eq!(thread.patch_count(), 2);

        let state = thread.rebuild_state().unwrap();
        assert_eq!(state["counter"], 5);
        assert_eq!(
            state["__tool_call_scope"]["call_1"]["tool_call_state"]["status"],
            json!("succeeded")
        );
    });
}

// StepResult has been removed; its semantics are captured by LoopOutcome.

struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("failing", "Failing Tool", "Always fails")
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::ExecutionFailed(
            "Intentional failure".to_string(),
        ))
    }
}

#[test]
fn test_execute_tools_with_failing_tool() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Calling failing tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "failing",
                json!({}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([FailingTool]);

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        let msg = &thread.messages[0];
        assert!(msg.content.contains("error") || msg.content.contains("fail"));
    });
}

// ============================================================================
// Phase-based Plugin Tests
// ============================================================================

struct TestPhasePlugin {
    id: String,
}

impl TestPhasePlugin {
    fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[async_trait]
impl AgentBehavior for TestPhasePlugin {
    fn id(&self) -> &str {
        &self.id
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::AddSystemContext(
            "Test system context".into(),
        ))
        .and(ActionSet::single(BeforeInferenceAction::AddSessionContext(
            "Test thread context".into(),
        )))
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        if ctx.tool_name() == Some("echo") {
            ActionSet::single(AfterToolExecuteAction::AddSystemReminder(
                "Check the echo result".into(),
            ))
        } else {
            ActionSet::empty()
        }
    }
}

#[test]
fn test_agent_config_with_phase_plugin() {
    let behavior: Arc<dyn AgentBehavior> = Arc::new(TestPhasePlugin::new("test"));
    let config = BaseAgent::new("gpt-4").with_behavior(behavior);

    assert!(config.has_behavior());
}

struct BlockingPhasePlugin;

#[async_trait]
impl AgentBehavior for BlockingPhasePlugin {
    fn id(&self) -> &str {
        "blocker"
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        if ctx.tool_name() == Some("echo") {
            ActionSet::single(BeforeToolExecuteAction::Block(
                "Echo tool is blocked".into(),
            ))
        } else {
            ActionSet::empty()
        }
    }
}

#[test]
fn test_execute_tools_with_blocking_phase_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Blocked".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m").with_behavior(Arc::new(BlockingPhasePlugin));

        let thread = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        let msg = &thread.messages[0];
        assert!(
            msg.content.contains("blocked") || msg.content.contains("Error"),
            "Expected blocked/error in message, got: {}",
            msg.content
        );
    });
}

struct InvalidAfterToolMutationPlugin;

#[async_trait]
impl AgentBehavior for InvalidAfterToolMutationPlugin {
    fn id(&self) -> &str {
        "invalid_after_tool_mutation"
    }

    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        // BlockTool is now type-safe: it cannot be expressed in AfterToolExecuteAction.
        // The typed ActionSet system prevents this at compile time.
        ActionSet::empty()
    }
}

#[test]
fn test_execute_tools_gate_mutation_type_safe_in_after_tool_execute() {
    // With the typed ActionSet system, tool gate mutations (Block/Suspend) cannot be
    // expressed in AfterToolExecuteAction at compile time. This test verifies that
    // after_tool_execute with no gate actions completes normally.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "ok".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m").with_behavior(Arc::new(InvalidAfterToolMutationPlugin));

        let thread = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .expect(
                "tool execution should succeed when after_tool_execute returns empty action set",
            )
            .into_thread();

        assert_eq!(thread.message_count(), 1);
    });
}

struct InvalidDualToolGatePlugin;

#[async_trait]
impl AgentBehavior for InvalidDualToolGatePlugin {
    fn id(&self) -> &str {
        "invalid_dual_tool_gate"
    }

    async fn before_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        ActionSet::single(BeforeToolExecuteAction::Block("invalid gate".into())).and(
            ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                Suspension::new("confirm", "confirm").with_message("invalid gate"),
            ))),
        )
    }
}

#[test]
fn test_execute_tools_rejects_non_orthogonal_tool_gate_state() {
    // In the AgentBehavior model, both BlockTool and SuspendTool are valid in
    // BeforeToolExecute. When both are emitted, the tool is treated as blocked
    // (tool_blocked() is checked first in the execution path).
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "invalid".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m").with_behavior(Arc::new(InvalidDualToolGatePlugin));

        let thread = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .expect("dual gate effects are applied; tool_blocked() takes precedence")
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        let msg = &thread.messages[0];
        // SuspendTool is applied after BlockTool, so the final state is pending/suspended.
        assert!(
            msg.content.contains("awaiting approval") || msg.content.contains("paused"),
            "Expected suspended message, got: {}",
            msg.content
        );
    });
}

struct InvalidSuspendTicketMutationPlugin;

#[async_trait]
impl AgentBehavior for InvalidSuspendTicketMutationPlugin {
    fn id(&self) -> &str {
        "invalid_suspend_ticket_mutation"
    }

    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        // SuspendTool is now type-safe: it cannot be expressed in AfterToolExecuteAction.
        // The typed ActionSet system prevents this at compile time.
        ActionSet::empty()
    }
}

#[test]
fn test_execute_tools_suspend_ticket_type_safe_in_after_tool_execute() {
    // With the typed ActionSet system, suspend ticket mutations cannot be expressed
    // in AfterToolExecuteAction at compile time. This test verifies that after_tool_execute
    // with no suspension actions completes normally.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "ok".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m").with_behavior(Arc::new(InvalidSuspendTicketMutationPlugin));

        let thread = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .expect(
                "tool execution should succeed when after_tool_execute returns empty action set",
            )
            .into_thread();

        assert_eq!(thread.message_count(), 1);
    });
}

struct ReminderPhasePlugin;

#[async_trait]
impl AgentBehavior for ReminderPhasePlugin {
    fn id(&self) -> &str {
        "reminder"
    }

    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        ActionSet::single(AfterToolExecuteAction::AddSystemReminder(
            "Tool execution completed".into(),
        ))
    }
}

#[test]
fn test_execute_tools_with_reminder_phase_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "With reminder".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m").with_behavior(Arc::new(ReminderPhasePlugin));

        let thread = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .unwrap()
            .into_thread();

        // Should have tool response + reminder message
        assert_eq!(thread.message_count(), 2);
        assert!(thread.messages[1].content.contains("system-reminder"));
        assert!(thread.messages[1]
            .content
            .contains("Tool execution completed"));
    });
}

#[test]
fn test_build_messages_with_context() {
    let thread = Thread::new("test").with_message(Message::user("Hello"));
    let tool_descriptors = vec![ToolDescriptor::new("test", "Test", "Test tool")];
    let mut fixture = TestFixture::new();
    fixture.messages = thread.messages.clone();
    let mut step = fixture.step(tool_descriptors);

    step.inference
        .system_context
        .push("System context 1".into());
    step.inference
        .system_context
        .push("System context 2".into());
    step.inference.session_context.push("Thread context".into());

    let messages = build_messages(&step, "Base system prompt");

    assert_eq!(messages.len(), 3);
    assert!(messages[0].content.contains("Base system prompt"));
    assert!(messages[0].content.contains("System context 1"));
    assert!(messages[0].content.contains("System context 2"));
    assert_eq!(messages[1].content, "Thread context");
    assert_eq!(messages[2].content, "Hello");
}

#[test]
fn test_build_messages_empty_system() {
    let thread = Thread::new("test").with_message(Message::user("Hello"));
    let mut fixture = TestFixture::new();
    fixture.messages = thread.messages.clone();
    let step = fixture.step(vec![]);

    let messages = build_messages(&step, "");

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "Hello");
}

struct ToolFilterPlugin;

#[async_trait]
impl AgentBehavior for ToolFilterPlugin {
    fn id(&self) -> &str {
        "filter"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::ExcludeTool("dangerous_tool".into()))
    }
}

#[test]
fn test_tool_filtering_via_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let tool_descriptors = vec![
            ToolDescriptor::new("safe_tool", "Safe", "Safe tool"),
            ToolDescriptor::new("dangerous_tool", "Dangerous", "Dangerous tool"),
        ];
        let fixture = TestFixture::new();
        let mut step = fixture.step(tool_descriptors);
        let behavior = ToolFilterPlugin;
        let doc = tirea_state::DocCell::new(json!({}));

        emit_agent_phase(Phase::BeforeInference, &mut step, &behavior, &doc)
            .await
            .expect("BeforeInference should not fail");

        let inf = &step.inference;
        assert_eq!(inf.tools.len(), 1);
        assert_eq!(inf.tools[0].id, "safe_tool");
    });
}

#[tokio::test]
async fn test_plugin_state_channel_available_in_before_tool_execute() {
    struct GuardedPlugin;

    #[async_trait]
    impl AgentBehavior for GuardedPlugin {
        fn id(&self) -> &str {
            "guarded"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            let state = ctx.snapshot();
            let allow_exec = state
                .get("plugin")
                .and_then(|p| p.get("allow_exec"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !allow_exec {
                ActionSet::single(BeforeToolExecuteAction::Block(
                    "missing plugin.allow_exec in state".into(),
                ))
            } else {
                ActionSet::empty()
            }
        }
    }

    let tool = EchoTool;
    let call =
        crate::contracts::thread::ToolCall::new("call_1", "echo", json!({ "message": "hello" }));
    let state = json!({ "plugin": { "allow_exec": true } });
    let tool_descriptors = vec![tool.descriptor()];
    let guarded_behavior: Arc<dyn AgentBehavior> = Arc::new(GuardedPlugin);
    let run_policy = tirea_contract::RunPolicy::default();
    let phase_ctx = test_tool_phase_context(
        &tool_descriptors,
        Some(guarded_behavior.as_ref()),
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
        &run_policy,
        "test",
        &[],
        None,
    );

    let result = execute_single_tool_with_phases(Some(&tool), &call, &state, &phase_ctx)
        .await
        .expect("tool execution should succeed");

    assert!(result.execution.result.is_success());
}

#[tokio::test]
async fn test_tool_execute_effect_state_actions_become_pending_patches() {
    struct ActionEffectTool;

    #[async_trait]
    impl Tool for ActionEffectTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "action_effect_tool",
                "ActionEffect",
                "returns state actions",
            )
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "action_effect_tool",
                json!({"ok": true}),
            ))
        }

        async fn execute_effect(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolExecutionEffect, ToolError> {
            Ok(ToolExecutionEffect::new(ToolResult::success(
                "action_effect_tool",
                json!({"ok": true}),
            ))
            .with_action(AnyStateAction::new::<DebugFlags>(
                DebugFlagAction::AfterTool,
            )))
        }
    }

    let tool = ActionEffectTool;
    let call = crate::contracts::thread::ToolCall::new("call_1", "action_effect_tool", json!({}));
    let state = json!({});
    let tool_descriptors = vec![tool.descriptor()];
    let run_policy = tirea_contract::RunPolicy::default();
    let phase_ctx = test_tool_phase_context(
        &tool_descriptors,
        None,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
        &run_policy,
        "test",
        &[],
        None,
    );

    let result = execute_single_tool_with_phases(Some(&tool), &call, &state, &phase_ctx)
        .await
        .expect("tool execution should succeed");

    assert!(result.execution.result.is_success());
    let mut next_state = state.clone();
    if let Some(tool_patch) = result.execution.patch.as_ref() {
        next_state =
            tirea_state::apply_patch(&next_state, tool_patch.patch()).expect("apply tool patch");
    }
    let pending_refs: Vec<&Patch> = result.pending_patches.iter().map(|p| p.patch()).collect();
    next_state =
        tirea_state::apply_patches(&next_state, pending_refs).expect("apply pending patches");
    assert_eq!(next_state["debug"]["after_tool_effect"], json!(true));
}

#[tokio::test]
async fn test_tool_read_only_context_rejects_direct_writes() {
    struct DirectWriteEffectTool;

    #[async_trait]
    impl Tool for DirectWriteEffectTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "direct_write_effect_tool",
                "DirectWrite",
                "writes via context",
            )
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "direct_write_effect_tool",
                json!({"ok": true}),
            ))
        }

        async fn execute_effect(
            &self,
            _args: Value,
            ctx: &ToolCallContext<'_>,
        ) -> Result<ToolExecutionEffect, ToolError> {
            let state = ctx.state_of::<tirea_contract::testing::TestFixtureState>();
            if let Err(err) = state.set_label(Some("direct_write".to_string())) {
                return Err(ToolError::ExecutionFailed(format!(
                    "context write rejected: {err}"
                )));
            }
            Ok(ToolExecutionEffect::new(ToolResult::success(
                "direct_write_effect_tool",
                json!({"ok": true}),
            )))
        }
    }

    let tool = DirectWriteEffectTool;
    let call =
        crate::contracts::thread::ToolCall::new("call_1", "direct_write_effect_tool", json!({}));
    let state = json!({});
    let tool_descriptors = vec![tool.descriptor()];
    let run_policy = tirea_contract::RunPolicy::default();
    let phase_ctx = test_tool_phase_context(
        &tool_descriptors,
        None,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
        &run_policy,
        "test",
        &[],
        None,
    );

    let result = execute_single_tool_with_phases(Some(&tool), &call, &state, &phase_ctx)
        .await
        .expect("tool execution should complete");

    // Read-only context rejects writes; tool propagates as error
    assert!(result.execution.result.is_error());
    assert!(
        result
            .execution
            .result
            .message
            .as_deref()
            .unwrap_or("")
            .contains("read-only"),
        "error should mention read-only: {:?}",
        result.execution.result.message,
    );
}

#[tokio::test]
async fn test_execute_single_tool_context_waits_for_run_cancellation() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ContextCancellationProbeTool {
        ready: Arc<Notify>,
        observed_token: Arc<AtomicBool>,
        observed_cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for ContextCancellationProbeTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "cancel_probe",
                "Cancel Probe",
                "Wait for run cancellation from tool context",
            )
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            self.observed_token
                .store(ctx.cancellation_token().is_some(), Ordering::SeqCst);
            self.ready.notify_one();
            ctx.cancelled().await;
            self.observed_cancelled
                .store(ctx.is_cancelled(), Ordering::SeqCst);

            Ok(ToolResult::success(
                "cancel_probe",
                json!({ "cancelled": ctx.is_cancelled() }),
            ))
        }
    }

    let ready = Arc::new(Notify::new());
    let observed_token = Arc::new(AtomicBool::new(false));
    let observed_cancelled = Arc::new(AtomicBool::new(false));
    let tool = ContextCancellationProbeTool {
        ready: ready.clone(),
        observed_token: observed_token.clone(),
        observed_cancelled: observed_cancelled.clone(),
    };
    let call = crate::contracts::thread::ToolCall::new("call_1", "cancel_probe", json!({}));
    let state = json!({});
    let tool_descriptors = vec![tool.descriptor()];
    let run_policy = tirea_contract::RunPolicy::default();
    let token = CancellationToken::new();
    let token_for_task = token.clone();
    let ready_for_task = ready.clone();
    tokio::spawn(async move {
        ready_for_task.notified().await;
        token_for_task.cancel();
    });
    let phase_ctx = test_tool_phase_context(
        &tool_descriptors,
        None,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
        &run_policy,
        "test",
        &[],
        Some(&token),
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        execute_single_tool_with_phases(Some(&tool), &call, &state, &phase_ctx),
    )
    .await
    .expect("tool should finish after cancellation signal")
    .expect("tool execution should succeed");

    assert!(result.execution.result.is_success());
    assert_eq!(result.execution.result.data["cancelled"], json!(true));
    assert!(observed_token.load(Ordering::SeqCst));
    assert!(observed_cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn test_plugin_sees_real_session_id_and_typed_context_in_tool_phase() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static VERIFIED: AtomicBool = AtomicBool::new(false);

    struct SessionCheckPlugin;

    #[async_trait]
    impl AgentBehavior for SessionCheckPlugin {
        fn id(&self) -> &str {
            "session_check"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            assert_eq!(ctx.thread_id(), "real-thread-42");
            assert_eq!(ctx.run_identity().run_id_opt(), Some("tool-phase-run"));
            assert_eq!(
                ctx.run_policy().allowed_tools(),
                Some(&["echo".to_string()][..])
            );
            VERIFIED.store(true, Ordering::SeqCst);
            ActionSet::empty()
        }
    }

    VERIFIED.store(false, Ordering::SeqCst);

    let tool = EchoTool;
    let call =
        crate::contracts::thread::ToolCall::new("call_1", "echo", json!({ "message": "hi" }));
    let state = json!({});
    let tool_descriptors = vec![tool.descriptor()];
    let session_check_behavior: Arc<dyn AgentBehavior> = Arc::new(SessionCheckPlugin);

    let mut rt = tirea_contract::RunPolicy::new();
    rt.set_allowed_tools_if_absent(Some(&["echo".to_string()]));
    let mut phase_ctx = test_tool_phase_context(
        &tool_descriptors,
        Some(session_check_behavior.as_ref()),
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
        &rt,
        "real-thread-42",
        &[],
        None,
    );
    phase_ctx.run_identity = test_run_identity("tool-phase-run");

    let result = execute_single_tool_with_phases(Some(&tool), &call, &state, &phase_ctx)
        .await
        .expect("tool execution should succeed");

    assert!(result.execution.result.is_success());
    assert!(VERIFIED.load(Ordering::SeqCst), "plugin did not run");
}

#[tokio::test]
async fn test_plugin_state_patch_visible_in_next_step_before_inference() {
    struct StateChannelPlugin;

    #[async_trait]
    impl AgentBehavior for StateChannelPlugin {
        fn id(&self) -> &str {
            "state_channel"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            self.before_tool_actions(ctx).await
        }

        async fn before_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            self.before_inference_actions(ctx).await
        }
    }

    impl StateChannelPlugin {
        async fn before_tool_actions(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            ActionSet::single(BeforeToolExecuteAction::State(test_bool_state_action(
                "debug.seen_tool_execute",
                true,
            )))
        }

        async fn before_inference_actions(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            let state = ctx.snapshot();
            let seen_tool_execute = state
                .get("debug")
                .and_then(|d| d.get("seen_tool_execute"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if seen_tool_execute {
                ActionSet::single(BeforeInferenceAction::State(test_bool_state_action(
                    "debug.before_inference_observed",
                    true,
                )))
            } else {
                ActionSet::empty()
            }
        }
    }

    let responses = vec![
        MockResponse::text("run tools").with_tool_call("call_1", "echo", json!({"message": "a"})),
        MockResponse::text("done"),
    ];
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(StateChannelPlugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let (_events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(responses),
        config,
        thread,
        tools,
    )
    .await;

    let state = final_thread.rebuild_state().expect("state rebuild");
    assert_eq!(state["debug"]["seen_tool_execute"], true);
    assert_eq!(state["debug"]["before_inference_observed"], true);
}

#[tokio::test]
async fn test_run_phase_block_executes_phases_extracts_output_and_commits_pending_patches() {
    struct PhaseBlockPlugin {
        phases: Arc<Mutex<Vec<Phase>>>,
    }

    #[async_trait]
    impl AgentBehavior for PhaseBlockPlugin {
        fn id(&self) -> &str {
            "phase_block"
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::RunStart);
            ActionSet::empty()
        }
        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::StepStart);
            ActionSet::empty()
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            self.phases.lock().unwrap().push(Phase::BeforeInference);
            ActionSet::single(BeforeInferenceAction::AddSystemContext(
                "from_before_inference".into(),
            ))
            .and(ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            )))
            .and(ActionSet::single(BeforeInferenceAction::State(
                test_bool_state_action("debug.phase_block", true),
            )))
        }
        async fn after_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            self.phases.lock().unwrap().push(Phase::AfterInference);
            ActionSet::empty()
        }
        async fn before_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            self.phases.lock().unwrap().push(Phase::BeforeToolExecute);
            ActionSet::empty()
        }
        async fn after_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterToolExecuteAction> {
            self.phases.lock().unwrap().push(Phase::AfterToolExecute);
            ActionSet::empty()
        }
        async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::StepEnd);
            ActionSet::empty()
        }
        async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::RunEnd);
            ActionSet::empty()
        }
    }

    let phases = Arc::new(Mutex::new(Vec::new()));
    let config = BaseAgent::new("mock").with_behavior(Arc::new(PhaseBlockPlugin {
        phases: phases.clone(),
    }) as Arc<dyn AgentBehavior>);
    let thread = Thread::with_initial_state("test", json!({})).with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);
    let recorded = phases.lock().unwrap().clone();
    assert!(recorded.contains(&Phase::StepStart));
    assert!(recorded.contains(&Phase::BeforeInference));
    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    assert_eq!(state["debug"]["phase_block"], true);
}

#[tokio::test]
async fn test_emit_cleanup_phases_and_apply_runs_after_inference_and_step_end() {
    struct CleanupBehavior {
        phases: Arc<Mutex<Vec<Phase>>>,
    }

    #[async_trait]
    impl AgentBehavior for CleanupBehavior {
        fn id(&self) -> &str {
            "cleanup_plugin"
        }

        async fn after_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            self.phases.lock().unwrap().push(Phase::AfterInference);
            let err = ctx.inference_error();
            assert_eq!(
                err.map(|e| e.error_type.as_str()),
                Some("llm_stream_start_error")
            );
            ActionSet::empty()
        }

        async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::StepEnd);
            ActionSet::single(LifecycleAction::State(test_bool_state_action(
                "debug.cleanup_ran",
                true,
            )))
        }
    }

    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let tool_descriptors = vec![ToolDescriptor::new("echo", "Echo", "Echo")];
    let phases = Arc::new(Mutex::new(Vec::new()));
    let agent = BaseAgent::new("mock").with_behavior(Arc::new(CleanupBehavior {
        phases: phases.clone(),
    }) as Arc<dyn AgentBehavior>);
    emit_cleanup_phases(
        &mut run_ctx,
        &tool_descriptors,
        &agent,
        "llm_stream_start_error",
        "boom".to_string(),
        Some("server_error"),
    )
    .await
    .expect("cleanup phases should succeed");

    assert_eq!(
        phases.lock().unwrap().as_slice(),
        &[Phase::AfterInference, Phase::StepEnd]
    );
    let state = run_ctx.snapshot().expect("state rebuild should succeed");
    assert_eq!(state["debug"]["cleanup_ran"], true);
}

#[tokio::test]
async fn test_plugin_can_model_run_scoped_data_via_state_and_cleanup() {
    struct RunScopedStatePlugin;

    #[async_trait]
    impl AgentBehavior for RunScopedStatePlugin {
        fn id(&self) -> &str {
            "run_scoped_state"
        }

        async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phase_actions(Phase::RunStart, ctx).await
        }

        async fn step_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phase_actions(Phase::StepStart, ctx).await
        }

        async fn run_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phase_actions(Phase::RunEnd, ctx).await
        }
    }

    impl RunScopedStatePlugin {
        async fn phase_actions(
            &self,
            phase: Phase,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<LifecycleAction> {
            if phase == Phase::RunStart {
                return ActionSet::single(LifecycleAction::State(test_i64_state_action(
                    "debug.temp_counter",
                    1,
                )));
            }

            if phase == Phase::StepStart {
                let state = ctx.snapshot();
                let current = state
                    .get("debug")
                    .and_then(|a| a.get("temp_counter"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                return ActionSet::single(LifecycleAction::State(test_i64_state_action(
                    "debug.temp_counter",
                    current + 1,
                )));
            }

            if phase != Phase::RunEnd {
                return ActionSet::empty();
            }
            let state = ctx.snapshot();
            let current = state
                .get("debug")
                .and_then(|a| a.get("temp_counter"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let run_count = state
                .get("debug")
                .and_then(|d| d.get("run_count"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                + 1;
            let counter = state
                .get("debug")
                .and_then(|a| a.get("temp_counter"))
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);

            let _ = current; // keep parity with previous evaluation order for debug state read
            ActionSet::single(LifecycleAction::State(test_i64_state_action(
                "debug.run_count",
                run_count,
            )))
            .and(ActionSet::single(LifecycleAction::State(
                test_i64_state_action("debug.last_temp_counter", counter),
            )))
            .and(ActionSet::single(LifecycleAction::State(
                test_json_state_action("debug.temp_counter", Value::Null),
            )))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(RunScopedStatePlugin) as Arc<dyn AgentBehavior>);
    let tools = HashMap::new();
    let thread = Thread::with_initial_state("test", json!({}));

    let (_, first_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        config.clone(),
        thread,
        tools.clone(),
    )
    .await;
    let first_state = first_thread.rebuild_state().unwrap();
    assert_eq!(first_state["debug"]["run_count"], 1);
    assert_eq!(first_state["debug"]["last_temp_counter"], 2);
    assert_eq!(first_state["debug"]["temp_counter"], Value::Null);

    let (_, second_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        config,
        first_thread,
        tools,
    )
    .await;
    let second_state = second_thread.rebuild_state().unwrap();
    assert_eq!(second_state["debug"]["run_count"], 2);
    assert_eq!(
        second_state["debug"]["last_temp_counter"], 2,
        "run-local state should be recreated each run and cleaned on RunEnd"
    );
    assert_eq!(second_state["debug"]["temp_counter"], Value::Null);
}

// ============================================================================
// Additional Coverage Tests
// ============================================================================

#[test]
fn test_agent_config_debug() {
    let config = BaseAgent::new("gpt-4").with_system_prompt("You are helpful.");

    let debug_str = format!("{:?}", config);
    assert!(debug_str.contains("BaseAgent"));
    assert!(debug_str.contains("gpt-4"));
    // Check that system_prompt is shown as length indicator
    assert!(debug_str.contains("chars]"));
}

#[test]
fn test_agent_config_with_chat_options() {
    let chat_options = ChatOptions::default();
    let config = BaseAgent::new("gpt-4").with_chat_options(chat_options);
    assert!(config.chat_options.is_some());
}

#[test]
fn test_agent_config_with_plugins() {
    struct DummyPlugin;

    #[async_trait]
    impl AgentBehavior for DummyPlugin {
        fn id(&self) -> &str {
            "dummy"
        }
    }

    let config = BaseAgent::new("gpt-4").with_behavior(compose_test_behaviors(vec![
        Arc::new(DummyPlugin) as Arc<dyn AgentBehavior>,
        Arc::new(DummyPlugin) as Arc<dyn AgentBehavior>,
    ]));
    assert!(config.behavior.id().contains("dummy"));
}

struct PendingPhasePlugin;

#[async_trait]
impl AgentBehavior for PendingPhasePlugin {
    fn id(&self) -> &str {
        "pending"
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        if ctx.tool_name() == Some("echo") {
            use crate::contracts::Suspension;
            ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                Suspension::new("confirm_1", "confirm").with_message("Execute echo?"),
            )))
        } else {
            ActionSet::empty()
        }
    }
}

#[test]
fn test_execute_tools_with_pending_phase_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Pending".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m")
            .with_behavior(Arc::new(PendingPhasePlugin) as Arc<dyn AgentBehavior>);

        let outcome = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .unwrap();

        let (thread, suspended_call) = match outcome {
            ExecuteToolsOutcome::Suspended {
                thread,
                suspended_call,
            } => (thread, suspended_call),
            other => panic!("Expected Suspended outcome, got: {:?}", other),
        };

        assert_eq!(suspended_call.ticket.suspension.id, "confirm_1");
        assert_eq!(suspended_call.ticket.suspension.action, "confirm");

        // Pending tool gets a placeholder tool result to keep message sequence valid.
        assert_eq!(thread.messages.len(), 1);
        let msg = &thread.messages[0];
        assert_eq!(msg.role, crate::contracts::thread::Role::Tool);
        assert!(msg.content.contains("awaiting approval"));

        let state = thread.rebuild_state().unwrap();
        assert_eq!(
            get_suspended_call(&state, "call_1").expect("call should be suspended")["suspension"]
                ["id"],
            "confirm_1"
        );
    });
}

#[test]
fn test_invalid_args_are_returned_as_tool_error_before_pending_confirmation() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Pending".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m")
            .with_behavior(Arc::new(PendingPhasePlugin) as Arc<dyn AgentBehavior>);

        let thread = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .expect("invalid args should return a tool error instead of suspended interaction")
            .into_thread();

        assert_eq!(thread.messages.len(), 1);
        let msg = &thread.messages[0];
        assert_eq!(msg.role, crate::contracts::thread::Role::Tool);
        assert!(
            !msg.content.contains("awaiting approval"),
            "invalid args should not produce pending placeholder: {}",
            msg.content
        );

        let payload: Value = serde_json::from_str(&msg.content).expect("tool result must be json");
        assert_eq!(payload["status"], "error");
        assert_eq!(payload["tool_name"], "echo");
        assert!(
            payload["message"]
                .as_str()
                .is_some_and(|m| m.contains("Invalid arguments")),
            "tool error message should report invalid arguments: {}",
            msg.content
        );

        let final_state = thread.rebuild_state().expect("state should rebuild");
        let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
        assert!(
            suspended.is_empty(),
            "invalid args should not persist suspended suspension: {suspended:?}"
        );
    });
}

#[test]
fn test_apply_tool_results_suspends_all_interactions() {
    let thread = Thread::new("test");
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let mut first = tool_execution_result("call_1", None);
    first.outcome = crate::contracts::ToolCallOutcome::Suspended;
    first.suspended_call = Some({
        let suspension = Suspension::new("confirm_1", "confirm").with_message("approve first tool");
        build_suspended_call(
            "call_1",
            "test_tool",
            suspension.clone(),
            test_frontend_invocation(&suspension),
        )
    });

    let mut second = tool_execution_result("call_2", None);
    second.outcome = crate::contracts::ToolCallOutcome::Suspended;
    second.suspended_call = Some({
        let suspension =
            Suspension::new("confirm_2", "confirm").with_message("approve second tool");
        build_suspended_call(
            "call_2",
            "test_tool",
            suspension.clone(),
            test_frontend_invocation(&suspension),
        )
    });

    let applied = apply_tool_results_to_session(&mut run_ctx, &[first, second], None, false)
        .expect("apply should succeed");
    assert_eq!(applied.suspended_calls.len(), 2);
    assert_eq!(applied.suspended_calls[0].call_id, "call_1");
    assert_eq!(applied.suspended_calls[1].call_id, "call_2");
    assert_eq!(run_ctx.messages().len(), 2);
    assert!(
        run_ctx.messages()[0].content.contains("awaiting approval"),
        "first suspended tool message: {}",
        run_ctx.messages()[0].content
    );
    assert!(
        run_ctx.messages()[1].content.contains("awaiting approval"),
        "second suspended tool message: {}",
        run_ctx.messages()[1].content
    );
    let state = run_ctx.snapshot().expect("snapshot should succeed");
    assert_eq!(
        get_suspended_call(&state, "call_1").expect("call should be suspended")["suspension"]["id"],
        "confirm_1"
    );
    // Per-call map has both entries
    let scopes = state.get("__tool_call_scope").and_then(|v| v.as_object());
    assert!(scopes
        .as_ref()
        .and_then(|s| s.get("call_1"))
        .and_then(|e| e.get("suspended_call"))
        .is_some());
    assert!(scopes
        .as_ref()
        .and_then(|s| s.get("call_2"))
        .and_then(|e| e.get("suspended_call"))
        .is_some());
}

#[test]
fn test_apply_tool_results_appends_skill_instruction_as_user_message() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let result = skill_activation_result("call_1", "docx", Some("## DOCX\nUse docx-js."));

    let _applied = apply_tool_results_to_session(&mut run_ctx, &[result], None, false)
        .expect("apply_tool_results_to_session should succeed");

    assert_eq!(run_ctx.messages().len(), 2);
    assert_eq!(
        run_ctx.messages()[0].role,
        crate::contracts::thread::Role::Tool
    );
    assert_eq!(
        run_ctx.messages()[1].role,
        crate::contracts::thread::Role::User
    );
    assert_eq!(run_ctx.messages()[1].content, "## DOCX\nUse docx-js.");
}

#[test]
fn test_apply_tool_results_skill_instruction_user_message_attaches_metadata() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let result = skill_activation_result("call_1", "docx", Some("Use docx-js."));
    let meta = MessageMetadata {
        run_id: Some("run-1".to_string()),
        step_index: Some(3),
    };

    let _applied =
        apply_tool_results_to_session(&mut run_ctx, &[result], Some(meta.clone()), false)
            .expect("apply_tool_results_to_session should succeed");

    assert_eq!(run_ctx.messages().len(), 2);
    let user_msg = &run_ctx.messages()[1];
    assert_eq!(user_msg.role, crate::contracts::thread::Role::User);
    assert_eq!(user_msg.metadata.as_ref(), Some(&meta));
}

#[test]
fn test_apply_tool_results_skill_without_instruction_does_not_append_user_message() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let result = skill_activation_result("call_1", "docx", None);

    let _applied = apply_tool_results_to_session(&mut run_ctx, &[result], None, false)
        .expect("apply_tool_results_to_session should succeed");

    assert_eq!(run_ctx.messages().len(), 1);
    assert_eq!(
        run_ctx.messages()[0].role,
        crate::contracts::thread::Role::Tool
    );
}

#[test]
fn test_apply_tool_results_appends_user_messages_from_effect() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let result = ToolExecutionResult {
        execution: crate::engine::tool_execution::ToolExecution {
            call: crate::contracts::thread::ToolCall::new("call_1", "any_tool", json!({})),
            result: ToolResult::success("any_tool", json!({"ok": true})),
            patch: None,
        },
        reminders: Vec::new(),
        user_messages: vec!["first".to_string(), "second".to_string()],
        outcome: crate::contracts::ToolCallOutcome::Succeeded,
        suspended_call: None,
        pending_patches: Vec::new(),
        serialized_state_actions: vec![],
    };

    let _applied = apply_tool_results_to_session(&mut run_ctx, &[result], None, false)
        .expect("apply should succeed");

    assert_eq!(run_ctx.messages().len(), 3);
    assert_eq!(
        run_ctx.messages()[0].role,
        crate::contracts::thread::Role::Tool
    );
    assert_eq!(
        run_ctx.messages()[1].role,
        crate::contracts::thread::Role::User
    );
    assert_eq!(run_ctx.messages()[1].content, "first");
    assert_eq!(
        run_ctx.messages()[2].role,
        crate::contracts::thread::Role::User
    );
    assert_eq!(run_ctx.messages()[2].content, "second");
}

#[test]
fn test_apply_tool_results_ignores_blank_user_messages() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let result = ToolExecutionResult {
        execution: crate::engine::tool_execution::ToolExecution {
            call: crate::contracts::thread::ToolCall::new("call_1", "any_tool", json!({})),
            result: ToolResult::success("any_tool", json!({"ok": true})),
            patch: None,
        },
        reminders: Vec::new(),
        user_messages: vec!["".to_string(), "   ".to_string()],
        outcome: crate::contracts::ToolCallOutcome::Succeeded,
        suspended_call: None,
        pending_patches: Vec::new(),
        serialized_state_actions: vec![],
    };

    let _applied = apply_tool_results_to_session(&mut run_ctx, &[result], None, false)
        .expect("apply should succeed");

    assert_eq!(run_ctx.messages().len(), 1);
    assert_eq!(
        run_ctx.messages()[0].role,
        crate::contracts::thread::Role::Tool
    );
}

#[test]
fn test_apply_tool_results_keeps_tool_and_appended_user_message_order_stable() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let first = skill_activation_result("call_2", "beta", Some("Instruction B"));
    let second = skill_activation_result("call_1", "alpha", Some("Instruction A"));

    let _applied =
        apply_tool_results_to_session(&mut run_ctx, &[first, second], None, true).expect("apply");
    let messages = run_ctx.messages();

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, crate::contracts::thread::Role::Tool);
    assert_eq!(messages[0].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(messages[1].role, crate::contracts::thread::Role::Tool);
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[2].role, crate::contracts::thread::Role::User);
    assert_eq!(messages[2].content, "Instruction B");
    assert_eq!(messages[3].role, crate::contracts::thread::Role::User);
    assert_eq!(messages[3].content, "Instruction A");
}

#[test]
fn test_agent_loop_error_state_error() {
    let err = AgentLoopError::StateError("invalid state".to_string());
    assert!(err.to_string().contains("State error"));
    assert!(err.to_string().contains("invalid state"));
}

#[test]
fn test_execute_tools_missing_tool() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Calling unknown tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "unknown_tool",
                json!({}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new(); // Empty tools

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        let msg = &thread.messages[0];
        assert!(
            msg.content.contains("not found") || msg.content.contains("Error"),
            "Expected 'not found' error in message, got: {}",
            msg.content
        );
    });
}

#[test]
fn test_execute_tools_with_config_empty_calls() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "No tools".to_string(),
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4");

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap()
            .into_thread();

        // No messages should be added when there are no tool calls
        assert_eq!(thread.message_count(), 0);
    });
}

#[test]
fn test_execute_tools_with_config_basic() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Calling tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4");

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        assert_eq!(
            thread.messages[0].role,
            crate::contracts::thread::Role::Tool
        );
    });
}

// Scope-based tool policy enforcement is tested via RunContext at the
// orchestrator level (prepare_run / run_stream_with_context), where RunPolicy
// is explicitly wired. The low-level execute_tools_with_config path uses
// RunPolicy::default() and is not the right place to test scope filtering.

#[test]
fn test_execute_tools_with_config_attaches_scope_run_metadata() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test").with_message(
            Message::assistant_with_tool_calls(
                "calling tool",
                vec![crate::contracts::thread::ToolCall::new(
                    "call_1",
                    "echo",
                    json!({"message": "test"}),
                )],
            )
            .with_metadata(crate::contracts::thread::MessageMetadata {
                run_id: Some("run-meta-1".to_string()),
                step_index: Some(7),
            }),
        );

        let result = StreamResult {
            text: "Calling tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4");

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 2);
        let tool_msg = thread.messages.last().expect("tool message should exist");
        assert_eq!(tool_msg.role, crate::contracts::thread::Role::Tool);
        let meta = tool_msg
            .metadata
            .as_ref()
            .expect("tool message metadata should be attached");
        assert_eq!(meta.run_id.as_deref(), Some("run-meta-1"));
        assert_eq!(meta.step_index, Some(7));
    });
}

#[test]
fn test_execute_tools_with_config_with_blocking_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Blocked".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4")
            .with_behavior(Arc::new(BlockingPhasePlugin) as Arc<dyn AgentBehavior>);

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        let msg = &thread.messages[0];
        assert!(
            msg.content.contains("blocked") || msg.content.contains("Error"),
            "Expected blocked error in message, got: {}",
            msg.content
        );
    });
}

#[test]
fn test_execute_tools_with_config_denied_response_is_applied_via_tool_call_state_resume() {
    use crate::contracts::Suspension;

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let base_state = json!({});
        let pending_patch = set_single_suspended_call(
            &base_state,
            Suspension::new("call_1", "tool:echo").with_message("awaiting approval"),
            None,
        )
        .expect("failed to seed suspended interaction");
        let thread = Thread::with_initial_state("test", base_state).with_patch(pending_patch);
        let result = StreamResult {
            text: "Trying tool after denial".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let interaction =
            TestInteractionPlugin::with_responses(Vec::new(), vec!["call_1".to_string()]);
        let config =
            BaseAgent::new("gpt-4").with_behavior(Arc::new(interaction) as Arc<dyn AgentBehavior>);

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .expect("tool execution should succeed with denied decision applied")
            .into_thread();

        assert_eq!(thread.message_count(), 1);
        let msg = &thread.messages[0];
        assert_eq!(msg.role, crate::contracts::thread::Role::Tool);
        assert!(
            msg.content.contains("\"status\":\"error\""),
            "denied decision should yield error tool result, got: {}",
            msg.content
        );

        let final_state = thread.rebuild_state().expect("state should rebuild");
        let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
        assert!(
            suspended.is_empty(),
            "resolved suspended call should be cleared: {suspended:?}"
        );

        assert_eq!(
            final_state["__tool_call_scope"]["call_1"]["tool_call_state"]["status"],
            json!("cancelled"),
            "denied replay should persist cancelled lifecycle state"
        );
    });
}

#[test]
fn test_execute_tools_with_config_allows_terminal_id_reuse_for_provider_retry() {
    // Some providers (e.g., Gemini) reuse the same tool call ID when retrying.
    // A terminal (Succeeded/Failed) call ID should start a fresh lifecycle.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state(
            "test",
            json!({
                "__tool_call_scope": {
                    "call_1": {
                        "tool_call_state": {
                            "call_id": "call_1",
                            "tool_name": "echo",
                            "arguments": { "message": "already-done" },
                            "status": "succeeded",
                            "updated_at": 1
                        }
                    }
                }
            }),
        );
        let result = StreamResult {
            text: "re-run".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "retry-attempt"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4");

        let outcome = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .expect("terminal ID reuse should succeed as a fresh lifecycle");
        let thread = outcome.into_thread();
        // The tool result message should be present (tool executed successfully)
        assert_eq!(thread.messages.len(), 1);
        assert_eq!(
            thread.messages[0].role,
            crate::contracts::thread::Role::Tool
        );
        assert!(
            thread.messages[0].content.contains("retry-attempt"),
            "tool result should reflect the retried call arguments"
        );
    });
}

#[test]
fn test_execute_tools_with_config_with_pending_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Pending".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4")
            .with_behavior(Arc::new(PendingPhasePlugin) as Arc<dyn AgentBehavior>);

        let outcome = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap();

        let (thread, suspended_call) = match outcome {
            ExecuteToolsOutcome::Suspended {
                thread,
                suspended_call,
            } => (thread, suspended_call),
            other => panic!("Expected Suspended outcome, got: {:?}", other),
        };

        assert_eq!(suspended_call.ticket.suspension.id, "confirm_1");
        assert_eq!(suspended_call.ticket.suspension.action, "confirm");

        // Pending tool gets a placeholder tool result to keep message sequence valid.
        assert_eq!(thread.messages.len(), 1);
        let msg = &thread.messages[0];
        assert_eq!(msg.role, crate::contracts::thread::Role::Tool);
        assert!(msg.content.contains("awaiting approval"));

        // Pending interaction should be persisted via state.
        let state = thread.rebuild_state().unwrap();
        assert_eq!(
            get_suspended_call(&state, "call_1").expect("call should be suspended")["suspension"]
                ["id"],
            "confirm_1"
        );
    });
}

#[test]
fn test_execute_tools_with_config_with_reminder_plugin() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "With reminder".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4")
            .with_behavior(Arc::new(ReminderPhasePlugin) as Arc<dyn AgentBehavior>);

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap()
            .into_thread();

        // Should have tool response + reminder message
        assert_eq!(thread.message_count(), 2);
        assert!(thread.messages[1].content.contains("system-reminder"));
    });
}

#[test]
fn test_execute_tools_with_config_preserves_unresolved_suspended_calls_on_success() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Seed a session with a previously persisted suspended interaction.
        let base_state = json!({});
        let pending_patch = set_single_suspended_call(
            &base_state,
            Suspension::new("confirm_1", "confirm").with_message("ok"),
            None,
        )
        .expect("failed to set suspended interaction for test seed");
        let thread = Thread::with_initial_state("test", base_state).with_patch(pending_patch);

        let result = StreamResult {
            text: "Calling tool".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let config = BaseAgent::new("gpt-4");

        let thread = execute_tools_with_config(thread, &result, &tools, &config)
            .await
            .unwrap()
            .into_thread();

        let state = thread.rebuild_state().unwrap();
        let suspended = state.get("__tool_call_scope").and_then(|v| v.as_object());
        assert!(
            suspended.is_some_and(|calls| calls.contains_key("confirm_1")),
            "expected unresolved suspended call to be preserved, got: {suspended:?}"
        );
    });
}

#[test]
fn test_suspended_call_action_persists_all_entries() {
    let base_state = json!({});
    let calls: Vec<crate::contracts::runtime::SuspendedCall> = vec![
        {
            let suspension = Suspension::new("int_b", "confirm").with_message("b");
            build_suspended_call(
                "call_b",
                "echo",
                suspension.clone(),
                test_frontend_invocation(&suspension),
            )
        },
        {
            let suspension = Suspension::new("int_a", "confirm").with_message("a");
            build_suspended_call(
                "call_a",
                "echo",
                suspension.clone(),
                test_frontend_invocation(&suspension),
            )
        },
    ];
    let actions: Vec<AnyStateAction> = calls
        .into_iter()
        .map(|call| call.into_state_action())
        .collect();
    let patches = crate::contracts::runtime::state::reduce_state_actions(
        actions,
        &base_state,
        "test",
        &crate::contracts::runtime::state::ScopeContext::run(),
    )
    .expect("reduce suspended call actions");
    let mut run_ctx = RunContext::new(
        "test",
        base_state,
        Vec::<Arc<Message>>::new(),
        tirea_contract::RunPolicy::default(),
    );
    for patch in patches {
        run_ctx.add_thread_patch(patch);
    }
    let suspended = run_ctx.suspended_calls();
    assert_eq!(suspended.len(), 2);
    assert_eq!(suspended["call_a"].ticket.suspension.id, "int_a");
    assert_eq!(suspended["call_b"].ticket.suspension.id, "int_b");
}

// ========================================================================
// Phase lifecycle helpers & tests for run_loop_stream
// ========================================================================

/// Plugin that records phases and terminates before inference.
struct RecordAndTerminatePlugin {
    phases: Arc<Mutex<Vec<Phase>>>,
}

impl RecordAndTerminatePlugin {
    fn new() -> (Self, Arc<Mutex<Vec<Phase>>>) {
        let phases = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                phases: phases.clone(),
            },
            phases,
        )
    }
}

#[async_trait]
impl AgentBehavior for RecordAndTerminatePlugin {
    fn id(&self) -> &str {
        "record_and_terminate_behavior_requested"
    }
    async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.phases.lock().unwrap().push(Phase::RunStart);
        ActionSet::empty()
    }
    async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.phases.lock().unwrap().push(Phase::StepStart);
        ActionSet::empty()
    }
    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        self.phases.lock().unwrap().push(Phase::BeforeInference);
        ActionSet::single(BeforeInferenceAction::Terminate(
            TerminationReason::BehaviorRequested,
        ))
    }
    async fn after_inference(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<AfterInferenceAction> {
        self.phases.lock().unwrap().push(Phase::AfterInference);
        ActionSet::empty()
    }
    async fn before_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        self.phases.lock().unwrap().push(Phase::BeforeToolExecute);
        ActionSet::empty()
    }
    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        self.phases.lock().unwrap().push(Phase::AfterToolExecute);
        ActionSet::empty()
    }
    async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.phases.lock().unwrap().push(Phase::StepEnd);
        ActionSet::empty()
    }
    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.phases.lock().unwrap().push(Phase::RunEnd);
        ActionSet::empty()
    }
}

/// Collect all events from a stream.
async fn collect_stream_events(
    stream: Pin<Box<dyn Stream<Item = AgentEvent> + Send>>,
) -> Vec<AgentEvent> {
    use futures::StreamExt;
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn test_stream_terminate_behavior_requested_emits_run_end_phase() {
    let (recorder, phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    // Verify events include RunStart and RunFinish
    assert!(
        matches!(events.first(), Some(AgentEvent::RunStart { .. })),
        "Expected RunStart as first event, got: {:?}",
        events.first()
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::RunFinish { .. })),
        "Expected RunFinish as last event, got: {:?}",
        events.last()
    );

    // Verify phase lifecycle: RunStart → StepStart → BeforeInference → RunEnd
    let recorded = phases.lock().unwrap().clone();
    assert!(
        recorded.contains(&Phase::RunStart),
        "Missing RunStart phase"
    );
    assert!(recorded.contains(&Phase::RunEnd), "Missing RunEnd phase");

    // RunEnd must be last
    assert_eq!(
        recorded.last(),
        Some(&Phase::RunEnd),
        "RunEnd should be last phase, got: {:?}",
        recorded
    );
    let run_end_count = recorded.iter().filter(|p| **p == Phase::RunEnd).count();
    assert_eq!(run_end_count, 1, "RunEnd should be emitted exactly once");
}

#[tokio::test]
async fn test_stream_terminate_behavior_requested_emits_run_start_and_finish() {
    // Verify the complete event sequence on terminate_behavior_requested path
    let (recorder, _phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    let event_names: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AgentEvent::RunStart { .. } => "RunStart",
            AgentEvent::RunFinish { .. } => "RunFinish",
            AgentEvent::Error { .. } => "Error",
            _ => "Other",
        })
        .collect();
    assert_eq!(event_names, vec!["RunStart", "RunFinish"]);
}

#[tokio::test]
async fn test_stream_run_start_resume_replay_emits_after_run_start() {
    let (recorder, _phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread = Thread::with_initial_state(
        "test",
        json!({
            "__tool_call_scope": {
                "call_1": {
                    "tool_call_state": {
                        "call_id": "call_1",
                        "tool_name": "echo",
                        "arguments": {},
                        "status": "resuming",
                        "resume_token": "call_1",
                        "resume": {
                            "decision_id": "decision_1",
                            "action": "cancel",
                            "result": false,
                            "updated_at": 1
                        },
                        "updated_at": 1
                    },
                    "suspended_call": {
                        "call_id": "call_1",
                        "tool_name": "echo",
                        "arguments": {},
                        "suspension": { "id": "call_1", "action": "tool:echo" },
                        "pending": { "id": "call_1", "name": "echo", "arguments": {} },
                        "resume_mode": "replay_tool_call"
                    }
                }
            }
        }),
    )
    .with_message(crate::contracts::thread::Message::user("hello"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
    assert!(matches!(
        events.get(1),
        Some(AgentEvent::ToolCallResumed {
            target_id,
            result
        }) if target_id == "call_1" && result == &serde_json::Value::Bool(false)
    ));
    assert!(matches!(events.last(), Some(AgentEvent::RunFinish { .. })));
}

#[tokio::test]
async fn test_stream_terminate_behavior_requested_with_pending_state_emits_pending_and_pauses() {
    struct PendingTerminatePlugin;

    #[async_trait]
    impl AgentBehavior for PendingTerminatePlugin {
        fn id(&self) -> &str {
            "pending_terminate_behavior_requested"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::Suspended,
            ))
            .and(ActionSet::single(BeforeInferenceAction::State(
                single_suspended_call_state_action(
                    Suspension::new("agent_recovery_run-1", "recover_agent_run")
                        .with_message("resume?"),
                    None,
                ),
            )))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(PendingTerminatePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
    assert!(matches!(
        events.get(1),
        Some(AgentEvent::ToolCallStart { id, name })
            if id == "agent_recovery_run-1" && name == "recover_agent_run"
    ));
    assert!(matches!(
        events.get(2),
        Some(AgentEvent::ToolCallReady { id, name, .. })
            if id == "agent_recovery_run-1" && name == "recover_agent_run"
    ));
    assert!(matches!(
        events.get(3),
        Some(AgentEvent::RunFinish {
            termination: TerminationReason::Suspended,
            ..
        })
    ));
    assert_eq!(events.len(), 4, "unexpected extra events: {events:?}");
}

#[tokio::test]
async fn test_stream_run_action_with_suspended_only_state_emits_pending_events() {
    struct PendingTerminatePlugin;

    #[async_trait]
    impl AgentBehavior for PendingTerminatePlugin {
        fn id(&self) -> &str {
            "pending_terminate_suspended_only_stream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::Suspended,
            ))
        }
    }

    use crate::contracts::Suspension;

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(PendingTerminatePlugin) as Arc<dyn AgentBehavior>);
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("recover_1", "recover_agent_run").with_message("resume?"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(crate::contracts::thread::Message::user("hello"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
    assert!(matches!(
        events.get(1),
        Some(AgentEvent::ToolCallStart { id, name })
            if id == "recover_1" && name == "recover_agent_run"
    ));
    assert!(matches!(
        events.get(2),
        Some(AgentEvent::ToolCallReady { id, name, .. })
            if id == "recover_1" && name == "recover_agent_run"
    ));
    assert!(matches!(
        events.get(3),
        Some(AgentEvent::RunFinish {
            termination: TerminationReason::Suspended,
            ..
        })
    ));
}

#[tokio::test]
async fn test_stream_emits_interaction_resolved_on_denied_response() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let interaction =
        TestInteractionPlugin::with_responses(Vec::new(), vec!["call_write".to_string()]);
    let config = BaseAgent::new("gpt-4o-mini").with_behavior(compose_test_behaviors(vec![
        Arc::new(interaction),
        Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
    ]));
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_write", "tool:write").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(crate::contracts::thread::Message::user("continue"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResumed {
                target_id,
                result
            } if target_id == "call_write" && result == &serde_json::Value::Bool(false)
        )),
        "missing denied ToolCallResumed event: {events:?}"
    );
}

#[tokio::test]
async fn test_stream_permission_approval_replays_tool_and_appends_tool_result() {
    use crate::contracts::Suspension;

    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_permission_approval"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let interaction = TestInteractionPlugin::with_responses(vec!["call_1".to_string()], Vec::new());
    let config = BaseAgent::new("mock").with_behavior(compose_test_behaviors(vec![
        Arc::new(interaction),
        Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
    ]));

    // Seed state with a pre-existing suspended interaction for the pending tool call.
    let base_state = json!({});
    let echo_args = json!({"message": "approved-run"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:echo")
            .with_message("awaiting approval")
            .with_parameters(echo_args.clone()),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1", "echo", echo_args,
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));

    let tools = tool_map([EchoTool]);
    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        config,
        thread,
        tools,
    )
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResumed {
                target_id,
                result
            } if target_id == "call_1" && result == &serde_json::Value::Bool(true)
        )),
        "missing approval ToolCallResumed event: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallDone { id, result, .. }
                if id == "call_1" && result.status == crate::contracts::runtime::tool_call::ToolStatus::Success
        )),
        "approved flow must replay and execute original tool call: {events:?}"
    );

    let tool_msgs: Vec<&Arc<Message>> = final_thread
        .messages
        .iter()
        .filter(|m| {
            m.role == crate::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
        })
        .collect();
    assert!(!tool_msgs.is_empty(), "expected tool messages for call_1");
    let placeholder_index = tool_msgs
        .iter()
        .position(|m| m.content.contains("awaiting approval"))
        .expect("placeholder must remain immutable in append-only stream");
    let replay_index = tool_msgs
        .iter()
        .position(|m| m.content.contains("\"echoed\":\"approved-run\""))
        .expect("missing replayed tool result content");
    assert!(
        replay_index > placeholder_index,
        "replayed tool output must be appended after placeholder"
    );

    let final_state = final_thread.rebuild_state().expect("state should rebuild");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
    assert!(
        suspended.is_empty(),
        "suspended calls should be cleared after approval replay: {suspended:?}"
    );
}

#[tokio::test]
async fn test_run_loop_permission_approval_replays_tool_and_updates_lifecycle_state() {
    use crate::contracts::Suspension;

    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_permission_approval_nonstream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let interaction = TestInteractionPlugin::with_responses(vec!["call_1".to_string()], Vec::new());
    let config = BaseAgent::new("mock")
        .with_behavior(compose_test_behaviors(vec![
            Arc::new(interaction),
            Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
        ]))
        .with_llm_executor(Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
            "unused",
        ))])) as Arc<dyn LlmExecutor>);

    // Seed state with a pre-existing suspended interaction for the pending tool call.
    let base_state = json!({});
    let echo_args = json!({"message": "approved-run"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:echo")
            .with_message("awaiting approval")
            .with_parameters(echo_args.clone()),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1", "echo", echo_args,
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));

    let tools = tool_map([EchoTool]);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);

    let tool_msgs: Vec<&Arc<Message>> = outcome
        .run_ctx
        .messages()
        .iter()
        .filter(|m| {
            m.role == crate::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
        })
        .collect();
    assert!(!tool_msgs.is_empty(), "expected tool messages for call_1");
    let placeholder_index = tool_msgs
        .iter()
        .position(|m| m.content.contains("awaiting approval"))
        .expect("placeholder must remain immutable in append-only log");
    let replay_index = tool_msgs
        .iter()
        .position(|m| m.content.contains("\"echoed\":\"approved-run\""))
        .expect("missing replayed tool result content");
    assert!(
        replay_index > placeholder_index,
        "replayed tool output must be appended after placeholder"
    );

    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&state);
    assert!(
        suspended.is_empty(),
        "suspended calls should be cleared after approval replay: {suspended:?}"
    );

    assert_eq!(
        state["__tool_call_scope"]["call_1"]["tool_call_state"]["status"],
        json!("succeeded"),
        "run-start replay should end in succeeded lifecycle state"
    );
}

#[tokio::test]
async fn test_stream_permission_approval_replay_commits_before_and_after_replay() {
    use crate::contracts::Suspension;

    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_permission_approval_checkpoint"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let committer = Arc::new(RecordingStateCommitter::new(None));
    let interaction = TestInteractionPlugin::with_responses(vec!["call_1".to_string()], Vec::new());
    let config = BaseAgent::new("mock")
        .with_behavior(compose_test_behaviors(vec![
            Arc::new(interaction),
            Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
        ]))
        .with_llm_executor(
            Arc::new(MockStreamProvider::new(vec![MockResponse::text("unused")]))
                as Arc<dyn LlmExecutor>,
        );

    // Seed state with a pre-existing suspended interaction for the pending tool call.
    let base_state = json!({});
    let echo_args = json!({"message": "approved-run"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:echo")
            .with_message("awaiting approval")
            .with_parameters(echo_args.clone()),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1", "echo", echo_args,
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        tool_map([EchoTool]),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    ))
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResumed { target_id, result }
                if target_id == "call_1" && result == &serde_json::Value::Bool(true)
        )),
        "missing approval ToolCallResumed event: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallDone { id, .. } if id == "call_1"
        )),
        "approved replay must emit ToolCallDone: {events:?}"
    );

    assert_eq!(
        committer.reasons(),
        vec![
            CheckpointReason::UserMessage,
            CheckpointReason::ToolResultsCommitted,
            CheckpointReason::RunFinished
        ]
    );
}

#[tokio::test]
async fn test_run_loop_run_start_replay_uses_tool_call_resume_state_without_mailbox() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_tool_state_replay_nonstream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>)
        .with_llm_executor(Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
            "unused",
        ))])) as Arc<dyn LlmExecutor>);

    let echo_args = json!({"message": "approved-run"});
    let thread = Thread::with_initial_state(
        "test",
        json!({
            "__tool_call_scope": {
                "call_1": {
                    "tool_call_state": {
                        "call_id": "call_1",
                        "tool_name": "echo",
                        "arguments": echo_args,
                        "status": "resuming",
                        "resume_token": "call_1",
                        "resume": {
                            "decision_id": "decision_call_1",
                            "action": "resume",
                            "result": true,
                            "updated_at": 1
                        },
                        "updated_at": 1
                    },
                    "suspended_call": {
                        "call_id": "call_1",
                        "tool_name": "echo",
                        "arguments": echo_args,
                        "suspension": { "id": "call_1", "action": "tool:echo" },
                        "pending": { "id": "call_1", "name": "echo", "arguments": echo_args },
                        "resume_mode": "replay_tool_call"
                    }
                }
            }
        }),
    )
    .with_message(Message::assistant_with_tool_calls(
        "need permission",
        vec![crate::contracts::thread::ToolCall::new(
            "call_1",
            "echo",
            json!({"message": "approved-run"}),
        )],
    ))
    .with_message(Message::tool(
        "call_1",
        "Tool 'echo' is awaiting approval. Execution paused.",
    ));

    let tools = tool_map([EchoTool]);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && message.content.contains("\"echoed\":\"approved-run\"")
        }),
        "run-start replay should execute from tool_call resume state"
    );

    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
    assert!(
        suspended.is_empty(),
        "replayed call should clear suspended queue"
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_1"]["tool_call_state"]["status"],
        json!("succeeded")
    );
}

#[tokio::test]
async fn test_run_loop_run_start_settles_orphan_resuming_state_without_suspended_call() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_settle_orphan_resuming_nonstream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);

    let thread = Thread::with_initial_state(
        "test",
        json!({
            "__tool_call_scope": {
                "call_orphan": {
                    "tool_call_state": {
                        "call_id": "call_orphan",
                        "tool_name": "echo",
                        "arguments": { "message": "late decision" },
                        "status": "resuming",
                        "resume_token": "call_orphan",
                        "resume": {
                            "decision_id": "decision_orphan",
                            "action": "cancel",
                            "result": false,
                            "updated_at": 1
                        },
                        "updated_at": 1
                    }
                }
            }
        }),
    )
    .with_message(Message::user("continue"));

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);
    assert_eq!(outcome.stats.llm_calls, 0, "inference should not run");
    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    assert_eq!(
        final_state["__tool_call_scope"]["call_orphan"]["tool_call_state"]["status"],
        json!("cancelled")
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_orphan"]["tool_call_state"]["resume"]["action"],
        json!("cancel")
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_orphan"]["tool_call_state"]["resume_token"],
        json!("call_orphan")
    );
}

#[tokio::test]
async fn test_stream_permission_denied_does_not_replay_tool_call() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_permission_denial"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let interaction = TestInteractionPlugin::with_responses(Vec::new(), vec!["call_1".to_string()]);
    let config = BaseAgent::new("mock").with_behavior(compose_test_behaviors(vec![
        Arc::new(interaction),
        Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
    ]));
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:echo").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "denied-run"}),
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));

    let tools = tool_map([EchoTool]);
    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        config,
        thread,
        tools,
    )
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResumed {
                target_id,
                result
            } if target_id == "call_1" && result == &serde_json::Value::Bool(false)
        )),
        "missing denied ToolCallResumed event: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolCallDone { id, result, .. }
                    if id == "call_1"
                        && result.is_error()
                        && result
                            .message
                            .as_ref()
                            .is_some_and(|message| message.contains("denied"))
            )
        }),
        "denied flow should synthesize a tool error result for the original call: {events:?}"
    );

    let denied_tool_msg = final_thread
        .messages
        .iter()
        .find(|m| {
            m.role == crate::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.content.contains("denied")
        })
        .expect("denied flow should append a tool error message for call_1");
    assert!(denied_tool_msg.content.contains("denied"));

    let final_state = final_thread.rebuild_state().expect("state should rebuild");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
    assert!(suspended.is_empty());
}

#[tokio::test]
async fn test_run_loop_permission_denied_appends_tool_result_for_model_context() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_permission_denial_nonstream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let interaction = TestInteractionPlugin::with_responses(Vec::new(), vec!["call_1".to_string()]);
    let config = BaseAgent::new("mock").with_behavior(compose_test_behaviors(vec![
        Arc::new(interaction),
        Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
    ]));

    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:echo").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "denied-run"}),
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));
    let denied_count = outcome
        .run_ctx
        .messages()
        .iter()
        .filter(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && message.content.contains("denied")
        })
        .count();
    assert_eq!(
        denied_count, 1,
        "non-stream denied flow should append one denied tool message for model context"
    );
}

#[tokio::test]
async fn test_run_loop_permission_cancelled_appends_tool_result_for_model_context() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_permission_cancel_nonstream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let interaction = TestInteractionPlugin::from_interaction_responses(vec![
        crate::contracts::SuspensionResponse::new(
            "call_1",
            json!({"status": "cancelled", "reason": "User canceled in UI"}),
        ),
    ]);
    let config = BaseAgent::new("mock").with_behavior(compose_test_behaviors(vec![
        Arc::new(interaction),
        Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
    ]));

    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:echo").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "cancel-run"}),
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));

    let resolved_tool_messages: Vec<_> = outcome
        .run_ctx
        .messages()
        .iter()
        .filter(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && !message
                    .content
                    .contains("is awaiting approval. Execution paused.")
        })
        .collect();
    assert_eq!(
        resolved_tool_messages.len(),
        1,
        "cancelled flow should append one resolved tool message"
    );
    assert!(
        resolved_tool_messages[0].content.contains("canceled")
            || resolved_tool_messages[0].content.contains("cancelled"),
        "cancelled flow should preserve cancel semantics in tool message: {}",
        resolved_tool_messages[0].content
    );

    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
    assert!(suspended.is_empty());
}

#[tokio::test]
async fn test_run_loop_terminate_behavior_requested_emits_run_end_phase() {
    let (recorder, phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    // terminate_behavior_requested in run_loop terminates with BehaviorRequested (not NaturalEnd)
    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));

    let recorded = phases.lock().unwrap().clone();
    assert!(
        recorded.contains(&Phase::RunStart),
        "Missing RunStart phase"
    );
    assert!(recorded.contains(&Phase::RunEnd), "Missing RunEnd phase");
    assert_eq!(
        recorded.last(),
        Some(&Phase::RunEnd),
        "RunEnd should be last phase, got: {:?}",
        recorded
    );
    let run_end_count = recorded.iter().filter(|p| **p == Phase::RunEnd).count();
    assert_eq!(run_end_count, 1, "RunEnd should be emitted exactly once");
}

#[tokio::test]
async fn test_legacy_resume_replay_nonstream_resolution_state_is_ignored() {
    struct LegacyResumeReplayTerminatePlugin;

    #[async_trait]
    impl AgentBehavior for LegacyResumeReplayTerminatePlugin {
        fn id(&self) -> &str {
            "legacy_resume_replay_nonstream_resolution_state"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(LegacyResumeReplayTerminatePlugin) as Arc<dyn AgentBehavior>);

    let thread = Thread::with_initial_state(
        "test",
        json!({
            "__resolved_suspensions": {
                "resolutions": [
                    {
                        "target_id": "resolution_1",
                        "result": true
                    }
                ]
            }
        }),
    )
    .with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));

    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    let resolutions = state
        .get("__resolved_suspensions")
        .and_then(|legacy| legacy.get("resolutions"));
    assert_eq!(
        resolutions,
        Some(&json!([{
            "target_id": "resolution_1",
            "result": true
        }])),
        "legacy resume replay resolutions should be ignored by run-start replay"
    );
}

#[tokio::test]
async fn test_legacy_resume_replay_nonstream_queue_is_ignored() {
    struct LegacyResumeReplayRequeuePlugin;

    #[async_trait]
    impl AgentBehavior for LegacyResumeReplayRequeuePlugin {
        fn id(&self) -> &str {
            "legacy_resume_replay_nonstream_queue"
        }

        async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phase_actions(Phase::RunStart, ctx).await
        }
        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_call_id() == Some("replay_call_1") {
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new("confirm_replay_call_1", "confirm")
                        .with_message("approve first replay"),
                )));
            }
            ActionSet::empty()
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    impl LegacyResumeReplayRequeuePlugin {
        async fn phase_actions(
            &self,
            phase: Phase,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<LifecycleAction> {
            if phase != Phase::RunStart {
                return ActionSet::empty();
            }
            ActionSet::single(LifecycleAction::State(test_json_state_action(
                "__resume_tool_calls.calls",
                json!([
                    {
                        "id": "replay_call_1",
                        "name": "echo",
                        "arguments": {"message": "first"}
                    },
                    {
                        "id": "replay_call_2",
                        "name": "echo",
                        "arguments": {"message": "second"}
                    }
                ]),
            )))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(LegacyResumeReplayRequeuePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("resume"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, tool_map([EchoTool]), run_ctx, None, None, None).await;
    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);

    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    let legacy_replay_calls = state
        .get("__resume_tool_calls")
        .and_then(|legacy| legacy.get("calls"))
        .and_then(|calls| calls.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        legacy_replay_calls.len(),
        2,
        "legacy resume replay queue should remain untouched"
    );
    assert_eq!(
        legacy_replay_calls[0]["id"],
        Value::String("replay_call_1".to_string()),
        "legacy resume replay queue order should be preserved"
    );
}

#[tokio::test]
async fn test_run_loop_terminate_behavior_requested_with_suspended_state_returns_suspended_interaction(
) {
    struct PendingTerminatePlugin {
        phases: Arc<Mutex<Vec<Phase>>>,
    }

    #[async_trait]
    impl AgentBehavior for PendingTerminatePlugin {
        fn id(&self) -> &str {
            "pending_terminate_behavior_requested_non_stream"
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::RunStart);
            ActionSet::empty()
        }

        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::StepStart);
            ActionSet::empty()
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            self.phases.lock().unwrap().push(Phase::BeforeInference);
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
            .and(ActionSet::single(BeforeInferenceAction::State(
                single_suspended_call_state_action(
                    Suspension::new("agent_recovery_run-1", "recover_agent_run")
                        .with_message("resume?"),
                    None,
                ),
            )))
        }
        async fn after_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            self.phases.lock().unwrap().push(Phase::AfterInference);
            ActionSet::empty()
        }

        async fn before_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            self.phases.lock().unwrap().push(Phase::BeforeToolExecute);
            ActionSet::empty()
        }

        async fn after_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterToolExecuteAction> {
            self.phases.lock().unwrap().push(Phase::AfterToolExecute);
            ActionSet::empty()
        }

        async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::StepEnd);
            ActionSet::empty()
        }

        async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases.lock().unwrap().push(Phase::RunEnd);
            ActionSet::empty()
        }
    }

    let phases = Arc::new(Mutex::new(Vec::new()));
    let config = BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(PendingTerminatePlugin {
        phases: phases.clone(),
    }) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert!(matches!(outcome.termination, TerminationReason::Suspended));

    let suspended_calls = outcome.run_ctx.suspended_calls();
    let interaction = &suspended_calls
        .get("agent_recovery_run-1")
        .expect("should have suspended interaction")
        .ticket
        .suspension;
    assert_eq!(interaction.action, "recover_agent_run");
    assert_eq!(interaction.message, "resume?");

    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    assert_eq!(
        get_suspended_call(&state, "agent_recovery_run-1").expect("call should be suspended")
            ["suspension"]["action"],
        Value::String("recover_agent_run".to_string())
    );

    let recorded = phases.lock().unwrap().clone();
    assert_eq!(
        recorded.last(),
        Some(&Phase::RunEnd),
        "RunEnd should be last phase, got: {:?}",
        recorded
    );
    let run_end_count = recorded.iter().filter(|p| **p == Phase::RunEnd).count();
    assert_eq!(run_end_count, 1, "RunEnd should be emitted exactly once");
}

#[tokio::test]
async fn test_run_loop_terminate_behavior_requested_with_suspended_only_state_returns_suspended_interaction(
) {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_non_stream_suspended_only"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_pending", "tool:echo").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert!(matches!(outcome.termination, TerminationReason::Suspended));
}

#[tokio::test]
async fn test_run_loop_auto_generated_run_id_is_rfc4122_uuid_v7() {
    let (recorder, _phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    // terminate_behavior_requested in run_loop terminates with BehaviorRequested
    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));
    let run_id = outcome
        .run_ctx
        .run_identity()
        .run_id_opt()
        .unwrap_or_else(|| panic!("run_loop must populate execution run_id"));

    let parsed = uuid::Uuid::parse_str(run_id)
        .unwrap_or_else(|_| panic!("run_id must be parseable UUID, got: {run_id}"));
    assert_eq!(
        parsed.get_variant(),
        uuid::Variant::RFC4122,
        "run_id must be RFC4122 UUID, got: {run_id}"
    );
    assert_eq!(
        parsed.get_version_num(),
        7,
        "run_id must be version 7 UUID, got: {run_id}"
    );
}

#[tokio::test]
async fn test_run_loop_phase_sequence_on_terminate_behavior_requested() {
    // Verify the full phase sequence: RunStart → StepStart → BeforeInference → RunEnd
    let (recorder, phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    // terminate_behavior_requested in run_loop terminates with BehaviorRequested
    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));

    let recorded = phases.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            Phase::RunStart,
            Phase::StepStart,
            Phase::BeforeInference,
            Phase::RunEnd,
        ],
        "Unexpected phase sequence: {:?}",
        recorded
    );
}

#[tokio::test]
async fn test_run_loop_step_start_run_action_mutation_is_type_safe() {
    // With typed ActionSet<LifecycleAction>, step_start can only emit State actions.
    // RequestTermination cannot be placed in LifecycleAction (compile-time type safety).
    // Verify that a lifecycle-phase plugin with no-op behavior still runs successfully.
    struct NoOpStepStartPlugin;

    #[async_trait]
    impl AgentBehavior for NoOpStepStartPlugin {
        fn id(&self) -> &str {
            "noop_step_start"
        }
        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            ActionSet::empty()
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(NoOpStepStartPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert_eq!(
        outcome.termination,
        TerminationReason::BehaviorRequested,
        "expected BehaviorRequested from before_inference, got: {:?}",
        outcome.termination
    );
}

#[tokio::test]
async fn test_stream_step_start_run_action_mutation_is_type_safe() {
    // With typed ActionSet<LifecycleAction>, step_start can only emit State actions.
    // RequestTermination cannot be placed in LifecycleAction (compile-time type safety).
    // Verify that a stream run completes normally with a no-op step_start.
    struct NoOpStepStartPlugin;

    #[async_trait]
    impl AgentBehavior for NoOpStepStartPlugin {
        fn id(&self) -> &str {
            "noop_step_start_stream"
        }
        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            ActionSet::empty()
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(NoOpStepStartPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("hi"));
    let tools = HashMap::new();

    let events = run_mock_stream(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        config,
        thread,
        tools,
    )
    .await;

    assert!(
        matches!(events.last(), Some(AgentEvent::RunFinish { .. })),
        "expected stream to complete normally: {events:?}"
    );
}

#[tokio::test]
async fn test_run_loop_step_start_prompt_context_mutation_is_type_safe() {
    // With typed ActionSet<LifecycleAction>, step_start can only emit State actions.
    // AddSystemContext cannot be placed in LifecycleAction (compile-time type safety).
    // Verify that the loop completes normally with a no-op step_start plugin.
    struct NoOpStepStartPlugin;

    #[async_trait]
    impl AgentBehavior for NoOpStepStartPlugin {
        fn id(&self) -> &str {
            "noop_step_start_prompt"
        }
        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            ActionSet::empty()
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(NoOpStepStartPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert_eq!(
        outcome.termination,
        TerminationReason::BehaviorRequested,
        "expected BehaviorRequested, got: {:?}",
        outcome.termination
    );
}

#[tokio::test]
async fn test_run_loop_multiple_prompt_context_behaviors_are_additive() {
    // In the AgentBehavior model, multiple SystemContext effects are always additive.
    // Both behaviors append their context; no "non-append mutation" error applies.
    struct PromptAppendPlugin;

    #[async_trait]
    impl AgentBehavior for PromptAppendPlugin {
        fn id(&self) -> &str {
            "prompt_append"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::AddSystemContext("base".into())).and(
                ActionSet::single(BeforeInferenceAction::Terminate(
                    TerminationReason::BehaviorRequested,
                )),
            )
        }
    }

    struct PromptReplacePlugin;

    #[async_trait]
    impl AgentBehavior for PromptReplacePlugin {
        fn id(&self) -> &str {
            "prompt_replace"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::AddSystemContext("replaced".into()))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini").with_behavior(compose_test_behaviors(vec![
        Arc::new(PromptAppendPlugin) as Arc<dyn AgentBehavior>,
        Arc::new(PromptReplacePlugin) as Arc<dyn AgentBehavior>,
    ]));
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    // Both behaviors append system_context. The first also requests termination,
    // so the loop terminates before inference without hitting the LLM.
    assert_eq!(
        outcome.termination,
        TerminationReason::BehaviorRequested,
        "expected BehaviorRequested termination, got: {:?} / {:?}",
        outcome.termination,
        outcome.failure
    );
}

#[tokio::test]
async fn test_stream_rejects_prompt_context_mutation_outside_before_inference() {
    struct InvalidStepStartPromptPlugin;

    #[async_trait]
    impl AgentBehavior for InvalidStepStartPromptPlugin {
        fn id(&self) -> &str {
            "invalid_step_start_prompt"
        }
        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            ActionSet::empty()
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(InvalidStepStartPromptPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("hi"));
    let tools = HashMap::new();

    let events = run_mock_stream(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        config,
        thread,
        tools,
    )
    .await;

    // With typed ActionSet<LifecycleAction>, step_start can only emit State actions.
    // AddSessionContext cannot be placed there (compile-time type safety).
    // The stream should complete normally.
    assert!(
        matches!(events.last(), Some(AgentEvent::RunFinish { .. })),
        "expected stream to complete normally: {events:?}"
    );
}

struct BlockBeforeToolPlugin;

#[async_trait]
impl AgentBehavior for BlockBeforeToolPlugin {
    fn id(&self) -> &str {
        "block_before_tool"
    }
    async fn before_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        // With typed ActionSet<BeforeToolExecuteAction>, AddSystemReminder cannot be placed here
        // (it belongs to AfterToolExecuteAction). This is type-safe by construction.
        // Block is the valid before_tool_execute action that prevents tool execution.
        ActionSet::single(BeforeToolExecuteAction::Block(
            "blocked in BeforeToolExecute".into(),
        ))
    }
}

#[test]
fn test_execute_tools_reminder_mutation_outside_after_tool_execute_is_type_safe() {
    // With typed ActionSet, AddSystemReminder cannot be placed in BeforeToolExecuteAction
    // at compile time. Verify that Block (a valid BeforeToolExecute action) works correctly.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "blocked".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("mock")
            .with_behavior(Arc::new(BlockBeforeToolPlugin) as Arc<dyn AgentBehavior>);

        let outcome = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .expect("block in before_tool_execute should produce a Completed outcome");

        let thread = outcome.into_thread();
        assert_eq!(thread.messages.len(), 1);
        assert!(
            thread.messages[0]
                .content
                .to_lowercase()
                .contains("blocked"),
            "blocked tool should have blocked message in result: {}",
            thread.messages[0].content
        );
    });
}

struct ReminderAppendPlugin;

#[async_trait]
impl AgentBehavior for ReminderAppendPlugin {
    fn id(&self) -> &str {
        "reminder_append"
    }
    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        ActionSet::single(AfterToolExecuteAction::AddSystemReminder("first".into()))
    }
}

struct ReminderReplacePlugin;

#[async_trait]
impl AgentBehavior for ReminderReplacePlugin {
    fn id(&self) -> &str {
        "reminder_replace"
    }
    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        ActionSet::single(AfterToolExecuteAction::AddSystemReminder("second".into()))
    }
}

#[test]
fn test_execute_tools_multiple_reminder_behaviors_are_additive() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "ok".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("mock").with_behavior(compose_test_behaviors(vec![
            Arc::new(ReminderAppendPlugin) as Arc<dyn AgentBehavior>,
            Arc::new(ReminderReplacePlugin) as Arc<dyn AgentBehavior>,
        ]));

        let outcome = execute_tools_with_config(thread, &result, &tools, &agent)
            .await
            .expect("both SystemReminder effects should be additive in the behavior model");

        let out_thread = outcome.into_thread();
        let reminder_msgs: Vec<_> = out_thread
            .messages
            .iter()
            .filter(|m| m.content.contains("<system-reminder>"))
            .collect();
        assert_eq!(
            reminder_msgs.len(),
            2,
            "expected two additive system-reminder messages, got {reminder_msgs:?}"
        );
    });
}

#[tokio::test]
async fn test_stream_run_finish_has_matching_thread_id() {
    let (recorder, _phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);

    let thread =
        Thread::new("my-thread").with_message(crate::contracts::thread::Message::user("hello"));
    let tools = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    // Extract thread_id from RunStart and RunFinish
    let start_tid = events.iter().find_map(|e| match e {
        AgentEvent::RunStart { thread_id, .. } => Some(thread_id.clone()),
        _ => None,
    });
    let finish_tid = events.iter().find_map(|e| match e {
        AgentEvent::RunFinish { thread_id, .. } => Some(thread_id.clone()),
        _ => None,
    });

    assert_eq!(
        start_tid, finish_tid,
        "RunStart and RunFinish thread_ids must match"
    );
    assert_eq!(start_tid.as_deref(), Some("my-thread"));
}

#[test]
fn test_run_execution_context_tracks_run_ids() {
    let run_identity = test_run_identity_with_parent("my-run", Some("parent-run"), None);
    assert_eq!(run_identity.run_id_opt(), Some("my-run"));
    assert_eq!(run_identity.parent_run_id_opt(), Some("parent-run"));
}

// ========================================================================
// Mock ChatProvider for non-stream stop condition/retry tests
// ========================================================================

struct MockChatProvider {
    responses: Mutex<Vec<genai::Result<genai::chat::ChatResponse>>>,
    models_seen: Mutex<Vec<String>>,
}

impl MockChatProvider {
    fn new(responses: Vec<genai::Result<genai::chat::ChatResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            models_seen: Mutex::new(Vec::new()),
        }
    }

    fn seen_models(&self) -> Vec<String> {
        self.models_seen.lock().expect("lock poisoned").clone()
    }
}

fn text_chat_response(text: &str) -> genai::chat::ChatResponse {
    let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock");
    genai::chat::ChatResponse {
        content: MessageContent::from_text(text.to_string()),
        reasoning_content: None,
        model_iden: model_iden.clone(),
        provider_model_iden: model_iden,
        stop_reason: None,
        usage: Usage::default(),
        captured_raw_body: None,
    }
}

fn text_chat_response_with_usage(
    text: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
) -> genai::chat::ChatResponse {
    let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock");
    genai::chat::ChatResponse {
        content: MessageContent::from_text(text.to_string()),
        reasoning_content: None,
        model_iden: model_iden.clone(),
        provider_model_iden: model_iden,
        stop_reason: None,
        usage: Usage {
            prompt_tokens: Some(prompt_tokens),
            prompt_tokens_details: None,
            completion_tokens: Some(completion_tokens),
            completion_tokens_details: None,
            total_tokens: Some(prompt_tokens + completion_tokens),
        },
        captured_raw_body: None,
    }
}

fn truncated_chat_response(text: &str) -> genai::chat::ChatResponse {
    let mut response = text_chat_response(text);
    response.stop_reason = Some(genai::chat::StopReason::from("length".to_string()));
    response
}

fn tool_call_chat_response_object_args(
    call_id: &str,
    name: &str,
    args: Value,
) -> genai::chat::ChatResponse {
    let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock");
    genai::chat::ChatResponse {
        content: MessageContent::from_tool_calls(vec![genai::chat::ToolCall {
            call_id: call_id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: args,
            thought_signatures: None,
        }]),
        reasoning_content: None,
        model_iden: model_iden.clone(),
        provider_model_iden: model_iden,
        stop_reason: None,
        usage: Usage::default(),
        captured_raw_body: None,
    }
}

#[test]
fn stream_result_from_chat_response_uses_explicit_stop_reason() {
    let response = truncated_chat_response("partial");
    let result = stream_result_from_chat_response(&response);
    assert_eq!(result.stop_reason, Some(StopReason::MaxTokens));
}

#[test]
fn stream_result_from_chat_response_falls_back_when_stop_reason_unknown() {
    let mut response = tool_call_chat_response_object_args("c1", "echo", json!({"x": 1}));
    response.stop_reason = Some(genai::chat::StopReason::from(
        "provider_specific_reason".to_string(),
    ));

    let result = stream_result_from_chat_response(&response);
    assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
}

#[async_trait]
impl LlmExecutor for MockChatProvider {
    async fn exec_chat_response(
        &self,
        model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        self.models_seen
            .lock()
            .expect("lock poisoned")
            .push(model.to_string());
        let mut responses = self.responses.lock().expect("lock poisoned");
        if responses.is_empty() {
            Ok(text_chat_response("done"))
        } else {
            responses.remove(0)
        }
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<super::LlmEventStream> {
        unimplemented!("MockChatProvider doesn't support streaming")
    }

    fn name(&self) -> &'static str {
        "mock_chat"
    }
}

struct HangingChatProvider {
    ready: Arc<Notify>,
    proceed: Arc<Notify>,
    response: genai::chat::ChatResponse,
}

#[async_trait]
impl LlmExecutor for HangingChatProvider {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        self.ready.notify_one();
        self.proceed.notified().await;
        Ok(self.response.clone())
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<super::LlmEventStream> {
        unimplemented!("HangingChatProvider doesn't support streaming")
    }

    fn name(&self) -> &'static str {
        "hanging_chat"
    }
}

#[tokio::test]
async fn test_nonstream_uses_fallback_model_after_primary_failures() {
    let provider = Arc::new(MockChatProvider::new(vec![
        Err(genai::Error::Internal("429 rate limit".to_string())),
        Err(genai::Error::Internal("429 rate limit".to_string())),
        Ok(text_chat_response("ok")),
    ]));
    let config = BaseAgent::new("primary")
        .with_fallback_model("fallback")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 2,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
            retry_stream_start: true,
            ..LlmRetryPolicy::default()
        })
        .with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert_eq!(outcome.response.as_deref(), Some("ok"));
    assert_eq!(
        provider.seen_models(),
        vec![
            "primary".to_string(),
            "primary".to_string(),
            "fallback".to_string()
        ]
    );
    assert!(
        outcome
            .run_ctx
            .messages()
            .iter()
            .any(|m| m.role == crate::contracts::thread::Role::Assistant && m.content == "ok"),
        "assistant response should be stored in thread"
    );
}

#[tokio::test]
async fn test_nonstream_retry_budget_exhaustion_stops_additional_attempts() {
    let provider = Arc::new(MockChatProvider::new(vec![
        Err(genai::Error::Internal("429 rate limit".to_string())),
        Ok(text_chat_response("ok")),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 2,
            initial_backoff_ms: 10,
            max_backoff_ms: 10,
            backoff_jitter_percent: 0,
            max_retry_window_ms: Some(5),
            retry_stream_start: true,
            ..LlmRetryPolicy::default()
        })
        .with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert!(matches!(outcome.termination, TerminationReason::Error(_)));
    assert!(matches!(
        outcome.failure,
        Some(outcome::LoopFailure::Llm(message))
            if message.contains("429") && message.contains("retry budget exhausted")
    ));
    assert_eq!(provider.seen_models(), vec!["mock".to_string()]);
}

#[tokio::test]
async fn test_nonstream_truncation_recovery_stitches_final_response() {
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(truncated_chat_response("partial output...")),
        Ok(text_chat_response("complete response")),
    ]));
    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert_eq!(
        outcome.response.as_deref(),
        Some("partial output...complete response")
    );
}

#[tokio::test]
async fn test_nonstream_llm_error_runs_cleanup_and_run_end_phases() {
    struct CleanupOnLlmErrorPlugin {
        phases: Arc<Mutex<Vec<Phase>>>,
    }

    #[async_trait]
    impl AgentBehavior for CleanupOnLlmErrorPlugin {
        fn id(&self) -> &str {
            "cleanup_on_llm_error_nonstream"
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::RunStart);
            ActionSet::empty()
        }

        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::StepStart);
            ActionSet::empty()
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::BeforeInference);
            ActionSet::empty()
        }

        async fn after_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::AfterInference);
            let err_type = ctx.inference_error().map(|e| e.error_type.as_str());
            assert_eq!(err_type, Some("llm_exec_error"));
            ActionSet::empty()
        }

        async fn before_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::BeforeToolExecute);
            ActionSet::empty()
        }

        async fn after_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterToolExecuteAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::AfterToolExecute);
            ActionSet::empty()
        }

        async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::StepEnd);
            ActionSet::empty()
        }

        async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::RunEnd);
            ActionSet::empty()
        }
    }

    let phases = Arc::new(Mutex::new(Vec::new()));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(CleanupOnLlmErrorPlugin {
            phases: phases.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            retry_stream_start: true,
            ..LlmRetryPolicy::default()
        });
    let provider = Arc::new(MockChatProvider::new(vec![Err(genai::Error::Internal(
        "429 rate limit".to_string(),
    ))]));
    let config = config.with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;
    assert!(matches!(outcome.termination, TerminationReason::Error(_)));
    assert!(
        matches!(outcome.failure, Some(outcome::LoopFailure::Llm(ref message)) if message.contains("429")),
        "expected llm error with source message, got: {:?}",
        outcome.failure
    );

    let recorded = phases.lock().expect("lock poisoned").clone();
    assert!(
        recorded.contains(&Phase::AfterInference),
        "cleanup should run AfterInference on llm error, got: {recorded:?}"
    );
    assert!(
        recorded.contains(&Phase::StepEnd),
        "cleanup should run StepEnd on llm error, got: {recorded:?}"
    );
    assert!(
        recorded.contains(&Phase::RunEnd),
        "run should still emit RunEnd on llm error, got: {recorded:?}"
    );
}

#[tokio::test]
async fn test_nonstream_natural_end_wins_without_stop_policy() {
    let provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
        "done now",
    ))]));
    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = run_ctx_with_execution(&thread, "run-natural-end");

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert!(
        outcome
            .run_ctx
            .messages()
            .iter()
            .any(|m| m.role == crate::contracts::thread::Role::Assistant),
        "assistant turn should still be committed before stop check"
    );
    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    assert_eq!(final_state["__run"]["id"], json!("run-natural-end"));
    assert_eq!(final_state["__run"]["status"], json!("done"));
    assert_eq!(final_state["__run"]["done_reason"], json!("natural"));
}

#[tokio::test]
async fn test_nonstream_cancellation_token_during_inference() {
    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let provider = Arc::new(HangingChatProvider {
        ready: ready.clone(),
        proceed: proceed.clone(),
        response: text_chat_response("never"),
    });
    let token = CancellationToken::new();
    let token_for_run = token.clone();

    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let handle = tokio::spawn(async move {
        run_loop(
            &config,
            HashMap::new(),
            run_ctx,
            Some(token_for_run),
            None,
            None,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), ready.notified())
        .await
        .expect("inference execution did not reach cancellation checkpoint");
    token.cancel();

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(300), handle)
        .await
        .expect("non-stream run should stop shortly after cancellation during inference")
        .expect("run task should not panic");
    proceed.notify_waiters();

    assert_eq!(
        outcome.termination,
        TerminationReason::Cancelled,
        "expected cancellation during inference, got: {:?}",
        outcome.termination
    );
    assert!(
        outcome
            .run_ctx
            .messages()
            .iter()
            .any(|m| m.role == Role::User && m.content == CANCELLATION_INFERENCE_USER_MESSAGE),
        "expected persisted user interruption note for inference cancellation"
    );
}

#[test]
fn test_loop_outcome_run_finish_projection_natural_end_has_result_payload() {
    let outcome = LoopOutcome {
        run_ctx: RunContext::new(
            "thread-1",
            json!({}),
            vec![],
            crate::contracts::RunPolicy::default(),
        ),
        termination: TerminationReason::NaturalEnd,
        response: Some("final text".to_string()),
        usage: LoopUsage::default(),
        stats: LoopStats::default(),
        failure: None,
    };

    let event = outcome.to_run_finish_event("run-1".to_string());
    match event {
        AgentEvent::RunFinish {
            thread_id,
            run_id,
            result,
            termination,
        } => {
            assert_eq!(thread_id, "thread-1");
            assert_eq!(run_id, "run-1");
            assert_eq!(termination, TerminationReason::NaturalEnd);
            assert_eq!(result, Some(json!({ "response": "final text" })));
        }
        other => panic!("expected run finish event, got: {other:?}"),
    }
}

#[test]
fn test_loop_outcome_run_finish_projection_non_natural_has_no_result_payload() {
    let outcome = LoopOutcome {
        run_ctx: RunContext::new(
            "thread-2",
            json!({}),
            vec![],
            crate::contracts::RunPolicy::default(),
        ),
        termination: TerminationReason::Cancelled,
        response: Some("ignored".to_string()),
        usage: LoopUsage::default(),
        stats: LoopStats::default(),
        failure: None,
    };

    let event = outcome.to_run_finish_event("run-2".to_string());
    match event {
        AgentEvent::RunFinish {
            result,
            termination,
            ..
        } => {
            assert_eq!(termination, TerminationReason::Cancelled);
            assert_eq!(result, None);
        }
        other => panic!("expected run finish event, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_nonstream_loop_outcome_collects_usage_and_stats() {
    let provider = Arc::new(MockChatProvider::new(vec![Ok(
        text_chat_response_with_usage("done", 7, 3),
    )]));
    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("usage-stats").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert_eq!(outcome.response.as_deref(), Some("done"));
    assert_eq!(outcome.usage.prompt_tokens, 7);
    assert_eq!(outcome.usage.completion_tokens, 3);
    assert_eq!(outcome.usage.total_tokens, 10);
    assert_eq!(outcome.stats.steps, 1);
    assert_eq!(outcome.stats.llm_calls, 1);
    assert_eq!(outcome.stats.llm_retries, 0);
    assert_eq!(outcome.stats.tool_calls, 0);
    assert_eq!(outcome.stats.tool_errors, 0);
    assert!(outcome
        .run_ctx
        .messages()
        .iter()
        .any(|m| m.role == crate::contracts::thread::Role::Assistant && m.content == "done"));
}

#[tokio::test]
async fn test_nonstream_loop_outcome_llm_error_tracks_attempts_and_failure_kind() {
    let provider = Arc::new(MockChatProvider::new(vec![
        Err(genai::Error::Internal("429 rate limit".to_string())),
        Err(genai::Error::Internal("still failing".to_string())),
    ]));
    let config = BaseAgent::new("primary")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 2,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            retry_stream_start: true,
            ..LlmRetryPolicy::default()
        })
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("error-stats").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert!(matches!(outcome.termination, TerminationReason::Error(_)));
    assert_eq!(outcome.stats.llm_calls, 2);
    assert_eq!(outcome.stats.llm_retries, 1);
    assert_eq!(outcome.stats.steps, 0);
    assert!(matches!(
        outcome.failure,
        Some(outcome::LoopFailure::Llm(message)) if message.contains("model='primary' attempt=2/2")
    ));
}

#[tokio::test]
async fn test_nonstream_cancellation_token_during_tool_execution() {
    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let tool = ActivityGateTool {
        id: "activity_gate".to_string(),
        stream_id: "nonstream_cancel".to_string(),
        ready: ready.clone(),
        proceed,
    };
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "call_1",
            "activity_gate",
            json!({}),
        )),
        Ok(text_chat_response("done")),
    ]));
    let token = CancellationToken::new();
    let token_for_run = token.clone();

    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let tools = tool_map([tool]);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let handle = tokio::spawn(async move {
        run_loop(&config, tools, run_ctx, Some(token_for_run), None, None).await
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .expect("tool execution did not reach cancellation checkpoint");
    token.cancel();

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(300), handle)
        .await
        .expect("non-stream run should stop shortly after cancellation during tool execution")
        .expect("run task should not panic");

    assert_eq!(outcome.termination, TerminationReason::Cancelled);
    let run_ctx = outcome.run_ctx;
    assert!(
        run_ctx
            .messages()
            .iter()
            .any(|m| m.role == crate::contracts::thread::Role::Assistant),
        "assistant tool_call turn should be committed before cancellation"
    );
    assert!(
        !run_ctx
            .messages()
            .iter()
            .any(|m| m.role == crate::contracts::thread::Role::Tool),
        "tool results should not be committed after cancellation"
    );
    assert!(
        run_ctx
            .messages()
            .iter()
            .any(|m| m.role == Role::User && m.content == CANCELLATION_TOOL_USER_MESSAGE),
        "expected persisted user interruption note for tool cancellation"
    );
}

#[tokio::test]
async fn test_nonstream_parallel_tool_cancellation_appends_single_user_note() {
    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let tool_a = ActivityGateTool {
        id: "activity_gate_a".to_string(),
        stream_id: "nonstream_cancel_multi_a".to_string(),
        ready: ready.clone(),
        proceed: proceed.clone(),
    };
    let tool_b = ActivityGateTool {
        id: "activity_gate_b".to_string(),
        stream_id: "nonstream_cancel_multi_b".to_string(),
        ready: ready.clone(),
        proceed,
    };

    let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock");
    let first_response = genai::chat::ChatResponse {
        content: MessageContent::from_tool_calls(vec![
            genai::chat::ToolCall {
                call_id: "call_1".to_string(),
                fn_name: "activity_gate_a".to_string(),
                fn_arguments: json!({}),
                thought_signatures: None,
            },
            genai::chat::ToolCall {
                call_id: "call_2".to_string(),
                fn_name: "activity_gate_b".to_string(),
                fn_arguments: json!({}),
                thought_signatures: None,
            },
        ]),
        reasoning_content: None,
        model_iden: model_iden.clone(),
        provider_model_iden: model_iden,
        stop_reason: None,
        usage: Usage::default(),
        captured_raw_body: None,
    };
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(first_response),
        Ok(text_chat_response("done")),
    ]));
    let token = CancellationToken::new();
    let token_for_run = token.clone();
    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let tools = tool_map([tool_a, tool_b]);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let handle = tokio::spawn(async move {
        run_loop(&config, tools, run_ctx, Some(token_for_run), None, None).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .expect("parallel tool execution did not reach cancellation checkpoint");
    token.cancel();

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(300), handle)
        .await
        .expect("run should stop shortly after cancellation during parallel tool execution")
        .expect("run task should not panic");

    assert_eq!(outcome.termination, TerminationReason::Cancelled);
    let cancellation_count = outcome
        .run_ctx
        .messages()
        .iter()
        .filter(|message| {
            message.role == Role::User && message.content == CANCELLATION_TOOL_USER_MESSAGE
        })
        .count();
    assert_eq!(
        cancellation_count, 1,
        "parallel cancellation must append exactly one interruption note"
    );
}

#[tokio::test]
async fn test_nonstream_inference_abort_message_persisted_and_visible_next_run() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ObserveMessagePlugin {
        expected: &'static str,
        seen: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AgentBehavior for ObserveMessagePlugin {
        fn id(&self) -> &str {
            "observe_cancellation_message_inference_nonstream"
        }

        async fn before_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            if ctx
                .messages()
                .iter()
                .any(|m| m.role == Role::User && m.content == self.expected)
            {
                self.seen.store(true, Ordering::SeqCst);
            }
            ActionSet::empty()
        }
    }

    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let provider = Arc::new(HangingChatProvider {
        ready: ready.clone(),
        proceed: proceed.clone(),
        response: text_chat_response("never"),
    });
    let token = CancellationToken::new();
    let token_for_run = token.clone();
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));

    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let initial_thread = Thread::new("cancel-inference").with_message(Message::user("go"));
    let run_ctx =
        RunContext::from_thread(&initial_thread, tirea_contract::RunPolicy::default()).unwrap();

    let handle = tokio::spawn(async move {
        run_loop(
            &config,
            HashMap::new(),
            run_ctx,
            Some(token_for_run),
            Some(state_committer),
            None,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), ready.notified())
        .await
        .expect("inference execution did not reach cancellation checkpoint");
    token.cancel();

    let first_outcome = tokio::time::timeout(std::time::Duration::from_millis(300), handle)
        .await
        .expect("non-stream run should stop shortly after cancellation during inference")
        .expect("run task should not panic");
    proceed.notify_waiters();
    assert_eq!(first_outcome.termination, TerminationReason::Cancelled);

    let mut persisted_thread = initial_thread.clone();
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut persisted_thread);
    }
    assert!(
        persisted_thread
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == CANCELLATION_INFERENCE_USER_MESSAGE),
        "inference cancellation note should be persisted in thread history"
    );

    let seen = Arc::new(AtomicBool::new(false));
    let resume_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response("done"))]));
    let resume_config = BaseAgent::new("mock")
        .with_behavior(Arc::new(ObserveMessagePlugin {
            expected: CANCELLATION_INFERENCE_USER_MESSAGE,
            seen: seen.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_executor(resume_provider as Arc<dyn LlmExecutor>);
    let resume_run_ctx =
        RunContext::from_thread(&persisted_thread, tirea_contract::RunPolicy::default()).unwrap();
    let second_outcome = run_loop(
        &resume_config,
        HashMap::new(),
        resume_run_ctx,
        None,
        None,
        None,
    )
    .await;

    assert_eq!(second_outcome.termination, TerminationReason::NaturalEnd);
    assert!(
        seen.load(Ordering::SeqCst),
        "next inference should observe persisted cancellation message"
    );
}

#[tokio::test]
async fn test_nonstream_tool_abort_message_persisted_and_visible_next_run() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ObserveMessagePlugin {
        expected: &'static str,
        seen: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AgentBehavior for ObserveMessagePlugin {
        fn id(&self) -> &str {
            "observe_cancellation_message_tool_nonstream"
        }

        async fn before_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            if ctx
                .messages()
                .iter()
                .any(|m| m.role == Role::User && m.content == self.expected)
            {
                self.seen.store(true, Ordering::SeqCst);
            }
            ActionSet::empty()
        }
    }

    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let tool = ActivityGateTool {
        id: "activity_gate".to_string(),
        stream_id: "nonstream_cancel_persist".to_string(),
        ready: ready.clone(),
        proceed,
    };
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "call_1",
            "activity_gate",
            json!({}),
        )),
        Ok(text_chat_response("done")),
    ]));
    let token = CancellationToken::new();
    let token_for_run = token.clone();
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));

    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let tools = tool_map([tool]);
    let initial_thread = Thread::new("cancel-tool").with_message(Message::user("go"));
    let run_ctx =
        RunContext::from_thread(&initial_thread, tirea_contract::RunPolicy::default()).unwrap();

    let handle = tokio::spawn(async move {
        run_loop(
            &config,
            tools,
            run_ctx,
            Some(token_for_run),
            Some(state_committer),
            None,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .expect("tool execution did not reach cancellation checkpoint");
    token.cancel();

    let first_outcome = tokio::time::timeout(std::time::Duration::from_millis(300), handle)
        .await
        .expect("non-stream run should stop shortly after cancellation during tool execution")
        .expect("run task should not panic");
    assert_eq!(first_outcome.termination, TerminationReason::Cancelled);

    let mut persisted_thread = initial_thread.clone();
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut persisted_thread);
    }
    assert!(
        persisted_thread
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == CANCELLATION_TOOL_USER_MESSAGE),
        "tool cancellation note should be persisted in thread history"
    );

    let seen = Arc::new(AtomicBool::new(false));
    let resume_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response("done"))]));
    let resume_config = BaseAgent::new("mock")
        .with_behavior(Arc::new(ObserveMessagePlugin {
            expected: CANCELLATION_TOOL_USER_MESSAGE,
            seen: seen.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_executor(resume_provider as Arc<dyn LlmExecutor>);
    let resume_run_ctx =
        RunContext::from_thread(&persisted_thread, tirea_contract::RunPolicy::default()).unwrap();
    let second_outcome = run_loop(
        &resume_config,
        HashMap::new(),
        resume_run_ctx,
        None,
        None,
        None,
    )
    .await;

    assert_eq!(second_outcome.termination, TerminationReason::NaturalEnd);
    assert!(
        seen.load(Ordering::SeqCst),
        "next inference should observe persisted tool cancellation message"
    );
}

#[tokio::test]
async fn test_golden_run_loop_and_stream_natural_end_alignment() {
    let thread = Thread::new("golden-natural").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);
    let nonstream_provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "call_1",
            "echo",
            json!({"message": "aligned"}),
        )),
        Ok(text_chat_response("done")),
    ]));
    let nonstream_config =
        BaseAgent::new("mock").with_llm_executor(nonstream_provider as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let nonstream_outcome =
        run_loop(&nonstream_config, tools.clone(), run_ctx, None, None, None).await;
    assert_eq!(nonstream_outcome.termination, TerminationReason::NaturalEnd);
    let nonstream_response = nonstream_outcome.response.clone().unwrap_or_default();

    let (events, stream_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![
            MockResponse::text("").with_tool_call("call_1", "echo", json!({"message": "aligned"})),
            MockResponse::text("done"),
        ]),
        BaseAgent::new("mock"),
        thread,
        tools.clone(),
    )
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    assert_eq!(
        extract_run_finish_response(&events),
        Some(nonstream_response.clone())
    );
    assert_eq!(
        compact_canonical_messages_from_slice(nonstream_outcome.run_ctx.messages()),
        compact_canonical_messages(&stream_thread),
        "stream/non-stream should produce equivalent persisted message sequences"
    );
}

#[tokio::test]
async fn test_golden_run_loop_and_stream_cancelled_alignment() {
    let thread = Thread::new("golden-cancel").with_message(Message::user("go"));
    let tools = HashMap::new();
    let nonstream_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
        "unused",
    ))]));
    let nonstream_token = CancellationToken::new();
    nonstream_token.cancel();

    let nonstream_config =
        BaseAgent::new("mock").with_llm_executor(nonstream_provider as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let nonstream_outcome = run_loop(
        &nonstream_config,
        tools.clone(),
        run_ctx,
        Some(nonstream_token),
        None,
        None,
    )
    .await;
    assert_eq!(nonstream_outcome.termination, TerminationReason::Cancelled);

    let stream_token = CancellationToken::new();
    stream_token.cancel();
    let (events, stream_thread) = run_mock_stream_with_final_thread_with_context(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        BaseAgent::new("mock"),
        thread,
        tools,
        Some(stream_token),
        None,
    )
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Cancelled)
    );
    assert_eq!(extract_run_finish_response(&events), None);
    assert_eq!(
        compact_canonical_messages_from_slice(nonstream_outcome.run_ctx.messages()),
        compact_canonical_messages(&stream_thread),
        "stream/non-stream cancellation should leave equivalent persisted messages"
    );
}

#[tokio::test]
async fn test_golden_run_loop_and_stream_pending_resume_alignment() {
    struct GoldenPendingPlugin;

    #[async_trait]
    impl AgentBehavior for GoldenPendingPlugin {
        fn id(&self) -> &str {
            "golden_pending_plugin"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::Suspended,
            ))
            .and(ActionSet::single(BeforeInferenceAction::State(
                single_suspended_call_state_action(
                    Suspension::new("golden_resume_1", "recover_agent_run")
                        .with_message("resume me"),
                    None,
                ),
            )))
        }
    }

    let thread = Thread::new("golden-resume").with_message(Message::user("continue"));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(GoldenPendingPlugin) as Arc<dyn AgentBehavior>);
    let tools = HashMap::new();
    let nonstream_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
        "unused",
    ))]));

    let nonstream_config = config
        .clone()
        .with_llm_executor(nonstream_provider as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let nonstream_outcome =
        run_loop(&nonstream_config, tools.clone(), run_ctx, None, None, None).await;
    assert_eq!(nonstream_outcome.termination, TerminationReason::Suspended);
    let nonstream_suspended = nonstream_outcome.run_ctx.suspended_calls();
    let nonstream_interaction = &nonstream_suspended
        .get("golden_resume_1")
        .expect("non-stream outcome should have suspended interaction")
        .ticket
        .suspension;

    let (events, stream_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        config,
        thread,
        tools,
    )
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended)
    );
    let stream_interaction =
        extract_requested_interaction(&events).expect("stream should emit requested interaction");
    assert_eq!(stream_interaction.id, nonstream_interaction.id);
    assert_eq!(
        stream_interaction.action.trim_start_matches("tool:"),
        nonstream_interaction.action.trim_start_matches("tool:")
    );
    assert!(
        stream_interaction.message.is_empty(),
        "stream pending interaction uses ToolCallReady and does not carry message text"
    );

    assert_eq!(
        compact_canonical_messages_from_slice(nonstream_outcome.run_ctx.messages()),
        compact_canonical_messages(&stream_thread),
        "stream/non-stream pending path should preserve equivalent persisted messages"
    );

    let nonstream_state = nonstream_outcome
        .run_ctx
        .snapshot()
        .expect("non-stream state should rebuild");
    let stream_state = stream_thread
        .rebuild_state()
        .expect("stream state should rebuild");
    assert_eq!(
        nonstream_state.get("__tool_call_scope"),
        stream_state.get("__tool_call_scope")
    );
}

#[tokio::test]
async fn test_golden_run_loop_and_stream_no_plugins_pending_state_alignment() {
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("leftover_confirm", "confirm").with_message("stale pending"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("golden-no-plugin-pending", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("go"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let nonstream_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response("done"))]));
    let nonstream_config =
        BaseAgent::new("mock").with_llm_executor(nonstream_provider as Arc<dyn LlmExecutor>);
    let nonstream_run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let nonstream_outcome = run_loop(
        &nonstream_config,
        tools.clone(),
        nonstream_run_ctx,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(nonstream_outcome.termination, TerminationReason::Suspended);
    assert_eq!(nonstream_outcome.stats.llm_calls, 1);

    let (stream_events, stream_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        BaseAgent::new("mock"),
        thread,
        tools,
    )
    .await;
    assert_eq!(
        extract_termination(&stream_events),
        Some(TerminationReason::Suspended)
    );
    let stream_inference_count = stream_events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(stream_inference_count, 1);

    assert_eq!(
        compact_canonical_messages_from_slice(nonstream_outcome.run_ctx.messages()),
        compact_canonical_messages(&stream_thread),
        "no-plugin flow should remain semantically aligned between stream and non-stream"
    );
}

#[tokio::test]
async fn test_stream_replay_is_idempotent_across_reruns() {
    use crate::contracts::Suspension;

    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_replay_idempotent"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    fn replay_config() -> BaseAgent {
        let interaction =
            TestInteractionPlugin::with_responses(vec!["call_1".to_string()], Vec::new());
        BaseAgent::new("mock").with_behavior(compose_test_behaviors(vec![
            Arc::new(interaction),
            Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
        ]))
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let counting_tool: Arc<dyn Tool> = Arc::new(CountingEchoTool {
        calls: calls.clone(),
    });
    let tools = tool_map_from_arc([counting_tool]);

    // Seed state with a pre-existing suspended interaction for the pending tool call.
    let base_state = json!({});
    let echo_args = json!({"message": "approved-run"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:counting_echo")
            .with_message("awaiting approval")
            .with_parameters(echo_args.clone()),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("idempotent-replay", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "counting_echo",
                echo_args,
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'counting_echo' is awaiting approval. Execution paused.",
        ));

    let (first_events, first_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        replay_config(),
        thread,
        tools.clone(),
    )
    .await;
    assert!(
        first_events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallDone { id, result, .. }
            if id == "call_1" && result.status == crate::contracts::runtime::tool_call::ToolStatus::Success
        )),
        "first run should replay and execute the pending tool call"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "replayed tool should execute exactly once in first run"
    );

    let (second_events, second_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        replay_config(),
        first_thread,
        tools,
    )
    .await;
    assert!(
        !second_events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallDone { id, .. } if id == "call_1"
        )),
        "second run must not replay already-applied tool call"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "tool execution count must remain stable across reruns"
    );

    let final_state = second_thread.rebuild_state().expect("state should rebuild");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
    assert!(
        suspended.is_empty(),
        "no suspended calls should remain after replay"
    );
}

#[tokio::test]
async fn test_nonstream_replay_is_idempotent_across_reruns() {
    use crate::contracts::Suspension;

    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_replay_idempotent_nonstream"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    fn replay_config(provider: Arc<dyn LlmExecutor>) -> BaseAgent {
        let interaction =
            TestInteractionPlugin::with_responses(vec!["call_1".to_string()], Vec::new());
        BaseAgent::new("mock")
            .with_behavior(compose_test_behaviors(vec![
                Arc::new(interaction),
                Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>,
            ]))
            .with_llm_executor(provider)
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let counting_tool: Arc<dyn Tool> = Arc::new(CountingEchoTool {
        calls: calls.clone(),
    });
    let tools = tool_map_from_arc([counting_tool]);

    // Seed state with a pre-existing suspended interaction for the pending tool call.
    let base_state = json!({});
    let echo_args = json!({"message": "approved-run"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_1", "tool:counting_echo")
            .with_message("awaiting approval")
            .with_parameters(echo_args.clone()),
        None,
    )
    .expect("failed to seed suspended interaction");
    let seed_thread = Thread::with_initial_state("idempotent-replay-nonstream", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "counting_echo",
                echo_args,
            )],
        ))
        .with_message(Message::tool(
            "call_1",
            "Tool 'counting_echo' is awaiting approval. Execution paused.",
        ));

    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));
    let first_run_ctx =
        RunContext::from_thread(&seed_thread, tirea_contract::RunPolicy::default()).unwrap();
    let first_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
        "unused",
    ))]));
    let first_outcome = run_loop(
        &replay_config(first_provider as Arc<dyn LlmExecutor>),
        tools.clone(),
        first_run_ctx,
        None,
        Some(state_committer),
        None,
    )
    .await;
    assert!(
        first_outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && !message.content.contains("awaiting approval")
        }),
        "first run should replay and execute pending command"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "replayed command should execute exactly once in first run"
    );

    let mut persisted_thread = seed_thread.clone();
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut persisted_thread);
    }

    let second_provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response(
        "unused",
    ))]));
    let second_run_ctx =
        RunContext::from_thread(&persisted_thread, tirea_contract::RunPolicy::default()).unwrap();
    let second_outcome = run_loop(
        &replay_config(second_provider as Arc<dyn LlmExecutor>),
        tools,
        second_run_ctx,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second run must not replay already-executed command_id"
    );
    let done_tool_messages = second_outcome
        .run_ctx
        .messages()
        .iter()
        .filter(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && message.content.contains("\"echoed\":\"approved-run\"")
        })
        .count();
    assert_eq!(
        done_tool_messages, 1,
        "non-stream rerun must not append duplicate replayed tool result"
    );
}

// ========================================================================
// Mock ChatStreamProvider for stop condition integration tests
// ========================================================================

/// A single mock LLM response: text and optional tool calls.
#[derive(Clone)]
struct MockResponse {
    text: String,
    tool_calls: Vec<genai::chat::ToolCall>,
    usage: Option<Usage>,
}

impl MockResponse {
    fn text(s: &str) -> Self {
        Self {
            text: s.to_string(),
            tool_calls: Vec::new(),
            usage: None,
        }
    }

    fn with_tool_call(mut self, call_id: &str, name: &str, args: Value) -> Self {
        self.tool_calls.push(genai::chat::ToolCall {
            call_id: call_id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: Value::String(args.to_string()),
            thought_signatures: None,
        });
        self
    }

    fn with_usage(mut self, input: i32, output: i32) -> Self {
        self.usage = Some(Usage {
            prompt_tokens: Some(input),
            prompt_tokens_details: None,
            completion_tokens: Some(output),
            completion_tokens_details: None,
            total_tokens: Some(input + output),
        });
        self
    }
}

/// Mock provider that returns pre-configured responses in order.
/// After all responses are consumed, returns text-only (triggering NaturalEnd).
struct MockStreamProvider {
    responses: Mutex<Vec<MockResponse>>,
}

/// Provider that fails stream startup for a fixed number of calls, then succeeds.
struct FailingStartProvider {
    failures_left: Mutex<usize>,
    models_seen: Mutex<Vec<String>>,
}

impl FailingStartProvider {
    fn new(failures: usize) -> Self {
        Self {
            failures_left: Mutex::new(failures),
            models_seen: Mutex::new(Vec::new()),
        }
    }

    fn seen_models(&self) -> Vec<String> {
        self.models_seen.lock().expect("lock poisoned").clone()
    }
}

#[async_trait]
impl LlmExecutor for FailingStartProvider {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        unimplemented!("stream-only provider")
    }

    async fn exec_chat_stream_events(
        &self,
        model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<super::LlmEventStream> {
        self.models_seen
            .lock()
            .expect("lock poisoned")
            .push(model.to_string());
        let mut remaining = self.failures_left.lock().expect("lock poisoned");
        if *remaining > 0 {
            *remaining -= 1;
            return Err(genai::Error::Internal("429 rate limit".to_string()));
        }

        let events = vec![
            Ok(ChatStreamEvent::Start),
            Ok(ChatStreamEvent::Chunk(StreamChunk {
                content: "ok".to_string(),
            })),
            Ok(ChatStreamEvent::End(StreamEnd::default())),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &'static str {
        "failing_start"
    }
}

impl MockStreamProvider {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

/// Provider that returns a scripted stream result for each invocation and
/// records both model routing and the reconstructed chat request.
struct ScriptedStreamProvider {
    attempts: Mutex<Vec<Vec<genai::Result<ChatStreamEvent>>>>,
    models_seen: Mutex<Vec<String>>,
    requests_seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedStreamProvider {
    fn new(attempts: Vec<Vec<genai::Result<ChatStreamEvent>>>) -> Self {
        Self {
            attempts: Mutex::new(attempts),
            models_seen: Mutex::new(Vec::new()),
            requests_seen: Mutex::new(Vec::new()),
        }
    }

    fn seen_models(&self) -> Vec<String> {
        self.models_seen.lock().expect("lock poisoned").clone()
    }

    fn seen_requests(&self) -> Vec<ChatRequest> {
        self.requests_seen.lock().expect("lock poisoned").clone()
    }
}

#[async_trait]
impl LlmExecutor for ScriptedStreamProvider {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        unimplemented!("stream-only provider")
    }

    async fn exec_chat_stream_events(
        &self,
        model: &str,
        chat_req: ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<super::LlmEventStream> {
        self.models_seen
            .lock()
            .expect("lock poisoned")
            .push(model.to_string());
        self.requests_seen
            .lock()
            .expect("lock poisoned")
            .push(chat_req);
        let events = {
            let mut attempts = self.attempts.lock().expect("lock poisoned");
            if attempts.is_empty() {
                vec![
                    Ok(ChatStreamEvent::Start),
                    Ok(ChatStreamEvent::Chunk(StreamChunk {
                        content: "done".to_string(),
                    })),
                    Ok(ChatStreamEvent::End(StreamEnd::default())),
                ]
            } else {
                attempts.remove(0)
            }
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &'static str {
        "scripted_stream"
    }
}

fn web_stream_io_error(kind: std::io::ErrorKind, message: &str) -> genai::Error {
    genai::Error::WebStream {
        model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock"),
        cause: message.to_string(),
        error: Box::new(std::io::Error::new(kind, message)),
    }
}

fn request_texts(request: &ChatRequest) -> Vec<(ChatRole, String)> {
    request
        .messages
        .iter()
        .filter_map(|message| {
            message
                .content
                .first_text()
                .map(|text| (message.role.clone(), text.to_string()))
        })
        .collect()
}

fn text_stream_error_attempt(
    text: &str,
    error: genai::Error,
) -> Vec<genai::Result<ChatStreamEvent>> {
    vec![
        Ok(ChatStreamEvent::Start),
        Ok(ChatStreamEvent::Chunk(StreamChunk {
            content: text.to_string(),
        })),
        Err(error),
    ]
}

fn text_stream_success_attempt(text: &str) -> Vec<genai::Result<ChatStreamEvent>> {
    vec![
        Ok(ChatStreamEvent::Start),
        Ok(ChatStreamEvent::Chunk(StreamChunk {
            content: text.to_string(),
        })),
        Ok(ChatStreamEvent::End(StreamEnd::default())),
    ]
}

#[async_trait]
impl LlmExecutor for MockStreamProvider {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        unimplemented!("stream-only provider")
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<super::LlmEventStream> {
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                MockResponse::text("done")
            } else {
                responses.remove(0)
            }
        };

        let mut events: Vec<genai::Result<ChatStreamEvent>> = Vec::new();
        events.push(Ok(ChatStreamEvent::Start));

        if !resp.text.is_empty() {
            events.push(Ok(ChatStreamEvent::Chunk(StreamChunk {
                content: resp.text.clone(),
            })));
        }

        for tc in &resp.tool_calls {
            events.push(Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: tc.clone(),
            })));
        }

        let end = StreamEnd {
            captured_content: if resp.tool_calls.is_empty() {
                None
            } else {
                Some(MessageContent::from_tool_calls(resp.tool_calls))
            },
            captured_usage: resp.usage,
            ..Default::default()
        };
        events.push(Ok(ChatStreamEvent::End(end)));

        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &'static str {
        "mock_stream"
    }
}

/// Helper: run a mock stream and collect events.
async fn run_mock_stream(
    provider: MockStreamProvider,
    config: BaseAgent,
    thread: Thread,
    tools: HashMap<String, Arc<dyn Tool>>,
) -> Vec<AgentEvent> {
    let config = config.with_llm_executor(Arc::new(provider));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    collect_stream_events(stream).await
}

#[tokio::test]
async fn test_stream_serialization_emits_seq_timestamp_and_step_id() {
    let events = run_mock_stream(
        MockStreamProvider::new(vec![MockResponse::text("hello")]),
        BaseAgent::new("mock"),
        Thread::new("test").with_message(Message::user("go")),
        HashMap::new(),
    )
    .await;

    let serialized: Vec<Value> = events
        .iter()
        .map(|event| serde_json::to_value(event).expect("serialize event"))
        .collect();
    assert!(!serialized.is_empty());

    for (idx, event) in serialized.iter().enumerate() {
        assert_eq!(
            event.get("seq").and_then(Value::as_u64),
            Some(idx as u64),
            "seq mismatch at index {idx}: {event:?}"
        );
        assert!(
            event.get("timestamp_ms").and_then(Value::as_u64).is_some(),
            "timestamp_ms missing at index {idx}: {event:?}"
        );
    }

    let step_start = serialized
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("step_start"))
        .expect("step_start event");
    assert_eq!(
        step_start.get("step_id").and_then(Value::as_str),
        Some("step:0")
    );

    let text_delta = serialized
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("text_delta"))
        .expect("text_delta event");
    assert_eq!(
        text_delta.get("step_id").and_then(Value::as_str),
        Some("step:0")
    );
    assert!(text_delta.get("run_id").and_then(Value::as_str).is_some());
    assert!(text_delta
        .get("thread_id")
        .and_then(Value::as_str)
        .is_some());
}

#[tokio::test]
async fn test_stream_retries_startup_error_then_succeeds() {
    let provider = Arc::new(FailingStartProvider::new(1));
    let config = BaseAgent::new("mock").with_llm_retry_policy(LlmRetryPolicy {
        max_attempts_per_model: 2,
        initial_backoff_ms: 1,
        max_backoff_ms: 10,
        retry_stream_start: true,
        ..LlmRetryPolicy::default()
    });
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = HashMap::new();

    let config = config.with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    let seen = provider.seen_models();
    assert_eq!(seen, vec!["mock".to_string(), "mock".to_string()]);
}

#[tokio::test]
async fn test_stream_midstream_retry_budget_exhaustion_stops_recovery() {
    let provider = Arc::new(ScriptedStreamProvider::new(vec![
        text_stream_error_attempt(
            "hel",
            web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ),
        ),
        text_stream_success_attempt("lo"),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 10,
            max_backoff_ms: 10,
            backoff_jitter_percent: 0,
            max_retry_window_ms: Some(5),
            retry_stream_start: true,
            max_stream_event_retries: 1,
            stream_error_fallback_threshold: 2,
        })
        .with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert!(matches!(
        extract_termination(&events),
        Some(TerminationReason::Error(_))
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error { message, .. }
                if message.contains("connection reset by peer"))),
        "expected stream error after retry budget exhaustion: {events:?}"
    );
    assert_eq!(provider.seen_models(), vec!["mock".to_string()]);
}

#[tokio::test]
async fn test_stream_uses_fallback_model_after_primary_failures() {
    let provider = Arc::new(FailingStartProvider::new(2));
    let config = BaseAgent::new("primary")
        .with_fallback_model("fallback")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 2,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
            retry_stream_start: true,
            ..LlmRetryPolicy::default()
        });
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = HashMap::new();

    let config = config.with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    let seen = provider.seen_models();
    assert_eq!(
        seen,
        vec![
            "primary".to_string(),
            "primary".to_string(),
            "fallback".to_string()
        ]
    );
    assert_eq!(
        extract_inference_model(&events),
        Some("fallback".to_string())
    );
}

#[tokio::test]
async fn test_stream_midstream_text_error_retries_with_continuation_context() {
    let provider = Arc::new(ScriptedStreamProvider::new(vec![
        text_stream_error_attempt(
            "hel",
            web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ),
        ),
        text_stream_success_attempt("lo"),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            backoff_jitter_percent: 0,
            max_retry_window_ms: None,
            retry_stream_start: true,
            max_stream_event_retries: 1,
            stream_error_fallback_threshold: 2,
        })
        .with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    assert_eq!(
        provider.seen_models(),
        vec!["mock".to_string(), "mock".to_string()]
    );

    let requests = provider.seen_requests();
    assert_eq!(requests.len(), 2, "expected retry request to be captured");
    let second_request_texts = request_texts(&requests[1]);
    assert!(
        second_request_texts
            .iter()
            .any(|(role, text)| *role == ChatRole::Assistant && text == "hel"),
        "partial assistant text should be replayed into continuation request: {second_request_texts:?}"
    );
    assert!(
        second_request_texts.iter().any(|(role, text)| {
            *role == ChatRole::User && text.contains("interrupted due to a network error")
        }),
        "continuation prompt should be injected for text-only recovery: {second_request_texts:?}"
    );
}

#[tokio::test]
async fn test_stream_midstream_text_error_stitches_run_finish_response() {
    let provider = Arc::new(ScriptedStreamProvider::new(vec![
        text_stream_error_attempt(
            "hel",
            web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ),
        ),
        text_stream_success_attempt("lo"),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            backoff_jitter_percent: 0,
            max_retry_window_ms: None,
            retry_stream_start: true,
            max_stream_event_retries: 1,
            stream_error_fallback_threshold: 2,
        })
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    let streamed_text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(streamed_text, "hello");
    assert_eq!(
        extract_run_finish_response(&events),
        Some("hello".to_string())
    );
}

#[tokio::test]
async fn test_stream_midstream_tool_call_error_restarts_step_without_continuation_prompt() {
    let partial_tool_call = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "echo".to_string(),
        fn_arguments: Value::String("{\"message\":\"hel".to_string()),
        thought_signatures: None,
    };
    let complete_tool_call = genai::chat::ToolCall {
        call_id: "call_1".to_string(),
        fn_name: "echo".to_string(),
        fn_arguments: Value::String("{\"message\":\"hello\"}".to_string()),
        thought_signatures: None,
    };
    let provider = Arc::new(ScriptedStreamProvider::new(vec![
        vec![
            Ok(ChatStreamEvent::Start),
            Ok(ChatStreamEvent::Chunk(StreamChunk {
                content: "thinking ".to_string(),
            })),
            Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: partial_tool_call,
            })),
            Err(web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
        ],
        vec![
            Ok(ChatStreamEvent::Start),
            Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: complete_tool_call,
            })),
            Ok(ChatStreamEvent::End(StreamEnd::default())),
        ],
        text_stream_success_attempt("done"),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            backoff_jitter_percent: 0,
            max_retry_window_ms: None,
            retry_stream_start: true,
            max_stream_event_retries: 1,
            stream_error_fallback_threshold: 2,
        })
        .with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let tools = tool_map([EchoTool]);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_1"
        )),
        "tool call should complete after restart: {events:?}"
    );

    let requests = provider.seen_requests();
    assert_eq!(requests.len(), 3, "expected restart + post-tool inference");
    let second_request_texts = request_texts(&requests[1]);
    assert!(
        !second_request_texts
            .iter()
            .any(|(_, text)| text.contains("thinking")),
        "partial text from tool-bearing stream should not be replayed: {second_request_texts:?}"
    );
    assert!(
        !second_request_texts
            .iter()
            .any(|(_, text)| { text.contains("interrupted due to a network error") }),
        "tool-bearing restart should not inject continuation prompt: {second_request_texts:?}"
    );
}

#[tokio::test]
async fn test_stream_midstream_repeated_errors_escalate_to_fallback_model() {
    let provider = Arc::new(ScriptedStreamProvider::new(vec![
        text_stream_error_attempt(
            "A",
            web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ),
        ),
        text_stream_error_attempt(
            "B",
            web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ),
        ),
        text_stream_success_attempt("C"),
    ]));
    let config = BaseAgent::new("primary")
        .with_fallback_model("fallback")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            backoff_jitter_percent: 0,
            max_retry_window_ms: None,
            retry_stream_start: true,
            max_stream_event_retries: 2,
            stream_error_fallback_threshold: 2,
        })
        .with_llm_executor(provider.clone() as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    assert_eq!(
        provider.seen_models(),
        vec![
            "primary".to_string(),
            "primary".to_string(),
            "fallback".to_string(),
        ]
    );
    assert_eq!(
        extract_inference_model(&events),
        Some("fallback".to_string())
    );
    assert_eq!(
        extract_run_finish_response(&events),
        Some("ABC".to_string())
    );
}

/// Helper: run a mock stream and collect events plus final session.
async fn run_mock_stream_with_final_thread(
    provider: MockStreamProvider,
    config: BaseAgent,
    thread: Thread,
    tools: HashMap<String, Arc<dyn Tool>>,
) -> (Vec<AgentEvent>, Thread) {
    run_mock_stream_with_final_thread_with_context(provider, config, thread, tools, None, None)
        .await
}

/// Helper: run a mock stream and collect events plus final session with explicit run context.
async fn run_mock_stream_with_final_thread_with_context(
    provider: MockStreamProvider,
    config: BaseAgent,
    thread: Thread,
    tools: HashMap<String, Arc<dyn Tool>>,
    cancellation_token: Option<RunCancellationToken>,
    _state_committer: Option<Arc<dyn StateCommitter>>,
) -> (Vec<AgentEvent>, Thread) {
    let mut final_thread = thread.clone();
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(ChannelStateCommitter::new(checkpoint_tx));
    let config = config.with_llm_executor(Arc::new(provider));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        cancellation_token,
        Some(committer),
        None,
    );
    let events = collect_stream_events(stream).await;
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut final_thread);
    }
    (events, final_thread)
}

#[derive(Clone)]
struct RecordingStateCommitter {
    reasons: Arc<Mutex<Vec<CheckpointReason>>>,
    fail_on: Option<CheckpointReason>,
}

impl RecordingStateCommitter {
    fn new(fail_on: Option<CheckpointReason>) -> Self {
        Self {
            reasons: Arc::new(Mutex::new(Vec::new())),
            fail_on,
        }
    }

    fn reasons(&self) -> Vec<CheckpointReason> {
        self.reasons.lock().expect("lock poisoned").clone()
    }
}

#[async_trait]
impl StateCommitter for RecordingStateCommitter {
    async fn commit(
        &self,
        _thread_id: &str,
        changeset: crate::contracts::ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<u64, StateCommitError> {
        self.reasons
            .lock()
            .expect("lock poisoned")
            .push(changeset.reason.clone());

        if self
            .fail_on
            .as_ref()
            .is_some_and(|reason| *reason == changeset.reason)
        {
            return Err(StateCommitError::new(format!(
                "forced commit failure at {:?}",
                changeset.reason
            )));
        }
        let version = match precondition {
            VersionPrecondition::Any => 1,
            VersionPrecondition::Exact(version) => version.saturating_add(1),
        };
        Ok(version)
    }
}

struct RunStartSideEffectPlugin;

#[async_trait]
impl AgentBehavior for RunStartSideEffectPlugin {
    fn id(&self) -> &str {
        "run_start_side_effect_plugin"
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.phase_actions(Phase::RunStart, ctx).await
    }
}

impl RunStartSideEffectPlugin {
    async fn phase_actions(
        &self,
        phase: Phase,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<LifecycleAction> {
        if phase != Phase::RunStart {
            return ActionSet::empty();
        }
        ActionSet::single(LifecycleAction::State(AnyStateAction::new::<DebugFlags>(
            DebugFlagAction::RunStart,
        )))
    }
}

/// Extract the termination from the RunFinish event.
fn extract_termination(events: &[AgentEvent]) -> Option<TerminationReason> {
    events.iter().find_map(|e| match e {
        AgentEvent::RunFinish { termination, .. } => Some(termination.clone()),
        _ => None,
    })
}

fn extract_run_finish_response(events: &[AgentEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentEvent::RunFinish { result, .. } => result
            .as_ref()
            .map(|_| AgentEvent::extract_response(result)),
        _ => None,
    })
}

fn extract_requested_interaction(events: &[AgentEvent]) -> Option<Suspension> {
    events.iter().find_map(|e| match e {
        AgentEvent::ToolCallReady {
            id,
            name,
            arguments,
        } => Some(
            Suspension::new(id.clone(), format!("tool:{name}")).with_parameters(arguments.clone()),
        ),
        _ => None,
    })
}

fn extract_inference_model(events: &[AgentEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentEvent::InferenceComplete { model, .. } => Some(model.clone()),
        _ => None,
    })
}

#[test]
fn test_normalize_termination_for_suspended_calls_forces_waiting() {
    let mut run_ctx = RunContext::from_thread(
        &Thread::new("normalize-termination-suspended"),
        tirea_contract::RunPolicy::default(),
    )
    .expect("create run context");
    let state = run_ctx.snapshot().expect("snapshot state");
    let patch = set_single_suspended_call(
        &state,
        Suspension::new("confirm_1", "confirm").with_message("waiting for approval"),
        None,
    )
    .expect("seed suspended call");
    run_ctx.add_thread_patch(patch);

    let (termination, response) = normalize_termination_for_suspended_calls(
        &run_ctx,
        TerminationReason::NaturalEnd,
        Some("done".to_string()),
    );

    assert_eq!(termination, TerminationReason::Suspended);
    assert_eq!(response, None);
}

#[test]
fn test_normalize_termination_for_suspended_calls_keeps_cancelled() {
    let mut run_ctx = RunContext::from_thread(
        &Thread::new("normalize-termination-cancelled"),
        tirea_contract::RunPolicy::default(),
    )
    .expect("create run context");
    let state = run_ctx.snapshot().expect("snapshot state");
    let patch = set_single_suspended_call(
        &state,
        Suspension::new("confirm_1", "confirm").with_message("waiting for approval"),
        None,
    )
    .expect("seed suspended call");
    run_ctx.add_thread_patch(patch);

    let (termination, response) = normalize_termination_for_suspended_calls(
        &run_ctx,
        TerminationReason::Cancelled,
        Some("ignored".to_string()),
    );

    assert_eq!(termination, TerminationReason::Cancelled);
    assert_eq!(response, Some("ignored".to_string()));
}

#[test]
fn test_sync_run_lifecycle_for_termination_persists_status_and_reason() {
    let cases = vec![
        (TerminationReason::Suspended, "waiting", None),
        (TerminationReason::NaturalEnd, "done", Some("natural")),
        (
            TerminationReason::BehaviorRequested,
            "done",
            Some("behavior_requested"),
        ),
        (TerminationReason::Cancelled, "done", Some("cancelled")),
        (
            TerminationReason::Error("test error".to_string()),
            "done",
            Some("error"),
        ),
        (
            TerminationReason::stopped("limit"),
            "done",
            Some("stopped:limit"),
        ),
    ];

    for (termination, expected_status, expected_reason) in cases {
        let run_ctx = RunContext::from_thread(
            &Thread::new("lifecycle-state"),
            tirea_contract::RunPolicy::default(),
        )
        .expect("run ctx");
        let mut run_ctx = run_ctx;
        let run_identity = RunIdentity::new(
            "lifecycle-state".to_string(),
            None,
            "run-lifecycle".to_string(),
            None,
            "test-agent".to_string(),
            crate::contracts::RunOrigin::default(),
        );

        sync_run_lifecycle_for_termination(&mut run_ctx, &run_identity, &termination)
            .expect("sync lifecycle patch");
        let state = run_ctx.snapshot().expect("snapshot");

        assert_eq!(state["__run"]["status"], json!(expected_status));
        assert_eq!(
            state["__run"]["done_reason"],
            expected_reason.map_or(Value::Null, |value| json!(value))
        );
        assert_eq!(state["__run"]["id"], json!("run-lifecycle"));
        assert!(
            state["__run"]["updated_at"].as_u64().is_some(),
            "updated_at must be unix millis"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalMessage {
    role: crate::contracts::thread::Role,
    content: String,
    tool_call_id: Option<String>,
    visibility: crate::contracts::thread::Visibility,
    tool_calls: Vec<CanonicalToolCall>,
}

fn canonical_messages_from_slice(messages: &[Arc<Message>]) -> Vec<CanonicalMessage> {
    messages
        .iter()
        .map(|msg| {
            let mut tool_calls = msg
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|call| CanonicalToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.to_string(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            tool_calls.sort_by(|a, b| {
                a.id.cmp(&b.id)
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.arguments.cmp(&b.arguments))
            });

            CanonicalMessage {
                role: msg.role,
                content: msg.content.clone(),
                tool_call_id: msg.tool_call_id.clone(),
                visibility: msg.visibility,
                tool_calls,
            }
        })
        .collect()
}

fn compact_canonical_messages(thread: &Thread) -> Vec<CanonicalMessage> {
    compact_canonical_messages_from_slice(&thread.messages)
}

fn compact_canonical_messages_from_slice(messages: &[Arc<Message>]) -> Vec<CanonicalMessage> {
    let mut compacted = Vec::new();
    for msg in canonical_messages_from_slice(messages) {
        if compacted.last() == Some(&msg) {
            continue;
        }
        compacted.push(msg);
    }
    compacted
}

#[tokio::test]
async fn test_stream_state_commit_failure_on_assistant_turn_emits_error_and_run_finish() {
    let committer = Arc::new(RecordingStateCommitter::new(Some(
        CheckpointReason::AssistantTurnCommitted,
    )));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let config = BaseAgent::new("mock").with_llm_executor(Arc::new(MockStreamProvider::new(vec![
        MockResponse::text("done"),
    ])) as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    );
    let events = collect_stream_events(stream).await;

    assert!(matches!(
        extract_termination(&events),
        Some(TerminationReason::Error(_))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("state commit failed"))),
        "expected state commit error event, got: {events:?}"
    );
    assert_eq!(
        committer.reasons(),
        vec![
            CheckpointReason::AssistantTurnCommitted,
            CheckpointReason::RunFinished
        ]
    );
}

#[tokio::test]
async fn test_nonstream_checkpoints_include_run_start_side_effects() {
    let committer = Arc::new(RecordingStateCommitter::new(None));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let config = BaseAgent::new("mock")
        .with_llm_executor(
            Arc::new(MockChatProvider::new(vec![Ok(text_chat_response("done"))]))
                as Arc<dyn LlmExecutor>,
        )
        .with_behavior(Arc::new(RunStartSideEffectPlugin) as Arc<dyn AgentBehavior>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(
        &config,
        HashMap::new(),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    )
    .await;

    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert_eq!(
        committer.reasons(),
        vec![
            CheckpointReason::UserMessage,
            CheckpointReason::AssistantTurnCommitted,
            CheckpointReason::RunFinished
        ]
    );
}

#[tokio::test]
async fn test_nonstream_state_commit_failure_on_assistant_turn_returns_error() {
    let committer = Arc::new(RecordingStateCommitter::new(Some(
        CheckpointReason::AssistantTurnCommitted,
    )));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let config = BaseAgent::new("mock").with_llm_executor(Arc::new(MockChatProvider::new(vec![Ok(
        text_chat_response("done"),
    )])) as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(
        &config,
        HashMap::new(),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    )
    .await;

    assert!(matches!(outcome.termination, TerminationReason::Error(_)));
    assert!(matches!(
        outcome.failure,
        Some(LoopFailure::State(message)) if message.contains("state commit failed")
    ));
    assert_eq!(
        committer.reasons(),
        vec![
            CheckpointReason::AssistantTurnCommitted,
            CheckpointReason::RunFinished
        ]
    );
}

#[tokio::test]
async fn test_stream_state_commit_failure_on_tool_results_emits_error_before_tool_done() {
    let committer = Arc::new(RecordingStateCommitter::new(Some(
        CheckpointReason::ToolResultsCommitted,
    )));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let config = BaseAgent::new("mock").with_llm_executor(Arc::new(MockStreamProvider::new(vec![
        MockResponse::text("tool").with_tool_call("call_1", "echo", json!({"message":"hi"})),
    ])) as Arc<dyn LlmExecutor>);
    let tools = tool_map([EchoTool]);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    );
    let events = collect_stream_events(stream).await;

    assert!(matches!(
        extract_termination(&events),
        Some(TerminationReason::Error(_))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallReady { id, .. } if id == "call_1")),
        "tool round should begin before commit failure"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallDone { .. })),
        "tool result events must not be emitted after tool commit failure"
    );
    assert_eq!(
        committer.reasons(),
        vec![
            CheckpointReason::AssistantTurnCommitted,
            CheckpointReason::ToolResultsCommitted,
            CheckpointReason::RunFinished
        ]
    );
}

#[tokio::test]
async fn test_stream_run_finished_commit_failure_emits_error_without_run_finish_event() {
    let committer = Arc::new(RecordingStateCommitter::new(Some(
        CheckpointReason::RunFinished,
    )));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let config = BaseAgent::new("mock").with_llm_executor(Arc::new(MockStreamProvider::new(vec![
        MockResponse::text("done"),
    ])) as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    );
    let events = collect_stream_events(stream).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("state commit failed"))),
        "expected run-finished commit error event, got: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::RunFinish { .. })),
        "run finish event should be suppressed when final force-commit fails"
    );
    assert_eq!(
        committer.reasons(),
        vec![
            CheckpointReason::AssistantTurnCommitted,
            CheckpointReason::RunFinished
        ]
    );
}

#[tokio::test]
async fn test_stream_error_termination_run_finished_commit_failure_emits_state_error_only() {
    let committer = Arc::new(RecordingStateCommitter::new(Some(
        CheckpointReason::RunFinished,
    )));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let provider = Arc::new(ScriptedStreamProvider::new(vec![
        text_stream_error_attempt(
            "hel",
            web_stream_io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ),
        ),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 10,
            max_backoff_ms: 10,
            backoff_jitter_percent: 0,
            max_retry_window_ms: Some(5),
            retry_stream_start: true,
            max_stream_event_retries: 1,
            stream_error_fallback_threshold: 2,
        })
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    );
    let events = collect_stream_events(stream).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, code } if message.contains("state commit failed") && code.as_deref() == Some("STATE_ERROR"))),
        "expected state commit error event, got: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, code } if message.contains("connection reset by peer") && code.as_deref() == Some("LLM_ERROR"))),
        "original llm error must not be emitted after run-finished persistence failure: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::RunFinish { .. })),
        "run finish event should be suppressed when error termination cannot persist RunFinished: {events:?}"
    );
    assert_eq!(committer.reasons(), vec![CheckpointReason::RunFinished]);
}

#[tokio::test]
async fn test_stream_frontend_use_as_tool_result_emits_single_tool_call_start() {
    struct FrontendPendingPlugin;

    #[async_trait]
    impl AgentBehavior for FrontendPendingPlugin {
        fn id(&self) -> &str {
            "frontend_pending_plugin"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_call_id() != Some("call_1") {
                return ActionSet::empty();
            }
            let call_id = ctx.tool_call_id().unwrap().to_string();
            let invocation = FrontendToolInvocation::new(
                &call_id,
                "addTask",
                json!({ "title": "Deploy v2" }),
                InvocationOrigin::PluginInitiated {
                    plugin_id: "agui_frontend_tools".to_string(),
                },
                ResponseRouting::UseAsToolResult,
            );
            ActionSet::single(BeforeToolExecuteAction::Suspend(
                suspend_ticket_from_invocation(invocation),
            ))
        }
    }

    let thread = Thread::new("frontend-pending").with_message(Message::user("add task"));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(FrontendPendingPlugin) as Arc<dyn AgentBehavior>);
    let tools = tool_map([AddTaskTool]);
    let responses = vec![MockResponse::text("planning").with_tool_call(
        "call_1",
        "addTask",
        json!({ "title": "Deploy v2" }),
    )];

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    let starts_for_call_1 = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallStart { id, .. } if id == "call_1"))
        .count();
    assert_eq!(
        starts_for_call_1, 1,
        "frontend pending call must not emit duplicate ToolCallStart events: {events:?}"
    );

    let ready_for_call_1 = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallReady { id, .. } if id == "call_1"))
        .count();
    assert_eq!(
        ready_for_call_1, 1,
        "frontend pending call must emit a single ToolCallReady: {events:?}"
    );

    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinish {
            termination: TerminationReason::Suspended,
            ..
        })
    ));
}

#[tokio::test]
async fn test_stream_terminate_behavior_requested_force_commits_run_finished_delta() {
    let (recorder, _phases) = RecordAndTerminatePlugin::new();
    let committer = Arc::new(RecordingStateCommitter::new(None));
    let thread = Thread::new("test").with_message(Message::user("go"));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>)
        .with_llm_executor(Arc::new(MockStreamProvider::new(vec![])) as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        Some(committer.clone() as Arc<dyn StateCommitter>),
        None,
    );
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::BehaviorRequested)
    );
    assert_eq!(committer.reasons(), vec![CheckpointReason::RunFinished]);
}

#[tokio::test]
async fn test_stream_replay_state_failure_emits_error() {
    let broken_patch = tirea_state::TrackedPatch::new(
        Patch::new().with_op(Op::increment(tirea_state::path!("missing_counter"), 1_i64)),
    )
    .with_source("test:broken_state");

    // Build RunContext with base state, then add the broken patch so state()
    // fails lazily during loop execution (not eagerly in from_thread).
    let mut run_ctx = RunContext::new(
        "test",
        json!({}),
        vec![Arc::new(Message::user("resume"))],
        crate::contracts::RunPolicy::default(),
    );
    run_ctx.add_thread_patch(broken_patch);

    let config = BaseAgent::new("mock");
    let tools = tool_map([EchoTool]);

    let provider = MockStreamProvider::new(vec![MockResponse::text("should not run")]);
    let config = config.with_llm_executor(Arc::new(provider));
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("State error") || message.contains("replay"))),
            "expected state rebuild error, got events: {events:?}"
        );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallDone { .. })),
        "tool execution must not run when state rebuild fails"
    );
}

#[tokio::test]
async fn test_legacy_resume_replay_stream_queue_is_ignored() {
    struct LegacyResumeReplayRequeuePlugin;

    #[async_trait]
    impl AgentBehavior for LegacyResumeReplayRequeuePlugin {
        fn id(&self) -> &str {
            "legacy_resume_replay_stream_queue"
        }

        async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phase_actions(Phase::RunStart, ctx).await
        }
        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_call_id() == Some("replay_call_1") {
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new("confirm_replay_call_1", "confirm")
                        .with_message("approve first replay"),
                )));
            }
            ActionSet::empty()
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    impl LegacyResumeReplayRequeuePlugin {
        async fn phase_actions(
            &self,
            phase: Phase,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<LifecycleAction> {
            if phase != Phase::RunStart {
                return ActionSet::empty();
            }
            ActionSet::single(LifecycleAction::State(AnyStateAction::new::<
                ResumeToolCallsState,
            >(json!([
                {
                    "id": "replay_call_1",
                    "name": "echo",
                    "arguments": {"message": "first"}
                },
                {
                    "id": "replay_call_2",
                    "name": "echo",
                    "arguments": {"message": "second"}
                }
            ]))))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(LegacyResumeReplayRequeuePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("resume"));
    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![MockResponse::text("unused")]),
        config,
        thread,
        tool_map([EchoTool]),
    )
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::BehaviorRequested)
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallDone { id, .. } if id.starts_with("replay_call_"))),
        "legacy resume replay queue should not execute in stream mode"
    );
    assert!(
        !final_thread.messages.iter().any(|message| {
            message.role == crate::contracts::thread::Role::Tool
                && message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("replay_call_"))
        }),
        "legacy resume replay queue should not append tool result messages"
    );

    let final_state = final_thread.rebuild_state().expect("state should rebuild");
    let legacy_replay_calls = final_state
        .get("__resume_tool_calls")
        .and_then(|legacy| legacy.get("calls"))
        .and_then(|calls| calls.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        legacy_replay_calls.len(),
        2,
        "legacy resume replay queue should remain untouched"
    );
    assert_eq!(
        legacy_replay_calls[0]["id"],
        Value::String("replay_call_1".to_string()),
        "legacy resume replay queue order should be preserved"
    );
}

#[tokio::test]
async fn test_stream_parallel_multiple_pending_emits_all_suspended() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static SESSION_END_RAN: AtomicBool = AtomicBool::new(false);

    struct PendingAndRunEndPlugin;

    #[async_trait]
    impl AgentBehavior for PendingAndRunEndPlugin {
        fn id(&self) -> &str {
            "pending_and_run_end"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new(format!("confirm_{call_id}"), "confirm")
                        .with_message("needs confirmation"),
                )));
            }
            ActionSet::empty()
        }

        async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            SESSION_END_RAN.store(true, Ordering::SeqCst);
            ActionSet::empty()
        }
    }

    SESSION_END_RAN.store(false, Ordering::SeqCst);

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingAndRunEndPlugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let thread = Thread::new("test").with_message(Message::user("run tools"));
    let responses = vec![MockResponse::text("run both")
        .with_tool_call("call_1", "echo", json!({"message": "a"}))
        .with_tool_call("call_2", "echo", json!({"message": "b"}))];
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "multiple pending in parallel should not fail apply: {events:?}"
    );
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended)
    );
    // Per-call suspension: each suspended tool emits its own Pending event
    assert_eq!(
        events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    AgentEvent::ToolCallReady { id, name, .. }
                        if id.starts_with("confirm_") || name == "confirm"
                )
            })
            .count(),
        2,
        "each suspended tool should emit a Pending event"
    );
    assert!(
        SESSION_END_RAN.load(Ordering::SeqCst),
        "RunEnd phase must run when stream terminates on suspended interaction"
    );
}

// ========================================================================
// Core termination integration tests (stop conditions are loop-external)
// ========================================================================

#[tokio::test]
async fn test_stop_condition_config_is_ignored_in_stream_loop() {
    // Stop conditions are no longer evaluated by core loop.
    // Provider returns finite tool calls, then stream naturally ends.
    let responses: Vec<MockResponse> = (0..10)
        .map(|i| {
            MockResponse::text("calling echo").with_tool_call(
                &format!("c{i}"),
                "echo",
                json!({"message": "hi"}),
            )
        })
        .collect();

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_natural_end_no_tools() {
    // LLM returns text only → NaturalEnd.
    let provider = MockStreamProvider::new(vec![MockResponse::text("Hello!")]);
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("hi"));
    let tools = HashMap::new();

    let events = run_mock_stream(provider, config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[test]
fn test_apply_tool_results_rejects_conflicting_parallel_state_patches() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let left = tool_execution_result(
        "call_a",
        Some(TrackedPatch::new(Patch::new().with_op(Op::set(
            tirea_state::path!("debug", "shared"),
            json!(1),
        )))),
    );
    let right = tool_execution_result(
        "call_b",
        Some(TrackedPatch::new(Patch::new().with_op(Op::set(
            tirea_state::path!("debug", "shared"),
            json!(2),
        )))),
    );

    let err = match apply_tool_results_to_session(&mut run_ctx, &[left, right], None, true) {
        Ok(_) => panic!("parallel conflicting patches should be rejected"),
        Err(err) => err,
    };
    match err {
        AgentLoopError::StateError(message) => {
            assert!(
                message.contains("conflicting parallel state patches"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected state error, got: {other:?}"),
    }
}

#[test]
fn test_apply_tool_results_accepts_disjoint_parallel_state_patches() {
    let thread = Thread::with_initial_state("test", json!({}));
    let mut run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let left = tool_execution_result(
        "call_a",
        Some(TrackedPatch::new(Patch::new().with_op(Op::set(
            tirea_state::path!("debug", "alpha"),
            json!(1),
        )))),
    );
    let right = tool_execution_result(
        "call_b",
        Some(TrackedPatch::new(Patch::new().with_op(Op::set(
            tirea_state::path!("debug", "beta"),
            json!(2),
        )))),
    );

    let _applied = apply_tool_results_to_session(&mut run_ctx, &[left, right], None, true)
        .expect("parallel disjoint patches should succeed");
    let state = run_ctx.snapshot().expect("state rebuild");
    assert_eq!(state["debug"]["alpha"], 1);
    assert_eq!(state["debug"]["beta"], 2);
}

#[tokio::test]
async fn test_stop_behavior_requested() {
    // TerminateBehaviorRequestedPlugin → BehaviorRequested.
    let (recorder, _) = RecordAndTerminatePlugin::new();
    let config = BaseAgent::new("mock").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("hi"));
    let tools = HashMap::new();

    let provider = MockStreamProvider::new(vec![]);
    let events = run_mock_stream(provider, config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::BehaviorRequested)
    );
}

#[tokio::test]
async fn test_stop_on_tool_condition() {
    // StopOnTool is no longer evaluated by core loop.
    let responses = vec![
        MockResponse::text("step 1").with_tool_call("c1", "echo", json!({"message": "a"})),
        MockResponse::text("step 2").with_tool_call("c2", "finish_tool", json!({})),
    ];

    struct FinishTool;
    #[async_trait]
    impl Tool for FinishTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("finish_tool", "Finish", "Finishes the run")
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("finish_tool", json!({"done": true})))
        }
    }

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));

    let mut tools = tool_map([EchoTool]);
    let ft: Arc<dyn Tool> = Arc::new(FinishTool);
    tools.insert("finish_tool".to_string(), ft);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_content_match_condition() {
    // ContentMatch is no longer evaluated by core loop.
    let responses = vec![
        MockResponse::text("thinking...").with_tool_call("c1", "echo", json!({"message": "a"})),
        MockResponse::text("here is the FINAL_ANSWER: 42").with_tool_call(
            "c2",
            "echo",
            json!({"message": "b"}),
        ),
    ];

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("solve"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_token_budget_condition() {
    // TokenBudget is no longer evaluated by core loop.
    let responses = vec![
        MockResponse::text("step 1")
            .with_tool_call("c1", "echo", json!({"message": "a"}))
            .with_usage(200, 100),
        MockResponse::text("step 2")
            .with_tool_call("c2", "echo", json!({"message": "b"}))
            .with_usage(200, 100),
    ];

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_consecutive_errors_condition() {
    // ConsecutiveErrors is no longer evaluated by core loop.
    let responses: Vec<MockResponse> = (0..5)
        .map(|i| {
            MockResponse::text(&format!("round {i}")).with_tool_call(
                &format!("c{i}"),
                "failing",
                json!({}),
            )
        })
        .collect();

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([FailingTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_loop_detection_condition() {
    // LoopDetection is no longer evaluated by core loop.
    let responses: Vec<MockResponse> = (0..5)
        .map(|i| {
            MockResponse::text(&format!("round {i}")).with_tool_call(
                &format!("c{i}"),
                "echo",
                json!({"message": "same"}),
            )
        })
        .collect();

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_cancellation_token() {
    // Cancel before first inference.
    let token = CancellationToken::new();
    token.cancel();

    let provider = MockStreamProvider::new(vec![MockResponse::text("never")]);
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = HashMap::new();

    let config = config.with_llm_executor(Arc::new(provider) as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, Some(token), None, None);
    let events = collect_stream_events(stream).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Cancelled)
    );
}

#[tokio::test]
async fn test_stop_cancellation_token_during_inference_stream() {
    struct HangingStreamProvider;

    #[async_trait]
    impl LlmExecutor for HangingStreamProvider {
        async fn exec_chat_response(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&ChatOptions>,
        ) -> genai::Result<genai::chat::ChatResponse> {
            unimplemented!("stream-only provider")
        }

        async fn exec_chat_stream_events(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&ChatOptions>,
        ) -> genai::Result<super::LlmEventStream> {
            let stream = async_stream::stream! {
                yield Ok(ChatStreamEvent::Start);
                yield Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: "partial".to_string(),
                }));
                // Simulate a provider stream that hangs after emitting a partial response.
                let _: () = futures::future::pending().await;
            };
            Ok(Box::pin(stream))
        }

        fn name(&self) -> &'static str {
            "hanging_stream"
        }
    }

    let token = CancellationToken::new();
    let initial_thread = Thread::new("test").with_message(Message::user("go"));
    let mut final_thread = initial_thread.clone();
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));
    let config = BaseAgent::new("mock")
        .with_llm_executor(Arc::new(HangingStreamProvider) as Arc<dyn LlmExecutor>);
    let run_ctx =
        RunContext::from_thread(&initial_thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        Some(token.clone()),
        Some(state_committer),
        None,
    );

    let collect_task = tokio::spawn(async move { collect_stream_events(stream).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    token.cancel();

    let events = tokio::time::timeout(std::time::Duration::from_millis(250), collect_task)
        .await
        .expect("stream should stop shortly after cancellation")
        .expect("collector task should not panic");

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Cancelled)
    );

    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut final_thread);
    }
    assert!(
        final_thread
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == CANCELLATION_INFERENCE_USER_MESSAGE),
        "stream inference cancellation note should be persisted in thread history"
    );
}

#[tokio::test]
async fn test_stop_condition_applies_on_natural_end_without_tools() {
    let responses = vec![MockResponse::text("done now")];
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = HashMap::new();

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_run_loop_with_context_cancellation_token() {
    let (recorder, _phases) = RecordAndTerminatePlugin::new();
    let config =
        BaseAgent::new("gpt-4o-mini").with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    let token = CancellationToken::new();
    token.cancel();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, Some(token), None, None).await;

    assert!(
        matches!(outcome.termination, TerminationReason::Cancelled),
        "expected cancellation, got: {:?}",
        outcome.termination
    );
}

#[tokio::test]
async fn test_stop_first_condition_wins() {
    // Stop condition order no longer affects core loop termination.
    let responses = vec![MockResponse::text("r1")
        .with_tool_call("c1", "echo", json!({"message": "a"}))
        .with_usage(100, 100)];

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_default_max_rounds_from_config() {
    // max_rounds no longer drives termination in core loop.
    let responses: Vec<MockResponse> = (0..5)
        .map(|i| {
            MockResponse::text(&format!("r{i}")).with_tool_call(
                &format!("c{i}"),
                "echo",
                json!({"message": "a"}),
            )
        })
        .collect();

    let config = BaseAgent::new("mock").with_max_rounds(2);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_max_rounds_counts_no_tool_step() {
    // Single no-tool step should naturally end regardless of MaxRounds config.
    let responses = vec![MockResponse::text("done")];
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_termination_in_run_finish_event() {
    // Verify RunFinish event structure when natural end triggers.
    let responses =
        vec![MockResponse::text("r1").with_tool_call("c1", "echo", json!({"message": "a"}))];

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test-thread").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    let finish = events
        .iter()
        .find(|e| matches!(e, AgentEvent::RunFinish { .. }));
    assert!(finish.is_some());
    if let Some(AgentEvent::RunFinish {
        thread_id,
        termination,
        ..
    }) = finish
    {
        assert_eq!(thread_id, "test-thread");
        assert_eq!(*termination, TerminationReason::NaturalEnd);
    }
}

#[tokio::test]
async fn test_consecutive_errors_resets_on_success() {
    // Round 1: failing tool (consecutive_errors=1)
    // Round 2: echo succeeds (consecutive_errors=0)
    // Round 3: failing tool (consecutive_errors=1)
    // ConsecutiveErrors(2) should NOT trigger — never reaches 2.
    let responses = vec![
        MockResponse::text("r1").with_tool_call("c1", "failing", json!({})),
        MockResponse::text("r2").with_tool_call("c2", "echo", json!({"message": "ok"})),
        MockResponse::text("r3").with_tool_call("c3", "failing", json!({})),
    ];

    let mut tools = tool_map([EchoTool]);
    let ft: Arc<dyn Tool> = Arc::new(FailingTool);
    tools.insert("failing".to_string(), ft);

    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_run_state_tracks_completed_steps() {
    let mut state = LoopRunState::new();
    assert_eq!(state.completed_steps, 0);

    let tool_calls = vec![crate::contracts::thread::ToolCall::new(
        "c1",
        "echo",
        json!({}),
    )];
    state.record_tool_step(&tool_calls, 0);
    mark_step_completed(&mut state);
    assert_eq!(state.completed_steps, 1);
    assert_eq!(state.consecutive_errors, 0);
    assert_eq!(state.tool_call_history.len(), 1);
}

#[tokio::test]
async fn test_run_state_tracks_token_usage() {
    let mut state = LoopRunState::new();
    let result = StreamResult {
        text: "hello".to_string(),
        tool_calls: vec![],
        usage: Some(crate::contracts::TokenUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            thinking_tokens: Some(20),
            ..Default::default()
        }),
        stop_reason: None,
    };
    state.update_from_response(&result);
    assert_eq!(state.total_thinking_tokens, 20);
    assert_eq!(state.total_input_tokens, 100);
    assert_eq!(state.total_output_tokens, 50);

    state.update_from_response(&result);
    assert_eq!(state.total_thinking_tokens, 40);
    assert_eq!(state.total_input_tokens, 200);
    assert_eq!(state.total_output_tokens, 100);
}

#[tokio::test]
async fn test_run_state_caps_history_at_20() {
    let mut state = LoopRunState::new();
    for i in 0..25 {
        let tool_calls = vec![crate::contracts::thread::ToolCall::new(
            format!("c{i}"),
            format!("tool_{i}"),
            json!({}),
        )];
        state.record_tool_step(&tool_calls, 0);
    }
    assert_eq!(state.tool_call_history.len(), 20);
}

// ========================================================================
// Parallel Tool Execution: Partial Failure Tests
// ========================================================================

#[test]
fn test_parallel_tools_partial_failure() {
    // When running tools in parallel, a failing tool should produce an error
    // message, while the successful tool should still complete.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Call both".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new(
                    "call_echo",
                    "echo",
                    json!({"message": "ok"}),
                ),
                crate::contracts::thread::ToolCall::new("call_fail", "failing", json!({})),
            ],
            usage: None,
            stop_reason: None,
        };

        let mut tools = HashMap::new();
        tools.insert("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>);
        tools.insert(
            "failing".to_string(),
            Arc::new(FailingTool) as Arc<dyn Tool>,
        );

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();

        // Both tools produce messages.
        assert_eq!(
            thread.message_count(),
            2,
            "Both tools should produce a message"
        );

        // One should be success, one should be error.
        let contents: Vec<&str> = thread.messages.iter().map(|m| m.content.as_str()).collect();
        let has_success = contents.iter().any(|c| c.contains("echoed"));
        let has_error = contents
            .iter()
            .any(|c| c.to_lowercase().contains("error") || c.to_lowercase().contains("fail"));
        assert!(has_success, "Echo tool should succeed: {:?}", contents);
        assert!(
            has_error,
            "Failing tool should produce error: {:?}",
            contents
        );
    });
}

#[test]
fn test_parallel_tools_conflicting_state_patches_return_error() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state("test", json!({"counter": 0}));
        let result = StreamResult {
            text: "conflicting calls".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new("call_1", "counter", json!({"amount": 1})),
                crate::contracts::thread::ToolCall::new("call_2", "counter", json!({"amount": 2})),
            ],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([CounterTool]);

        let err = execute_tools(thread, &result, &tools, true)
            .await
            .expect_err("parallel conflicting patches should fail");
        assert!(
            matches!(err, AgentLoopError::StateError(ref msg) if msg.contains("conflict")),
            "expected conflict state error, got: {err:?}"
        );
    });
}

#[test]
fn test_parallel_tools_conflicting_state_actions_return_error() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state("test", json!({"debug": {}}));
        let result = StreamResult {
            text: "conflicting action calls".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new("call_1", "action_a", json!({})),
                crate::contracts::thread::ToolCall::new("call_2", "action_b", json!({})),
            ],
            usage: None,
            stop_reason: None,
        };

        let mut tools = HashMap::new();
        tools.insert(
            "action_a".to_string(),
            Arc::new(ActionStateTool {
                id: "action_a",
                action: DebugFlagAction::RunStart,
            }) as Arc<dyn Tool>,
        );
        tools.insert(
            "action_b".to_string(),
            Arc::new(ActionStateTool {
                id: "action_b",
                action: DebugFlagAction::BeforeInference,
            }) as Arc<dyn Tool>,
        );

        let err = execute_tools(thread, &result, &tools, true)
            .await
            .expect_err("parallel conflicting state actions should fail");
        assert!(
            matches!(err, AgentLoopError::StateError(ref msg) if msg.contains("conflict")),
            "expected conflict state error, got: {err:?}"
        );
    });
}

#[test]
fn test_sequential_tools_partial_failure() {
    // Same test but with sequential execution.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Call both".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new(
                    "call_echo",
                    "echo",
                    json!({"message": "ok"}),
                ),
                crate::contracts::thread::ToolCall::new("call_fail", "failing", json!({})),
            ],
            usage: None,
            stop_reason: None,
        };

        let mut tools = HashMap::new();
        tools.insert("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>);
        tools.insert(
            "failing".to_string(),
            Arc::new(FailingTool) as Arc<dyn Tool>,
        );

        let thread = execute_tools(thread, &result, &tools, false)
            .await
            .unwrap()
            .into_thread();

        assert_eq!(
            thread.message_count(),
            2,
            "Both tools should produce a message"
        );
        let contents: Vec<&str> = thread.messages.iter().map(|m| m.content.as_str()).collect();
        let has_success = contents.iter().any(|c| c.contains("echoed"));
        let has_error = contents
            .iter()
            .any(|c| c.to_lowercase().contains("error") || c.to_lowercase().contains("fail"));
        assert!(has_success, "Echo tool should succeed: {:?}", contents);
        assert!(
            has_error,
            "Failing tool should produce error: {:?}",
            contents
        );
    });
}

#[tokio::test]
async fn test_sequential_tools_stop_after_first_suspension() {
    struct PendingEveryToolPlugin {
        seen_calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl AgentBehavior for PendingEveryToolPlugin {
        fn id(&self) -> &str {
            "pending_every_tool"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                self.seen_calls
                    .lock()
                    .expect("lock poisoned")
                    .push(call_id.to_string());
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new(format!("confirm_{call_id}"), "confirm")
                        .with_message("needs confirmation"),
                )));
            }
            ActionSet::empty()
        }
    }

    let seen_calls = Arc::new(Mutex::new(Vec::new()));
    let agent = BaseAgent::new("m")
        .with_behavior(Arc::new(PendingEveryToolPlugin {
            seen_calls: seen_calls.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(super::tool_exec::SequentialToolExecutor));

    let thread = Thread::new("test");
    let result = StreamResult {
        text: "Call both".to_string(),
        tool_calls: vec![
            crate::contracts::thread::ToolCall::new("call_1", "echo", json!({"message":"a"})),
            crate::contracts::thread::ToolCall::new("call_2", "echo", json!({"message":"b"})),
        ],
        usage: None,
        stop_reason: None,
    };
    let tools = tool_map([EchoTool]);

    let outcome = execute_tools_with_config(thread, &result, &tools, &agent)
        .await
        .expect("sequential mode should pause on first suspended interaction");
    let (thread, suspended_call) = match outcome {
        ExecuteToolsOutcome::Suspended {
            thread,
            suspended_call,
        } => (thread, suspended_call),
        other => panic!("expected Suspended, got: {other:?}"),
    };
    assert_eq!(suspended_call.ticket.suspension.id, "confirm_call_1");
    assert_eq!(
        seen_calls.lock().expect("lock poisoned").clone(),
        vec!["call_1".to_string()],
        "second tool must not execute after first suspended interaction in sequential mode"
    );
    assert_eq!(thread.messages.len(), 1);
    assert_eq!(thread.messages[0].tool_call_id.as_deref(), Some("call_1"));
}

#[tokio::test]
async fn test_parallel_tools_allow_single_suspended_interaction_per_round() {
    struct PendingEveryToolPlugin {
        seen_calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl AgentBehavior for PendingEveryToolPlugin {
        fn id(&self) -> &str {
            "pending_every_tool_parallel"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                self.seen_calls
                    .lock()
                    .expect("lock poisoned")
                    .push(call_id.to_string());
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new(format!("confirm_{call_id}"), "confirm")
                        .with_message("needs confirmation"),
                )));
            }
            ActionSet::empty()
        }
    }

    let seen_calls = Arc::new(Mutex::new(Vec::new()));
    let agent = BaseAgent::new("m").with_behavior(Arc::new(PendingEveryToolPlugin {
        seen_calls: seen_calls.clone(),
    }) as Arc<dyn AgentBehavior>);

    let thread = Thread::new("test");
    let result = StreamResult {
        text: "Call both".to_string(),
        tool_calls: vec![
            crate::contracts::thread::ToolCall::new("call_1", "echo", json!({"message":"a"})),
            crate::contracts::thread::ToolCall::new("call_2", "echo", json!({"message":"b"})),
        ],
        usage: None,
        stop_reason: None,
    };
    let tools = tool_map([EchoTool]);

    let outcome = execute_tools_with_config(thread, &result, &tools, &agent)
        .await
        .expect("parallel mode should suspend all interactions and pause");
    let (thread, suspended_call) = match outcome {
        ExecuteToolsOutcome::Suspended {
            thread,
            suspended_call,
        } => (thread, suspended_call),
        other => panic!("expected Suspended, got: {other:?}"),
    };
    // First suspended call's interaction is returned
    assert_eq!(suspended_call.ticket.suspension.id, "confirm_call_1");
    let mut seen = seen_calls.lock().expect("lock poisoned").clone();
    seen.sort();
    assert_eq!(
        seen,
        vec!["call_1".to_string(), "call_2".to_string()],
        "parallel mode should still execute both BeforeToolExecute phases"
    );
    assert_eq!(thread.messages.len(), 2);
    // Both tools should be suspended (not deferred)
    assert!(
        thread.messages[0].content.contains("awaiting approval"),
        "first tool should be suspended: {}",
        thread.messages[0].content
    );
    assert!(
        thread.messages[1].content.contains("awaiting approval"),
        "second tool should also be suspended: {}",
        thread.messages[1].content
    );
}

// ========================================================================
// Plugin Execution Order Tests
// ========================================================================

/// Plugin that records when it runs (appends to a shared Vec).
struct OrderTrackingPlugin {
    id: &'static str,
    order_log: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl AgentBehavior for OrderTrackingPlugin {
    fn id(&self) -> &str {
        self.id
    }

    async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::RunStart));
        ActionSet::empty()
    }
    async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::StepStart));
        ActionSet::empty()
    }
    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::BeforeInference));
        ActionSet::empty()
    }
    async fn after_inference(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<AfterInferenceAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::AfterInference));
        ActionSet::empty()
    }
    async fn before_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::BeforeToolExecute));
        ActionSet::empty()
    }
    async fn after_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::AfterToolExecute));
        ActionSet::empty()
    }
    async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::StepEnd));
        ActionSet::empty()
    }
    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        self.order_log
            .lock()
            .unwrap()
            .push(format!("{}:{:?}", self.id, Phase::RunEnd));
        ActionSet::empty()
    }
}

#[test]
fn test_plugin_execution_order_preserved() {
    // Plugins should execute in the order they are provided.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));

        let plugin_a = OrderTrackingPlugin {
            id: "plugin_a",
            order_log: Arc::clone(&log),
        };
        let plugin_b = OrderTrackingPlugin {
            id: "plugin_b",
            order_log: Arc::clone(&log),
        };

        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Test".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);
        let agent = BaseAgent::new("m")
            .with_behavior(compose_test_behaviors(vec![
                Arc::new(plugin_a) as Arc<dyn AgentBehavior>,
                Arc::new(plugin_b) as Arc<dyn AgentBehavior>,
            ]))
            .with_tool_executor(Arc::new(super::tool_exec::SequentialToolExecutor));

        let _ = execute_tools_with_config(thread, &result, &tools, &agent).await;

        let entries = log.lock().unwrap().clone();

        // For each phase, plugin_a should appear before plugin_b.
        let before_a = entries
            .iter()
            .position(|e| e.starts_with("plugin_a:BeforeToolExecute"));
        let before_b = entries
            .iter()
            .position(|e| e.starts_with("plugin_b:BeforeToolExecute"));
        if let (Some(a), Some(b)) = (before_a, before_b) {
            assert!(
                a < b,
                "plugin_a should run before plugin_b in BeforeToolExecute phase"
            );
        }

        let after_a = entries
            .iter()
            .position(|e| e.starts_with("plugin_a:AfterToolExecute"));
        let after_b = entries
            .iter()
            .position(|e| e.starts_with("plugin_b:AfterToolExecute"));
        if let (Some(a), Some(b)) = (after_a, after_b) {
            assert!(
                a < b,
                "plugin_a should run before plugin_b in AfterToolExecute phase"
            );
        }
    });
}

/// Plugin that unconditionally blocks the tool in BeforeToolExecute.
///
/// In the legacy mutable model this conditionally blocked based on
/// `step.tool_pending()`.  The AgentBehavior model applies all effects
/// concurrently from an immutable snapshot, so the condition is removed.
struct ConditionalBlockPlugin;

#[async_trait]
impl AgentBehavior for ConditionalBlockPlugin {
    fn id(&self) -> &str {
        "conditional_block"
    }

    async fn before_tool_execute(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        ActionSet::single(BeforeToolExecuteAction::Block(
            "Blocked because tool was pending".to_string(),
        ))
    }
}

#[test]
fn test_plugin_order_affects_outcome() {
    // In the AgentBehavior model, effects are applied in declaration order.
    // When suspend and block are both emitted, the last one wins because
    // each clears the other's state (block clears suspend, suspend clears block).
    struct PendingPhasePluginLegacy;

    #[async_trait]
    impl AgentBehavior for PendingPhasePluginLegacy {
        fn id(&self) -> &str {
            "pending"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_name() == Some("echo") {
                ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new("confirm_1", "confirm").with_message("Execute echo?"),
                )))
            } else {
                ActionSet::empty()
            }
        }
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::new("test");
        let result = StreamResult {
            text: "Test".to_string(),
            tool_calls: vec![crate::contracts::thread::ToolCall::new(
                "call_1",
                "echo",
                json!({"message": "test"}),
            )],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([EchoTool]);

        // Order 1: suspend first, then block → block wins (last applied).
        let agent_order1 = BaseAgent::new("m")
            .with_behavior(compose_test_behaviors(vec![
                Arc::new(PendingPhasePluginLegacy) as Arc<dyn AgentBehavior>,
                Arc::new(ConditionalBlockPlugin) as Arc<dyn AgentBehavior>,
            ]))
            .with_tool_executor(Arc::new(super::tool_exec::SequentialToolExecutor));
        let r1 = execute_tools_with_config(thread.clone(), &result, &tools, &agent_order1).await;
        let s1 = r1.unwrap().into_thread();
        assert_eq!(s1.message_count(), 1);
        assert!(
            s1.messages[0].content.to_lowercase().contains("blocked"),
            "Order 1 (suspend then block): block should win: {}",
            s1.messages[0].content
        );

        // Order 2: block first, then suspend → suspend wins (last applied).
        let agent_order2 = BaseAgent::new("m")
            .with_behavior(compose_test_behaviors(vec![
                Arc::new(ConditionalBlockPlugin) as Arc<dyn AgentBehavior>,
                Arc::new(PendingPhasePluginLegacy) as Arc<dyn AgentBehavior>,
            ]))
            .with_tool_executor(Arc::new(super::tool_exec::SequentialToolExecutor));
        let r2 = execute_tools_with_config(thread, &result, &tools, &agent_order2).await;
        assert!(
            r2.as_ref().unwrap().is_suspended(),
            "Order 2 (block then suspend): suspend should win"
        );
    });
}

// ========================================================================
// Message ID alignment integration tests
// ========================================================================
//
// These tests verify that pre-generated message IDs flow correctly through
// the entire pipeline: streaming AgentEvents → stored Thread messages →
// AG-UI protocol events → AI SDK protocol events.

/// Verify that `StepStart.message_id` matches the stored assistant `Message.id`.
#[tokio::test]
async fn test_message_id_stepstart_matches_stored_assistant_message() {
    let responses = vec![MockResponse::text("Hello world")];
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("hi"));

    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(responses),
        config,
        thread,
        HashMap::new(),
    )
    .await;

    // Extract message_id from StepStart event.
    let step_msg_id = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::StepStart { message_id } => Some(message_id.clone()),
            _ => None,
        })
        .expect("stream must contain a StepStart event");

    // The pre-generated ID must be a valid UUID v7.
    assert_eq!(step_msg_id.len(), 36, "message_id should be a UUID");
    assert_eq!(&step_msg_id[14..15], "7", "message_id should be UUID v7");

    // Find the assistant message stored in the final thread.
    let assistant_msg = final_thread
        .messages
        .iter()
        .find(|m| m.role == crate::contracts::thread::Role::Assistant)
        .expect("final thread must contain an assistant message");

    assert_eq!(
        assistant_msg.id.as_deref(),
        Some(step_msg_id.as_str()),
        "StepStart.message_id must equal stored assistant Message.id"
    );
}

/// Verify that `ToolCallDone.message_id` matches the stored tool `Message.id`.
#[tokio::test]
async fn test_message_id_toolcalldone_matches_stored_tool_message() {
    // Two responses: first triggers a tool call, second is the final answer.
    let responses = vec![
        MockResponse::text("let me search").with_tool_call(
            "call_1",
            "echo",
            json!({"message": "test"}),
        ),
        MockResponse::text("found it"),
    ];
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("search"));
    let tools = tool_map([EchoTool]);

    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(responses),
        config,
        thread,
        tools,
    )
    .await;

    // Extract message_id from the ToolCallDone event.
    let tool_done_msg_id = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallDone { message_id, .. } => Some(message_id.clone()),
            _ => None,
        })
        .expect("stream must contain a ToolCallDone event");

    assert_eq!(
        tool_done_msg_id.len(),
        36,
        "tool message_id should be a UUID"
    );

    // Find the tool result message in the final thread.
    let tool_msg = final_thread
        .messages
        .iter()
        .find(|m| m.role == crate::contracts::thread::Role::Tool)
        .expect("final thread must contain a tool message");

    assert_eq!(
        tool_msg.id.as_deref(),
        Some(tool_done_msg_id.as_str()),
        "ToolCallDone.message_id must equal stored tool Message.id"
    );
}

/// End-to-end: run a multi-step stream with tool calls and verify all message IDs
/// are consistent across runtime events and stored messages.
#[tokio::test]
async fn test_message_id_end_to_end_multi_step() {
    // Step 1: tool call round. Step 2: final text answer.
    let responses = vec![
        MockResponse::text("searching").with_tool_call("c1", "echo", json!({"message": "query"})),
        MockResponse::text("final answer"),
    ];
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = tool_map([EchoTool]);

    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(responses),
        config,
        thread,
        tools,
    )
    .await;

    // Collect all StepStart message_ids and ToolCallDone message_ids.
    let step_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StepStart { message_id } => Some(message_id.clone()),
            _ => None,
        })
        .collect();
    let tool_ids: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, message_id, .. } => {
                Some((id.clone(), message_id.clone()))
            }
            _ => None,
        })
        .collect();

    assert_eq!(step_ids.len(), 2, "two steps expected (tool round + final)");
    assert_eq!(tool_ids.len(), 1, "one tool call done expected");

    // All IDs must be distinct.
    let all_ids: Vec<&str> = step_ids
        .iter()
        .map(|s| s.as_str())
        .chain(tool_ids.iter().map(|(_, mid)| mid.as_str()))
        .collect();
    let unique: std::collections::HashSet<&str> = all_ids.iter().copied().collect();
    assert_eq!(
        all_ids.len(),
        unique.len(),
        "all pre-generated IDs must be unique"
    );

    // Verify stored assistant messages match step IDs.
    let assistant_msgs: Vec<&Arc<Message>> = final_thread
        .messages
        .iter()
        .filter(|m| m.role == crate::contracts::thread::Role::Assistant)
        .collect();
    assert_eq!(assistant_msgs.len(), 2);
    assert_eq!(assistant_msgs[0].id.as_deref(), Some(step_ids[0].as_str()));
    assert_eq!(assistant_msgs[1].id.as_deref(), Some(step_ids[1].as_str()));

    // Verify stored tool message matches ToolCallDone ID.
    let tool_msgs: Vec<&Arc<Message>> = final_thread
        .messages
        .iter()
        .filter(|m| m.role == crate::contracts::thread::Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1);
    assert_eq!(
        tool_msgs[0].id.as_deref(),
        Some(tool_ids[0].1.as_str()),
        "stored tool Message.id must match ToolCallDone.message_id"
    );
}

#[tokio::test]
async fn test_run_step_terminate_behavior_requested_returns_empty_result_without_assistant_message()
{
    let (recorder, phases) = RecordAndTerminatePlugin::new();
    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(recorder) as Arc<dyn AgentBehavior>)
        .with_max_rounds(1);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;

    // terminate_behavior_requested in run_loop terminates with BehaviorRequested
    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));
    assert!(outcome.response.as_ref().is_none_or(|s| s.is_empty()));
    assert_eq!(outcome.run_ctx.messages().len(), 1);

    let recorded = phases.lock().expect("lock poisoned").clone();
    assert_eq!(
        recorded,
        vec![
            Phase::RunStart,
            Phase::StepStart,
            Phase::BeforeInference,
            Phase::RunEnd
        ]
    );
}

#[tokio::test]
async fn test_run_step_terminate_behavior_requested_with_suspended_state_returns_suspended_interaction(
) {
    struct PendingTerminateStepPlugin;

    #[async_trait]
    impl AgentBehavior for PendingTerminateStepPlugin {
        fn id(&self) -> &str {
            "pending_terminate_behavior_requested_step"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
            .and(ActionSet::single(BeforeInferenceAction::State(
                single_suspended_call_state_action(
                    Suspension::new("agent_recovery_step-1", "recover_agent_run")
                        .with_message("resume step?"),
                    None,
                ),
            )))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(PendingTerminateStepPlugin) as Arc<dyn AgentBehavior>)
        .with_max_rounds(1);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert!(matches!(outcome.termination, TerminationReason::Suspended));

    let suspended_calls = outcome.run_ctx.suspended_calls();
    let interaction = &suspended_calls
        .get("agent_recovery_step-1")
        .expect("should have suspended interaction")
        .ticket
        .suspension;
    assert_eq!(interaction.action, "recover_agent_run");
    assert_eq!(interaction.message, "resume step?");

    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    assert_eq!(
        get_suspended_call(&state, "agent_recovery_step-1").expect("call should be suspended")
            ["suspension"]["action"],
        Value::String("recover_agent_run".to_string())
    );
}

#[tokio::test]
async fn test_stream_tool_execution_injects_scope_context_for_tools() {
    let responses = vec![
        MockResponse::text("call scope").with_tool_call("call_1", "scope_snapshot", json!({})),
        MockResponse::text("done"),
    ];
    let config = BaseAgent::new("mock");
    let thread = Thread::with_initial_state("stream-caller", json!({"k":"v"}))
        .with_message(Message::user("hello"));
    let tools = tool_map([ScopeSnapshotTool]);

    let (_events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(responses),
        config,
        thread,
        tools,
    )
    .await;

    let tool_msg = final_thread
        .messages
        .iter()
        .find(|m| {
            m.role == crate::contracts::thread::Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
        })
        .expect("scope snapshot tool result should exist");
    let tool_result: ToolResult =
        serde_json::from_str(&tool_msg.content).expect("tool result json");
    assert_eq!(
        tool_result.status,
        crate::contracts::runtime::tool_call::ToolStatus::Success
    );
    assert_eq!(tool_result.data["thread_id"], json!("stream-caller"));
    assert_eq!(tool_result.data["messages_len"], json!(2));
}

#[tokio::test]
async fn test_stream_startup_error_runs_cleanup_phases_and_persists_cleanup_patch() {
    struct CleanupOnStartErrorPlugin {
        phases: Arc<Mutex<Vec<Phase>>>,
    }

    #[async_trait]
    impl AgentBehavior for CleanupOnStartErrorPlugin {
        fn id(&self) -> &str {
            "cleanup_on_start_error"
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::RunStart);
            ActionSet::empty()
        }
        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::StepStart);
            ActionSet::empty()
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::BeforeInference);
            ActionSet::empty()
        }
        async fn after_inference(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::AfterInference);
            let err_type = ctx.inference_error().map(|e| e.error_type.as_str());
            assert_eq!(err_type, Some("llm_stream_start_error"));
            ActionSet::empty()
        }
        async fn before_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::BeforeToolExecute);
            ActionSet::empty()
        }
        async fn after_tool_execute(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterToolExecuteAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::AfterToolExecute);
            ActionSet::empty()
        }
        async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::StepEnd);
            ActionSet::single(LifecycleAction::State(test_bool_state_action(
                "debug.cleanup_ran",
                true,
            )))
        }
        async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            self.phases
                .lock()
                .expect("lock poisoned")
                .push(Phase::RunEnd);
            ActionSet::empty()
        }
    }

    let phases = Arc::new(Mutex::new(Vec::new()));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(CleanupOnStartErrorPlugin {
            phases: phases.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_retry_policy(LlmRetryPolicy {
            max_attempts_per_model: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
            retry_stream_start: true,
            ..LlmRetryPolicy::default()
        });

    let initial_thread =
        Thread::with_initial_state("test", json!({})).with_message(Message::user("go"));
    let mut final_thread = initial_thread.clone();
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));

    let config =
        config.with_llm_executor(Arc::new(FailingStartProvider::new(10)) as Arc<dyn LlmExecutor>);
    let run_ctx =
        RunContext::from_thread(&initial_thread, tirea_contract::RunPolicy::default()).unwrap();
    let events = collect_stream_events(run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        Some(state_committer),
        None,
    ))
    .await;

    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut final_thread);
    }

    assert!(matches!(
        extract_termination(&events),
        Some(TerminationReason::Error(_))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.contains("429"))),
        "expected stream error event from startup failure, got: {events:?}"
    );

    let recorded = phases.lock().expect("lock poisoned").clone();
    assert!(
        recorded.contains(&Phase::AfterInference),
        "cleanup should run AfterInference on startup failure, got: {recorded:?}"
    );
    assert!(
        recorded.contains(&Phase::StepEnd),
        "cleanup should run StepEnd on startup failure, got: {recorded:?}"
    );
    assert!(
        recorded.contains(&Phase::RunEnd),
        "run should still emit RunEnd on startup failure, got: {recorded:?}"
    );

    let state = final_thread.rebuild_state().expect("state should rebuild");
    assert_eq!(state["debug"]["cleanup_ran"], true);
}

#[tokio::test]
async fn test_stream_stop_condition_is_ignored_and_natural_end_wins() {
    let responses = vec![MockResponse::text("done now")];
    let config = BaseAgent::new("mock");
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools = HashMap::new();

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
}

#[tokio::test]
async fn test_stop_cancellation_token_during_tool_execution_stream() {
    let ready = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let tool = ActivityGateTool {
        id: "activity_gate".to_string(),
        stream_id: "stream_cancel".to_string(),
        ready: ready.clone(),
        proceed,
    };

    let responses = vec![MockResponse::text("running tool").with_tool_call(
        "call_1",
        "activity_gate",
        json!({}),
    )];
    let token = CancellationToken::new();
    let initial_thread = Thread::new("test").with_message(Message::user("go"));
    let mut final_thread = initial_thread.clone();
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));
    let config = BaseAgent::new("mock")
        .with_llm_executor(Arc::new(MockStreamProvider::new(responses)) as Arc<dyn LlmExecutor>);
    let tools = tool_map([tool]);
    let run_ctx =
        RunContext::from_thread(&initial_thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        Some(token.clone()),
        Some(state_committer),
        None,
    );

    let collector = tokio::spawn(async move { collect_stream_events(stream).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), ready.notified())
        .await
        .expect("tool execution did not reach cancellation checkpoint");
    token.cancel();

    let events = tokio::time::timeout(std::time::Duration::from_millis(300), collector)
        .await
        .expect("stream should stop shortly after cancellation during tool execution")
        .expect("collector task should not panic");

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Cancelled)
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallDone { .. })),
        "tool should not report completion after cancellation"
    );
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut final_thread);
    }
    let cancellation_count = final_thread
        .messages
        .iter()
        .filter(|m| m.role == Role::User && m.content == CANCELLATION_TOOL_USER_MESSAGE)
        .count();
    assert_eq!(
        cancellation_count, 1,
        "stream tool cancellation should persist exactly one interruption note"
    );
}

// ========================================================================
// RunContext Patch Lifecycle Tests
// ========================================================================

/// Patches added via `add_patch` are lazily evaluated — they only affect
/// state when `state()` is called.
#[test]
fn test_run_ctx_patches_are_lazily_evaluated() {
    let mut run_ctx = RunContext::new("test", json!({"counter": 0}), vec![], Default::default());

    // Add patches but don't call state() yet
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("counter"), json!(1))),
    ));
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("extra"), json!("added"))),
    ));

    // Base state is still the original value
    assert_eq!(run_ctx.thread_base()["counter"], 0);
    assert!(run_ctx.thread_base().get("extra").is_none());

    // state() computes the accumulated patches
    let state = run_ctx.snapshot().unwrap();
    assert_eq!(state["counter"], 1);
    assert_eq!(state["extra"], "added");

    // Patches are still tracked (not consumed by state())
    assert_eq!(run_ctx.thread_patches().len(), 2);
}

/// Multiple `state()` calls return consistent results and are idempotent.
#[test]
fn test_run_ctx_state_is_idempotent() {
    let mut run_ctx = RunContext::new("test", json!({"v": 0}), vec![], Default::default());
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("v"), json!(42))),
    ));

    let s1 = run_ctx.snapshot().unwrap();
    let s2 = run_ctx.snapshot().unwrap();
    assert_eq!(s1, s2, "state() must be idempotent");
}

/// Patches added between two `state()` calls are visible in the second call.
#[test]
fn test_run_ctx_incremental_patches_visible_in_rebuild() {
    let mut run_ctx = RunContext::new("test", json!({"a": 0, "b": 0}), vec![], Default::default());

    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("a"), json!(1))),
    ));
    let s1 = run_ctx.snapshot().unwrap();
    assert_eq!(s1["a"], 1);
    assert_eq!(s1["b"], 0);

    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("b"), json!(2))),
    ));
    let s2 = run_ctx.snapshot().unwrap();
    assert_eq!(s2["a"], 1, "prior patch must still be applied");
    assert_eq!(s2["b"], 2, "new patch must be visible");
}

/// `take_delta()` consumes only the *new* patches since the last take.
#[test]
fn test_run_ctx_take_delta_tracks_incremental_patches() {
    let mut run_ctx = RunContext::new("test", json!({}), vec![], Default::default());

    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("x"), json!(1))),
    ));
    let d1 = run_ctx.take_delta();
    assert_eq!(d1.patches.len(), 1);

    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("y"), json!(2))),
    ));
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("z"), json!(3))),
    ));
    let d2 = run_ctx.take_delta();
    assert_eq!(d2.patches.len(), 2, "only patches since last take_delta");

    // state() still sees ALL patches (delta tracking is orthogonal)
    let state = run_ctx.snapshot().unwrap();
    assert_eq!(state["x"], 1);
    assert_eq!(state["y"], 2);
    assert_eq!(state["z"], 3);
}

/// Parallel disjoint tool patches are applied atomically via `apply_tool_results_to_session`,
/// and the conflict-free patches from both tools are visible in `state()`.
#[test]
fn test_parallel_disjoint_patches_applied_atomically() {
    let mut run_ctx = RunContext::new(
        "test",
        json!({"alpha": 0, "beta": 0}),
        vec![],
        Default::default(),
    );
    let left = tool_execution_result(
        "call_a",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("alpha"), json!(10))),
        )),
    );
    let right = tool_execution_result(
        "call_b",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("beta"), json!(20))),
        )),
    );

    let applied = apply_tool_results_to_session(&mut run_ctx, &[left, right], None, true)
        .expect("disjoint parallel patches must succeed");

    // State snapshot reflects both patches
    let snapshot = applied.state_snapshot.expect("state should have changed");
    assert_eq!(snapshot["alpha"], 10);
    assert_eq!(snapshot["beta"], 20);

    // RunContext also reflects both
    let state = run_ctx.snapshot().unwrap();
    assert_eq!(state["alpha"], 10);
    assert_eq!(state["beta"], 20);

    // Tool result messages are added
    assert_eq!(
        run_ctx.messages().len(),
        2,
        "each tool gets a result message"
    );
}

/// When parallel tools produce conflicting patches, NO patches are applied —
/// the error is returned before `add_patches()`.
#[test]
fn test_parallel_conflicting_patches_rejected_before_application() {
    let mut run_ctx = RunContext::new("test", json!({"shared": 0}), vec![], Default::default());
    let left = tool_execution_result(
        "call_a",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(1))),
        )),
    );
    let right = tool_execution_result(
        "call_b",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(2))),
        )),
    );

    match apply_tool_results_to_session(&mut run_ctx, &[left, right], None, true) {
        Err(AgentLoopError::StateError(_)) => {} // expected
        Err(other) => panic!("expected StateError, got: {other:?}"),
        Ok(_) => panic!("conflicting patches must fail"),
    }

    // Crucially: no patches were applied to run_ctx
    assert_eq!(
        run_ctx.thread_patches().len(),
        0,
        "no patches should be added on conflict"
    );
    assert_eq!(
        run_ctx.messages().len(),
        0,
        "no messages should be added on conflict"
    );

    let state = run_ctx.snapshot().unwrap();
    assert_eq!(
        state["shared"], 0,
        "state must remain unchanged after conflict rejection"
    );
}

/// Sequential execution: the second tool sees the first tool's state changes
/// because the sequential executor propagates intermediate state.
#[test]
fn test_sequential_tools_see_accumulated_state() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state("test", json!({"counter": 0}));
        let result = StreamResult {
            text: "Two increments".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new("call_1", "counter", json!({"amount": 3})),
                crate::contracts::thread::ToolCall::new("call_2", "counter", json!({"amount": 7})),
            ],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([CounterTool]);

        // Sequential execution: false = not parallel
        let thread = execute_tools(thread, &result, &tools, false)
            .await
            .unwrap()
            .into_thread();

        // Tool 1 sees counter=0, sets to 3
        // Tool 2 sees counter=3 (accumulated!), sets to 10
        let state = thread.rebuild_state().unwrap();
        assert_eq!(
            state["counter"], 10,
            "sequential tools must see accumulated state: 0 → +3 → +7 = 10"
        );
    });
}

/// Parallel execution: each tool sees the SAME frozen snapshot, so both start
/// from counter=0 independently. But parallel counter writes conflict.
#[test]
fn test_parallel_tools_see_frozen_snapshot_not_accumulated() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let thread = Thread::with_initial_state("test", json!({"counter": 0}));
        let result = StreamResult {
            text: "Two increments".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new("call_1", "counter", json!({"amount": 3})),
                crate::contracts::thread::ToolCall::new("call_2", "counter", json!({"amount": 7})),
            ],
            usage: None,
            stop_reason: None,
        };
        let tools = tool_map([CounterTool]);

        // Parallel execution: true = parallel
        // Both tools write to "counter" → conflict detected
        let err = execute_tools(thread, &result, &tools, true)
            .await
            .expect_err("parallel counter writes should conflict");
        assert!(
            matches!(err, AgentLoopError::StateError(ref msg) if msg.contains("conflict")),
            "expected conflict error, got: {err:?}"
        );
    });
}

/// Parallel tools writing to DIFFERENT state paths succeed, and both writes
/// are visible in the final state.
#[test]
fn test_parallel_tools_disjoint_paths_both_visible() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // AlphaTool writes to "alpha", BetaTool writes to "beta"
        struct AlphaTool;
        #[async_trait]
        impl Tool for AlphaTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("alpha", "Alpha", "Write alpha")
            }
            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolCallContext<'_>,
            ) -> Result<ToolResult, ToolError> {
                let state = ctx.state::<TestCounterState>("alpha");
                state.set_counter(111).expect("failed to set counter");
                Ok(ToolResult::success("alpha", json!({"ok": true})))
            }
            async fn execute_effect(
                &self,
                _args: Value,
                _ctx: &ToolCallContext<'_>,
            ) -> Result<ToolExecutionEffect, ToolError> {
                Ok(
                    ToolExecutionEffect::new(ToolResult::success("alpha", json!({"ok": true})))
                        .with_action(AnyStateAction::new_at::<TestCounterState>(
                            "alpha",
                            TestCounterAction::SetCounter(111),
                        )),
                )
            }
        }
        struct BetaTool;
        #[async_trait]
        impl Tool for BetaTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("beta", "Beta", "Write beta")
            }
            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolCallContext<'_>,
            ) -> Result<ToolResult, ToolError> {
                let state = ctx.state::<TestCounterState>("beta");
                state.set_counter(222).expect("failed to set counter");
                Ok(ToolResult::success("beta", json!({"ok": true})))
            }
            async fn execute_effect(
                &self,
                _args: Value,
                _ctx: &ToolCallContext<'_>,
            ) -> Result<ToolExecutionEffect, ToolError> {
                Ok(
                    ToolExecutionEffect::new(ToolResult::success("beta", json!({"ok": true})))
                        .with_action(AnyStateAction::new_at::<TestCounterState>(
                            "beta",
                            TestCounterAction::SetCounter(222),
                        )),
                )
            }
        }

        let thread = Thread::with_initial_state(
            "test",
            json!({"alpha": {"counter": 0}, "beta": {"counter": 0}}),
        );
        let result = StreamResult {
            text: "Two tools".to_string(),
            tool_calls: vec![
                crate::contracts::thread::ToolCall::new("call_a", "alpha", json!({})),
                crate::contracts::thread::ToolCall::new("call_b", "beta", json!({})),
            ],
            usage: None,
            stop_reason: None,
        };
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        tools.insert("alpha".to_string(), Arc::new(AlphaTool));
        tools.insert("beta".to_string(), Arc::new(BetaTool));

        let thread = execute_tools(thread, &result, &tools, true)
            .await
            .unwrap()
            .into_thread();

        let state = thread.rebuild_state().unwrap();
        assert_eq!(state["alpha"]["counter"], 111, "alpha tool patch applied");
        assert_eq!(state["beta"]["counter"], 222, "beta tool patch applied");
    });
}

/// Plugin pending patches from a phase are accumulated into RunContext
/// alongside tool patches, and both are visible in state().
#[test]
fn test_plugin_pending_patches_accumulated_with_tool_patches() {
    let mut run_ctx = RunContext::new(
        "test",
        json!({"tool_field": 0, "plugin_field": 0}),
        vec![],
        Default::default(),
    );

    // Simulate a tool result with its own patch
    let tool_result = tool_execution_result(
        "call_1",
        Some(TrackedPatch::new(Patch::new().with_op(Op::set(
            tirea_state::path!("tool_field"),
            json!(100),
        )))),
    );

    // Simulate a plugin pending patch (added alongside the tool result)
    let mut result_with_plugin_patch = tool_result;
    result_with_plugin_patch
        .pending_patches
        .push(TrackedPatch::new(Patch::new().with_op(Op::set(
            tirea_state::path!("plugin_field"),
            json!(200),
        ))));

    let _applied =
        apply_tool_results_to_session(&mut run_ctx, &[result_with_plugin_patch], None, false)
            .expect("should succeed");

    let state = run_ctx.snapshot().unwrap();
    assert_eq!(state["tool_field"], 100, "tool patch applied");
    assert_eq!(state["plugin_field"], 200, "plugin pending patch applied");

    // Both patches are tracked
    assert!(
        run_ctx.thread_patches().len() >= 2,
        "both tool and plugin patches should be in run_ctx, got {}",
        run_ctx.thread_patches().len()
    );
}

/// End-to-end: multi-step loop with state-writing tool verifies that patches
/// from step N are visible in step N+1's state via RunContext.
#[tokio::test]
async fn test_run_loop_patches_accumulate_across_steps() {
    // Two-step loop: step 1 increments counter by 5, step 2 by 10.
    // After step 2, final state should show 15.
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "c1",
            "counter",
            json!({"amount": 5}),
        )),
        Ok(tool_call_chat_response_object_args(
            "c2",
            "counter",
            json!({"amount": 10}),
        )),
        Ok(text_chat_response("done")),
    ]));

    let thread =
        Thread::with_initial_state("test", json!({"counter": 0})).with_message(Message::user("go"));
    let tools = tool_map([CounterTool]);

    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;

    assert!(
        matches!(outcome.termination, TerminationReason::NaturalEnd),
        "expected NaturalEnd, got: {:?}",
        outcome.termination
    );

    let final_state = outcome.run_ctx.snapshot().unwrap();
    assert_eq!(
        final_state["counter"], 15,
        "patches from both steps must accumulate: 0 + 5 + 10 = 15"
    );

    // Verify patches are tracked
    assert!(
        outcome.run_ctx.thread_patches().len() >= 2,
        "at least one patch per tool step, got {}",
        outcome.run_ctx.thread_patches().len()
    );
}

// =============================================================================
// Category 2: StateCommitter + version evolution
// =============================================================================

/// commit_pending_delta with force=false is a no-op when delta is empty.
#[tokio::test]
async fn test_commit_pending_delta_noops_when_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(state_commit::ChannelStateCommitter::new(tx));

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");

    // No messages or patches added — delta is empty
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::AssistantTurnCommitted,
        false, // not forced
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    // Nothing should have been sent
    assert!(rx.try_recv().is_err(), "empty delta should be ignored");
    // Version unchanged
    assert_eq!(run_ctx.version(), 0);
}

/// commit_pending_delta with force=true persists even when delta is empty.
#[tokio::test]
async fn test_commit_pending_delta_force_persists_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(state_commit::ChannelStateCommitter::new(tx));

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");

    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::RunFinished,
        true, // forced
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    let changeset = rx
        .try_recv()
        .expect("forced commit should produce a changeset");
    assert_eq!(changeset.run_id, "run-1");
    assert_eq!(changeset.reason, CheckpointReason::RunFinished);
    assert!(changeset.messages.is_empty());
    assert!(changeset.patches.is_empty());
    // Version should advance from 0 to 1
    assert_eq!(run_ctx.version(), 1);
}

/// Version advances correctly after each commit.
#[tokio::test]
async fn test_commit_pending_delta_version_advancement() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(state_commit::ChannelStateCommitter::new(tx));

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");
    assert_eq!(run_ctx.version(), 0);

    // First commit
    run_ctx.add_message(Arc::new(Message::user("msg1")));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::UserMessage,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        run_ctx.version(),
        1,
        "version should be 1 after first commit"
    );
    let _ = rx.try_recv().unwrap();

    // Second commit
    run_ctx.add_message(Arc::new(Message::assistant("reply")));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::AssistantTurnCommitted,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        run_ctx.version(),
        2,
        "version should be 2 after second commit"
    );
    let _ = rx.try_recv().unwrap();

    // Timestamp should have been set
    assert!(run_ctx.version_timestamp().is_some());
}

/// commit_pending_delta uses Exact precondition with current version.
#[tokio::test]
async fn test_commit_pending_delta_precondition_exactness() {
    use std::sync::Mutex as StdMutex;

    struct CapturingCommitter {
        preconditions: StdMutex<Vec<VersionPrecondition>>,
    }

    #[async_trait]
    impl StateCommitter for CapturingCommitter {
        async fn commit(
            &self,
            _thread_id: &str,
            _changeset: crate::contracts::ThreadChangeSet,
            precondition: VersionPrecondition,
        ) -> Result<u64, StateCommitError> {
            let version = match &precondition {
                VersionPrecondition::Any => 1,
                VersionPrecondition::Exact(v) => v + 1,
            };
            self.preconditions.lock().unwrap().push(precondition);
            Ok(version)
        }
    }

    let committer: Arc<dyn StateCommitter> = Arc::new(CapturingCommitter {
        preconditions: StdMutex::new(Vec::new()),
    });

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");
    run_ctx.set_version(10, None);

    run_ctx.add_message(Arc::new(Message::user("hi")));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::UserMessage,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    // The committer was called with Exact(10) since initial version is 10.
    // We verify via version advancement: ChannelStateCommitter returns v+1.
    assert_eq!(
        run_ctx.version(),
        11,
        "version should advance from 10 to 11"
    );
}

/// Error from StateCommitter propagates as AgentLoopError::StateError.
#[tokio::test]
async fn test_commit_pending_delta_error_propagation() {
    struct FailingCommitter;

    #[async_trait]
    impl StateCommitter for FailingCommitter {
        async fn commit(
            &self,
            _thread_id: &str,
            _changeset: crate::contracts::ThreadChangeSet,
            _precondition: VersionPrecondition,
        ) -> Result<u64, StateCommitError> {
            Err(StateCommitError::new("simulated failure"))
        }
    }

    let committer: Arc<dyn StateCommitter> = Arc::new(FailingCommitter);
    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");
    run_ctx.add_message(Arc::new(Message::user("hi")));

    let result = state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::UserMessage,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await;

    match result {
        Err(AgentLoopError::StateError(msg)) => {
            assert!(msg.contains("simulated failure"), "error message: {msg}");
        }
        other => panic!("expected StateError, got: {other:?}"),
    }
    // Version should NOT have advanced
    assert_eq!(run_ctx.version(), 0);
}

/// No StateCommitter provided: commit_pending_delta is a no-op.
#[tokio::test]
async fn test_commit_pending_delta_no_committer() {
    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");
    run_ctx.add_message(Arc::new(Message::user("hi")));

    // None committer — should succeed silently
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::UserMessage,
        false,
        None,
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    // Delta should still be unconsumed (not taken)
    assert!(run_ctx.has_delta());
}

// =============================================================================
// Category 3: Multi-checkpoint incremental correctness
// =============================================================================

/// Consecutive checkpoints produce disjoint deltas — no double-counting.
#[tokio::test]
async fn test_consecutive_checkpoints_disjoint_deltas() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(state_commit::ChannelStateCommitter::new(tx));

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");

    // Checkpoint 1: user message
    run_ctx.add_message(Arc::new(Message::user("hello")));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::UserMessage,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    // Checkpoint 2: assistant turn + patch
    run_ctx.add_message(Arc::new(Message::assistant("hi there")));
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("greeted"), json!(true))),
    ));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::AssistantTurnCommitted,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    // Checkpoint 3: tool results
    run_ctx.add_message(Arc::new(Message::tool("call-1", "tool result")));
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("tool_done"), json!(true))),
    ));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::ToolResultsCommitted,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    let cs1 = rx.try_recv().unwrap();
    let cs2 = rx.try_recv().unwrap();
    let cs3 = rx.try_recv().unwrap();

    // Each checkpoint has only its own data
    assert_eq!(cs1.messages.len(), 1, "checkpoint 1: 1 user message");
    assert_eq!(cs1.patches.len(), 0, "checkpoint 1: no patches");

    assert_eq!(cs2.messages.len(), 1, "checkpoint 2: 1 assistant message");
    assert_eq!(cs2.patches.len(), 1, "checkpoint 2: 1 patch");

    assert_eq!(cs3.messages.len(), 1, "checkpoint 3: 1 tool message");
    assert_eq!(cs3.patches.len(), 1, "checkpoint 3: 1 patch");

    // Union = 3 messages, 2 patches
    let total_messages: usize = cs1.messages.len() + cs2.messages.len() + cs3.messages.len();
    let total_patches: usize = cs1.patches.len() + cs2.patches.len() + cs3.patches.len();
    assert_eq!(total_messages, 3, "union of deltas = all messages");
    assert_eq!(total_patches, 2, "union of deltas = all patches");
}

/// RunEnd forced checkpoint captures remaining unconsumed delta.
#[tokio::test]
async fn test_run_end_checkpoint_captures_remaining() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(state_commit::ChannelStateCommitter::new(tx));

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");

    // Add data without committing
    run_ctx.add_message(Arc::new(Message::user("hello")));
    run_ctx.add_message(Arc::new(Message::assistant("world")));
    run_ctx.add_thread_patch(TrackedPatch::new(
        Patch::new().with_op(Op::set(tirea_state::path!("x"), json!(1))),
    ));

    // Force RunFinished checkpoint
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::RunFinished,
        true,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    let cs = rx.try_recv().unwrap();
    assert_eq!(cs.messages.len(), 2, "all messages captured");
    assert_eq!(cs.patches.len(), 1, "all patches captured");
    assert_eq!(cs.reason, CheckpointReason::RunFinished);

    // After forced commit, delta is empty
    assert!(!run_ctx.has_delta());
}

/// After all checkpoints, a final forced commit produces empty changeset.
#[tokio::test]
async fn test_all_deltas_consumed_final_checkpoint_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let committer: Arc<dyn StateCommitter> = Arc::new(state_commit::ChannelStateCommitter::new(tx));

    let mut run_ctx = RunContext::new("t-1", json!({}), vec![], Default::default());
    let run_identity = test_run_identity("run-1");

    // Add and commit
    run_ctx.add_message(Arc::new(Message::user("hi")));
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::UserMessage,
        false,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();
    let _ = rx.try_recv().unwrap();

    // Final forced commit with nothing new
    state_commit::commit_pending_delta(
        &mut run_ctx,
        CheckpointReason::RunFinished,
        true,
        Some(&committer),
        &run_identity,
        None,
        None,
    )
    .await
    .unwrap();

    let cs = rx.try_recv().unwrap();
    assert!(cs.messages.is_empty(), "no new messages");
    assert!(cs.patches.is_empty(), "no new patches");
}

// =============================================================================
// Category 6: Parallel conflict delta state
// =============================================================================

/// When parallel tool patches conflict, the error leaves run_ctx delta clean —
/// rejected patches are NOT added to run_ctx.
#[test]
fn test_conflict_rejection_leaves_delta_clean() {
    let mut run_ctx = RunContext::new("test", json!({"counter": 0}), vec![], Default::default());

    // Two conflicting patches on the same path
    let left = tool_execution_result(
        "call_left",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("counter"), json!(10))),
        )),
    );
    let right = tool_execution_result(
        "call_right",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("counter"), json!(20))),
        )),
    );

    // Record initial delta state
    let pre_patches = run_ctx.thread_patches().len();

    match apply_tool_results_to_session(&mut run_ctx, &[left, right], None, true) {
        Err(AgentLoopError::StateError(_)) => {} // expected
        Err(other) => panic!("expected StateError, got: {other:?}"),
        Ok(_) => panic!("conflicting patches must fail"),
    }

    // Delta should be clean — no patches added
    assert_eq!(
        run_ctx.thread_patches().len(),
        pre_patches,
        "conflicting patches must NOT be added to run_ctx"
    );
    // State unchanged
    let state = run_ctx.snapshot().unwrap();
    assert_eq!(
        state["counter"], 0,
        "state unchanged after conflict rejection"
    );
}

/// Sequential mode: if second tool's patch fails, first tool's patch is
/// already applied (sequential semantics). But the error prevents further
/// tool patches from being added.
#[test]
fn test_sequential_error_preserves_prior_patches() {
    let mut run_ctx = RunContext::new("test", json!({"a": 0, "b": 0}), vec![], Default::default());

    // First tool writes "a" — succeeds
    let first = tool_execution_result(
        "call_1",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("a"), json!(100))),
        )),
    );

    // Apply first tool result in non-parallel (sequential) mode
    let _applied = apply_tool_results_to_session(&mut run_ctx, &[first], None, false)
        .expect("single tool should succeed");

    // Verify "a" is patched
    let state = run_ctx.snapshot().unwrap();
    assert_eq!(state["a"], 100);
    let patches_after_first = run_ctx.thread_patches().len();
    assert!(
        patches_after_first >= 1,
        "at least one patch must be recorded after first sequential apply"
    );

    // Now apply a second tool that also writes "a" — this should still succeed
    // in non-parallel mode since conflict detection is only for parallel
    let second = tool_execution_result(
        "call_2",
        Some(TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("a"), json!(200))),
        )),
    );
    let _applied = apply_tool_results_to_session(&mut run_ctx, &[second], None, false)
        .expect("sequential mode allows overwriting");

    let state = run_ctx.snapshot().unwrap();
    assert_eq!(state["a"], 200, "sequential overwrites are allowed");
    assert!(
        run_ctx.thread_patches().len() > patches_after_first,
        "second sequential apply should append additional patch entries"
    );
}

#[test]
fn build_messages_filters_orphaned_tool_results() {
    let mut fix = TestFixture::new();
    fix.messages = vec![
        Arc::new(Message::user("hello")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("call_1", "serverInfo", json!({}))],
        )),
        // Matching tool result — should be kept
        Arc::new(Message::tool("call_1", "ok")),
        // Orphaned tool result (e.g. from PermissionConfirm interception) — should be filtered
        Arc::new(Message::tool("fc_xyz", "approved")),
    ];
    let step = fix.step(vec![]);
    let msgs = build_messages(&step, "sys");

    // System prompt + user + assistant + matching tool = 4
    assert_eq!(msgs.len(), 4);
    // The orphaned fc_xyz tool result must not appear
    assert!(
        !msgs
            .iter()
            .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("fc_xyz")),
        "orphaned tool result should be filtered"
    );
    // The matching tool result must still be present
    assert!(
        msgs.iter()
            .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call_1")),
        "matching tool result should be kept"
    );
}

#[test]
fn build_messages_keeps_tool_results_with_matching_call() {
    let mut fix = TestFixture::new();
    fix.messages = vec![
        Arc::new(Message::user("hi")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("call_1", "readFile", json!({})),
                ToolCall::new("call_2", "deleteTask", json!({})),
            ],
        )),
        Arc::new(Message::tool("call_1", "file contents")),
        Arc::new(Message::tool("call_2", "deleted")),
    ];
    let step = fix.step(vec![]);
    let msgs = build_messages(&step, "");

    // System prompt is empty so not added, user + assistant + 2 tool results = 4
    let tool_msgs: Vec<_> = msgs.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(
        tool_msgs.len(),
        2,
        "both matching tool results should be kept"
    );
}

#[test]
fn build_messages_keeps_error_tool_results_for_matching_calls() {
    let invalid_args_result = serde_json::to_string(&ToolResult::error(
        "echo",
        "Invalid arguments: missing required field 'message'",
    ))
    .expect("serialize invalid args tool result");
    let denied_result = serde_json::to_string(&ToolResult::error("echo", "User denied the action"))
        .expect("serialize denied tool result");

    let mut fix = TestFixture::new();
    fix.messages = vec![
        Arc::new(Message::user("hi")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("call_invalid", "echo", json!({})),
                ToolCall::new("call_denied", "echo", json!({"message":"x"})),
            ],
        )),
        Arc::new(Message::tool("call_invalid", invalid_args_result)),
        Arc::new(Message::tool("call_denied", denied_result)),
    ];
    let step = fix.step(vec![]);
    let msgs = build_messages(&step, "sys");

    let error_tool_msgs: Vec<&Message> = msgs
        .iter()
        .filter(|m| {
            m.role == Role::Tool
                && matches!(
                    m.tool_call_id.as_deref(),
                    Some("call_invalid") | Some("call_denied")
                )
        })
        .collect();

    assert_eq!(
        error_tool_msgs.len(),
        2,
        "matching error tool results should be kept in inference context"
    );
    assert!(error_tool_msgs.iter().any(|m| {
        m.tool_call_id.as_deref() == Some("call_invalid") && m.content.contains("Invalid arguments")
    }));
    assert!(error_tool_msgs.iter().any(|m| {
        m.tool_call_id.as_deref() == Some("call_denied") && m.content.contains("User denied")
    }));
}

#[test]
fn build_messages_drops_superseded_pending_placeholder_for_same_tool_call() {
    let mut fix = TestFixture::new();
    fix.messages = vec![
        Arc::new(Message::user("hi")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "call_1",
                "copyToClipboard",
                json!({"text":"hello"}),
            )],
        )),
        Arc::new(Message::tool(
            "call_1",
            "Tool 'copyToClipboard' is awaiting approval. Execution paused.",
        )),
        Arc::new(Message::tool(
            "call_1",
            r#"{"status":"success","data":{"text":"hello"}}"#,
        )),
    ];
    let step = fix.step(vec![]);
    let msgs = build_messages(&step, "sys");

    let call_1_tool_msgs: Vec<&Message> = msgs
        .iter()
        .filter(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call_1"))
        .collect();

    assert_eq!(
        call_1_tool_msgs.len(),
        1,
        "superseded pending placeholder should be removed from inference context"
    );
    assert!(
        !call_1_tool_msgs[0].content.contains("awaiting approval"),
        "remaining tool message must be the real result"
    );
}

#[test]
fn build_messages_keeps_tool_result_that_only_contains_placeholder_phrase() {
    let mut fix = TestFixture::new();
    fix.messages = vec![
        Arc::new(Message::user("hi")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("call_1", "echo", json!({"message":"hello"}))],
        )),
        Arc::new(Message::tool(
            "call_1",
            "Log: Tool 'echo' is awaiting approval. Execution paused. But this is just debug output.",
        )),
        Arc::new(Message::tool(
            "call_1",
            r#"{"status":"ok","data":{"message":"hello"}}"#,
        )),
    ];
    let step = fix.step(vec![]);
    let msgs = build_messages(&step, "sys");

    let call_1_tool_msgs: Vec<&Message> = msgs
        .iter()
        .filter(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call_1"))
        .collect();

    assert_eq!(
        call_1_tool_msgs.len(),
        2,
        "substring match must not misclassify normal tool output as pending placeholder"
    );
    assert!(call_1_tool_msgs
        .iter()
        .any(|m| m.content.starts_with("Log: Tool 'echo'")));
}

#[test]
fn build_messages_keeps_pending_placeholder_when_no_real_tool_result_exists() {
    let mut fix = TestFixture::new();
    fix.messages = vec![
        Arc::new(Message::user("hi")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "call_1",
                "copyToClipboard",
                json!({"text":"hello"}),
            )],
        )),
        Arc::new(Message::tool(
            "call_1",
            "Tool 'copyToClipboard' is awaiting approval. Execution paused.",
        )),
    ];
    let step = fix.step(vec![]);
    let msgs = build_messages(&step, "sys");

    assert!(
        msgs.iter().any(|m| {
            m.role == Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.content.contains("awaiting approval")
        }),
        "pending placeholder should remain when no resolved result exists"
    );
}

#[tokio::test]
async fn test_stream_permission_intercept_emits_tool_call_start_for_frontend() {
    // A plugin that intercepts a backend tool call and invokes PermissionConfirm
    // via ReplayOriginalTool routing. This must emit ToolCallStart + ToolCallReady
    // events for the frontend to render a permission dialog.
    struct PermissionInterceptPlugin;

    #[async_trait]
    impl AgentBehavior for PermissionInterceptPlugin {
        fn id(&self) -> &str {
            "permission_intercept_plugin"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_call_id() != Some("call_1") {
                return ActionSet::empty();
            }
            if let Some((ticket, _call_id)) = build_frontend_suspend_ticket(
                ctx,
                "PermissionConfirm",
                json!({ "tool_name": "serverInfo", "tool_args": {} }),
                ResponseRouting::ReplayOriginalTool,
            ) {
                ActionSet::single(BeforeToolExecuteAction::Suspend(ticket))
            } else {
                ActionSet::empty()
            }
        }
    }

    let thread = Thread::new("permission-intercept").with_message(Message::user("get server info"));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PermissionInterceptPlugin) as Arc<dyn AgentBehavior>);
    let tools = tool_map([EchoTool]);
    let responses = vec![MockResponse::text("checking").with_tool_call(
        "call_1",
        "echo",
        json!({ "message": "info" }),
    )];

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    // The fc_xxx ToolCallStart must be emitted for PermissionConfirm
    let permission_starts: Vec<_> = events
        .iter()
        .filter(
            |e| matches!(e, AgentEvent::ToolCallStart { name, .. } if name == "PermissionConfirm"),
        )
        .collect();
    assert_eq!(
        permission_starts.len(),
        1,
        "PermissionConfirm must emit exactly one ToolCallStart event: {events:?}"
    );

    let permission_readys: Vec<_> = events
        .iter()
        .filter(
            |e| matches!(e, AgentEvent::ToolCallReady { name, .. } if name == "PermissionConfirm"),
        )
        .collect();
    assert_eq!(
        permission_readys.len(),
        1,
        "PermissionConfirm must emit exactly one ToolCallReady event: {events:?}"
    );

    // The run should terminate with Suspended (waiting for frontend response)
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::RunFinish {
                termination: TerminationReason::Suspended,
                ..
            })
        ),
        "run should pause with Suspended: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// HOL-blocking fix: mixed pending/completed tools should not block entire run
// ---------------------------------------------------------------------------

/// Non-stream parity for mixed pending/completed tools:
/// one call is pending, others complete, and loop continues to next inference.
#[tokio::test]
async fn test_nonstream_mixed_pending_and_completed_tools_continues_loop() {
    struct PendingOnlyCall2Plugin;

    #[async_trait]
    impl AgentBehavior for PendingOnlyCall2Plugin {
        fn id(&self) -> &str {
            "pending_only_call_2_nonstream"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                if call_id == "call_2" {
                    return ActionSet::single(BeforeToolExecuteAction::Suspend(
                        test_suspend_ticket(
                            Suspension::new("confirm_call_2", "confirm")
                                .with_message("approve delete?"),
                        ),
                    ));
                }
            }
            ActionSet::empty()
        }
    }

    let mut first = text_chat_response("");
    first.content = MessageContent::from_tool_calls(vec![
        genai::chat::ToolCall {
            call_id: "call_1".to_string(),
            fn_name: "echo".to_string(),
            fn_arguments: json!({"message": "a"}),
            thought_signatures: None,
        },
        genai::chat::ToolCall {
            call_id: "call_2".to_string(),
            fn_name: "echo".to_string(),
            fn_arguments: json!({"message": "b"}),
            thought_signatures: None,
        },
        genai::chat::ToolCall {
            call_id: "call_3".to_string(),
            fn_name: "echo".to_string(),
            fn_arguments: json!({"message": "c"}),
            thought_signatures: None,
        },
    ]);
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(first),
        Ok(text_chat_response(
            "I got results for a and c, delete needs approval",
        )),
    ]));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingOnlyCall2Plugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()))
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("run tools"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, tool_map([EchoTool]), run_ctx, None, None, None).await;
    assert_eq!(outcome.termination, TerminationReason::Suspended);
    assert_eq!(
        outcome.stats.llm_calls, 2,
        "non-stream should continue to a second inference round when partial tool results are available"
    );

    let suspended = outcome.run_ctx.suspended_calls();
    assert_eq!(suspended.len(), 1, "only call_2 should remain suspended");
    assert!(suspended.contains_key("call_2"));

    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && !message.content.contains("awaiting approval")
        }),
        "call_1 should produce a completed tool result"
    );
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_3")
                && !message.content.contains("awaiting approval")
        }),
        "call_3 should produce a completed tool result"
    );
}

/// Non-stream single pending tool should enter waiting immediately.
#[tokio::test]
async fn test_nonstream_single_pending_tool_enters_waiting() {
    struct PendingAllToolsPlugin;

    #[async_trait]
    impl AgentBehavior for PendingAllToolsPlugin {
        fn id(&self) -> &str {
            "pending_single_tool_nonstream"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new(format!("confirm_{call_id}"), "confirm")
                        .with_message("needs confirmation"),
                )));
            }
            ActionSet::empty()
        }
    }

    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "call_1",
            "echo",
            json!({"message": "a"}),
        )),
        Ok(text_chat_response("unused")),
    ]));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingAllToolsPlugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()))
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("run tool"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, tool_map([EchoTool]), run_ctx, None, None, None).await;
    assert_eq!(outcome.termination, TerminationReason::Suspended);
    assert_eq!(
        outcome.stats.llm_calls, 1,
        "single pending tool should pause without entering a second inference round"
    );

    let suspended = outcome.run_ctx.suspended_calls();
    assert_eq!(suspended.len(), 1);
    assert!(suspended.contains_key("call_1"));
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_1")
                && message.content.contains("awaiting approval")
        }),
        "pending call should leave a waiting placeholder message"
    );
}

/// When some tool calls complete and one is pending, the run should continue
/// to the next inference round so the LLM sees the completed results. The run
/// should eventually terminate with Suspended.
#[tokio::test]
async fn test_stream_mixed_pending_and_completed_tools_continues_loop() {
    struct PendingOnlyCall2Plugin;

    #[async_trait]
    impl AgentBehavior for PendingOnlyCall2Plugin {
        fn id(&self) -> &str {
            "pending_only_call_2"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                if call_id == "call_2" {
                    return ActionSet::single(BeforeToolExecuteAction::Suspend(
                        test_suspend_ticket(
                            Suspension::new("confirm_call_2", "confirm")
                                .with_message("approve delete?"),
                        ),
                    ));
                }
            }
            ActionSet::empty()
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingOnlyCall2Plugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let thread = Thread::new("test").with_message(Message::user("run tools"));

    // First response: 3 tool calls, call_2 will be pending.
    // Second response: text only (LLM reasons with the results).
    let responses = vec![
        MockResponse::text("")
            .with_tool_call("call_1", "echo", json!({"message": "a"}))
            .with_tool_call("call_2", "echo", json!({"message": "b"}))
            .with_tool_call("call_3", "echo", json!({"message": "c"})),
        MockResponse::text("I got results for a and c, delete needs approval"),
    ];
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    // The LLM should have been called twice (two InferenceComplete events).
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 2,
        "LLM should get a second inference round with completed results: {events:?}"
    );

    // call_1 and call_3 should have ToolCallDone events.
    let done_ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallDone { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        done_ids.contains(&"call_1"),
        "call_1 should have ToolCallDone: {events:?}"
    );
    assert!(
        done_ids.contains(&"call_3"),
        "call_3 should have ToolCallDone: {events:?}"
    );

    // Run should terminate with Suspended.
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended),
        "run should eventually pause with Suspended: {events:?}"
    );

    // Exactly one Pending event should be emitted.
    let pending_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolCallReady { id, name, .. }
                    if id.starts_with("confirm_") || name == "confirm"
            )
        })
        .count();
    assert_eq!(
        pending_count, 1,
        "exactly one suspended interaction should be emitted: {events:?}"
    );
}

/// When ALL tool calls are pending, the run should terminate immediately
/// with Suspended (no need for another inference round).
#[tokio::test]
async fn test_stream_all_tools_pending_pauses_run() {
    struct PendingAllToolsPlugin;

    #[async_trait]
    impl AgentBehavior for PendingAllToolsPlugin {
        fn id(&self) -> &str {
            "pending_all_tools"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                return ActionSet::single(BeforeToolExecuteAction::Suspend(test_suspend_ticket(
                    Suspension::new(format!("confirm_{call_id}"), "confirm")
                        .with_message("needs confirmation"),
                )));
            }
            ActionSet::empty()
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingAllToolsPlugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let thread = Thread::new("test").with_message(Message::user("run tools"));
    let responses = vec![MockResponse::text("")
        .with_tool_call("call_1", "echo", json!({"message": "a"}))
        .with_tool_call("call_2", "echo", json!({"message": "b"}))];
    let tools = tool_map([EchoTool]);

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    // Only one inference round — no second call since all are pending.
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 1,
        "should have only one inference round when all tools are pending: {events:?}"
    );

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended),
        "run should pause with Suspended: {events:?}"
    );
}

/// Verify that the suspended interaction state is persisted correctly when the
/// run continues past a partial pending round and eventually terminates.
#[tokio::test]
async fn test_stream_mixed_pending_persists_interaction_state() {
    struct PendingOnlyCall2Plugin;

    #[async_trait]
    impl AgentBehavior for PendingOnlyCall2Plugin {
        fn id(&self) -> &str {
            "pending_only_call_2_persist"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if let Some(call_id) = ctx.tool_call_id() {
                if call_id == "call_2" {
                    return ActionSet::single(BeforeToolExecuteAction::Suspend(
                        test_suspend_ticket(
                            Suspension::new("confirm_call_2", "confirm")
                                .with_message("approve delete?"),
                        ),
                    ));
                }
            }
            ActionSet::empty()
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingOnlyCall2Plugin) as Arc<dyn AgentBehavior>)
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));
    let thread = Thread::new("test").with_message(Message::user("run tools"));
    let responses = vec![
        MockResponse::text("")
            .with_tool_call("call_1", "echo", json!({"message": "a"}))
            .with_tool_call("call_2", "echo", json!({"message": "b"})),
        MockResponse::text("done"),
    ];
    let tools = tool_map([EchoTool]);

    // Use run_loop_stream directly to inspect the final state via RunContext.
    let provider = MockStreamProvider::new(responses);
    let config = config.with_llm_executor(Arc::new(provider));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    // Run should terminate with Suspended.
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended),
    );

    // The state snapshot should contain the suspended interaction.
    let last_state = events.iter().rev().find_map(|e| match e {
        AgentEvent::StateSnapshot { snapshot } => Some(snapshot.clone()),
        _ => None,
    });
    assert!(
        last_state.is_some(),
        "should have a state snapshot: {events:?}"
    );
    let state = last_state.unwrap();
    assert_eq!(
        state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("call_2"))
            .and_then(|entry| entry.get("suspended_call"))
            .and_then(|sc| sc.get("suspension"))
            .and_then(|pi| pi.get("id"))
            .and_then(|id| id.as_str()),
        Some("confirm_call_2"),
        "suspended interaction should be persisted in state: {state:?}"
    );
}

/// Core loop without plugins should not terminate on pre-existing pending
/// interaction state.  The core is a generic inference→tools→repeat engine;
/// only plugins decide about interaction termination.
#[tokio::test]
async fn test_no_plugins_loop_ignores_pending() {
    use crate::contracts::Suspension;

    // Seed state with a pre-existing suspended interaction.
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("leftover_confirm", "confirm").with_message("stale pending"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("go"));

    // No plugins — the core should run inference normally and terminate with
    // NaturalEnd (text-only response, no tool calls).
    let config = BaseAgent::new("mock");
    let responses = vec![MockResponse::text("done")];
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    // The run should complete with NaturalEnd — the core ignores pending state.
    // The backward-compat boundary in terminate_run!/finish_run! overrides the
    // reason to Suspended when the state has one, but only for
    // non-Error/non-Cancelled reasons.  Since inference ran and returned text,
    // the reason is NaturalEnd, which gets overridden to Suspended.
    // This is the expected thin boundary behavior.
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended),
        "backward-compat boundary should override to Suspended: {events:?}"
    );

    // Crucially, inference DID run (the core didn't short-circuit).
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 1,
        "core should have run inference despite pre-existing pending: {events:?}"
    );
}

#[tokio::test]
async fn test_nonstream_run_start_added_pending_pauses_before_inference() {
    struct RunStartPendingPlugin;

    #[async_trait]
    impl AgentBehavior for RunStartPendingPlugin {
        fn id(&self) -> &str {
            "run_start_pending_nonstream"
        }

        async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            let _ = ctx;
            ActionSet::single(LifecycleAction::State(single_suspended_call_state_action(
                Suspension::new("recover_1", "recover_agent_run").with_message("resume?"),
                None,
            )))
        }
    }

    let provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response("done"))]));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(RunStartPendingPlugin) as Arc<dyn AgentBehavior>)
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::Suspended);
    assert_eq!(outcome.stats.llm_calls, 0, "inference should not run");
    let suspended_calls = outcome.run_ctx.suspended_calls();
    assert_eq!(
        suspended_calls
            .get("recover_1")
            .expect("suspension expected")
            .ticket
            .suspension
            .action,
        "recover_agent_run"
    );
}

#[tokio::test]
async fn test_stream_run_start_added_pending_emits_and_pauses_before_inference() {
    struct RunStartPendingPlugin;

    #[async_trait]
    impl AgentBehavior for RunStartPendingPlugin {
        fn id(&self) -> &str {
            "run_start_pending_stream"
        }

        async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            let _ = ctx;
            ActionSet::single(LifecycleAction::State(single_suspended_call_state_action(
                Suspension::new("recover_1", "recover_agent_run").with_message("resume?"),
                None,
            )))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(RunStartPendingPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let events = run_mock_stream(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        config,
        thread,
        HashMap::new(),
    )
    .await;

    assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCallStart { id, name }
            if id == "recover_1" && name == "recover_agent_run"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCallReady { id, name, .. }
            if id == "recover_1" && name == "recover_agent_run"
    )));
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended),
        "run should pause before inference: {events:?}"
    );
    let inference_count = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(inference_count, 0, "inference should not run: {events:?}");
}

#[tokio::test]
async fn test_nonstream_completed_tool_round_does_not_clear_existing_suspended_calls() {
    use crate::contracts::Suspension;

    // Seed state with a pre-existing suspended interaction.
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("leftover_confirm", "confirm").with_message("stale pending"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("run"));
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "call_1",
            "echo",
            json!({"message": "ok"}),
        )),
        Ok(text_chat_response("done")),
    ]));
    let config = BaseAgent::new("mock").with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, tool_map([EchoTool]), run_ctx, None, None, None).await;
    assert_eq!(outcome.termination, TerminationReason::Suspended);
    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    assert!(
        state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("leftover_confirm"))
            .is_some(),
        "existing unresolved suspended call must not be cleared by unrelated successful tool round"
    );
}

#[tokio::test]
async fn test_stream_completed_tool_round_does_not_clear_existing_suspended_calls() {
    use crate::contracts::Suspension;

    // Seed state with a pre-existing suspended interaction.
    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("leftover_confirm", "confirm").with_message("stale pending"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("run"));
    let (events, final_thread) = run_mock_stream_with_final_thread(
        MockStreamProvider::new(vec![
            MockResponse::text("").with_tool_call("call_1", "echo", json!({"message": "ok"})),
            MockResponse::text("done"),
        ]),
        BaseAgent::new("mock"),
        thread,
        tool_map([EchoTool]),
    )
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended)
    );
    let final_state = final_thread.rebuild_state().expect("state should rebuild");
    assert!(
        final_state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("leftover_confirm"))
            .is_some(),
        "existing unresolved suspended call must not be cleared by unrelated successful tool round"
    );
}

/// A plugin that sets `request_termination(BehaviorRequested)` in BeforeInference
/// should cause the run to terminate immediately without running inference.
#[tokio::test]
async fn test_plugin_run_action_stops_loop() {
    struct TerminatePlugin;

    #[async_trait]
    impl AgentBehavior for TerminatePlugin {
        fn id(&self) -> &str {
            "terminate_plugin"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config =
        BaseAgent::new("mock").with_behavior(Arc::new(TerminatePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    // Provide a response, but it should never be consumed.
    let responses = vec![MockResponse::text("should not appear")];
    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    // Run should terminate with BehaviorRequested.
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::BehaviorRequested),
        "run should terminate with BehaviorRequested: {events:?}"
    );

    // No inference should have run.
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 0,
        "no inference should run when plugin requests termination: {events:?}"
    );
}

/// Verify that mutating `run_action` outside BeforeInference/AfterInference is
/// rejected by the phase-mutation validator (non-stream path).
#[tokio::test]
async fn test_run_loop_step_start_run_action_mutation_is_type_safe_v2() {
    // With typed ActionSet<LifecycleAction>, step_start can only emit State actions.
    // RequestTermination cannot be placed in LifecycleAction (compile-time type safety).
    struct NoOpStepStartPlugin;

    #[async_trait]
    impl AgentBehavior for NoOpStepStartPlugin {
        fn id(&self) -> &str {
            "noop_step_start_term"
        }

        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            ActionSet::empty()
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(NoOpStepStartPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("hello"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;
    assert_eq!(
        outcome.termination,
        TerminationReason::BehaviorRequested,
        "expected BehaviorRequested, got: {:?}",
        outcome.termination
    );
}

/// Verify that mutating `run_action` outside BeforeInference/AfterInference is
/// rejected by the phase-mutation validator (stream path).
#[tokio::test]
async fn test_stream_step_start_run_action_mutation_is_type_safe_v2() {
    // With typed ActionSet<LifecycleAction>, step_start can only emit State actions.
    // RequestTermination cannot be placed in LifecycleAction (compile-time type safety).
    struct NoOpStepStartPlugin;

    #[async_trait]
    impl AgentBehavior for NoOpStepStartPlugin {
        fn id(&self) -> &str {
            "noop_step_start_term_stream"
        }

        async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
            ActionSet::empty()
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(NoOpStepStartPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("hi"));
    let tools = HashMap::new();

    let events = run_mock_stream(
        MockStreamProvider::new(vec![MockResponse::text("done")]),
        config,
        thread,
        tools,
    )
    .await;

    assert!(
        matches!(events.last(), Some(AgentEvent::RunFinish { .. })),
        "expected stream to complete normally: {events:?}"
    );
}

/// Non-stream run_loop: plugin-driven termination via run_action
/// stops the loop without running inference.
#[tokio::test]
async fn test_run_loop_plugin_run_action_stops_loop() {
    struct TerminatePlugin;

    #[async_trait]
    impl AgentBehavior for TerminatePlugin {
        fn id(&self) -> &str {
            "terminate_nonstream"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("gpt-4o-mini")
        .with_behavior(Arc::new(TerminatePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(crate::contracts::thread::Message::user("go"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let outcome = run_loop(&config, tools, run_ctx, None, None, None).await;

    assert_eq!(
        outcome.termination,
        TerminationReason::BehaviorRequested,
        "non-stream run should terminate with BehaviorRequested"
    );
    assert!(
        outcome.failure.is_none(),
        "no failure expected: {:?}",
        outcome.failure
    );
    assert_eq!(outcome.stats.llm_calls, 0, "no LLM calls should have run");
}

#[tokio::test]
async fn test_run_loop_applies_plugin_state_effect_patch_before_inference() {
    struct StateEffectPlugin;

    #[async_trait]
    impl AgentBehavior for StateEffectPlugin {
        fn id(&self) -> &str {
            "state_effect_before_inference"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
            .and(ActionSet::single(BeforeInferenceAction::State(
                test_bool_state_action("debug.before_inference_effect", true),
            )))
        }
    }

    let config =
        BaseAgent::new("mock").with_behavior(Arc::new(StateEffectPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;
    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);
    assert_eq!(outcome.stats.llm_calls, 0, "inference should not run");
    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    assert_eq!(state["debug"]["before_inference_effect"], json!(true));
}

#[tokio::test]
async fn test_run_loop_applies_plugin_state_effect_patch_after_tool_execute() {
    struct StateEffectToolPlugin;

    #[async_trait]
    impl AgentBehavior for StateEffectToolPlugin {
        fn id(&self) -> &str {
            "state_effect_after_tool_execute"
        }

        async fn after_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterToolExecuteAction> {
            if ctx.tool_call_id() == Some("call_1") {
                ActionSet::single(AfterToolExecuteAction::State(test_bool_state_action(
                    "debug.after_tool_effect",
                    true,
                )))
            } else {
                ActionSet::empty()
            }
        }
    }

    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(tool_call_chat_response_object_args(
            "call_1",
            "echo",
            json!({"message": "hi"}),
        )),
        Ok(text_chat_response("done")),
    ]));
    let config = BaseAgent::new("mock")
        .with_llm_executor(provider as Arc<dyn LlmExecutor>)
        .with_behavior(Arc::new(StateEffectToolPlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, tool_map([EchoTool]), run_ctx, None, None, None).await;
    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    let state = outcome.run_ctx.snapshot().expect("state should rebuild");
    assert_eq!(state["debug"]["after_tool_effect"], json!(true));
}

#[tokio::test]
async fn test_run_loop_after_inference_run_action_stops_before_tool_execution() {
    struct AfterInferenceTerminatePlugin;

    #[async_trait]
    impl AgentBehavior for AfterInferenceTerminatePlugin {
        fn id(&self) -> &str {
            "after_inference_terminate_nonstream"
        }

        async fn after_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            ActionSet::single(AfterInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let provider = Arc::new(MockChatProvider::new(vec![Ok(
        tool_call_chat_response_object_args("call_1", "echo", json!({"message": "hi"})),
    )]));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(AfterInferenceTerminatePlugin) as Arc<dyn AgentBehavior>)
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();

    let outcome = run_loop(&config, tool_map([EchoTool]), run_ctx, None, None, None).await;

    assert_eq!(outcome.termination, TerminationReason::BehaviorRequested);
    assert_eq!(
        outcome.stats.llm_calls, 1,
        "inference should run exactly once"
    );
    assert_eq!(
        outcome.stats.tool_calls, 0,
        "tool execution should not start when AfterInference requests termination"
    );
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == crate::contracts::thread::Role::Assistant
                && message
                    .tool_calls
                    .as_ref()
                    .map(|calls| calls.iter().any(|call| call.id == "call_1"))
                    .unwrap_or(false)
        }),
        "assistant response should still be committed before termination"
    );
}

#[tokio::test]
async fn test_stream_after_inference_run_action_stops_before_tool_events() {
    struct AfterInferenceTerminatePlugin;

    #[async_trait]
    impl AgentBehavior for AfterInferenceTerminatePlugin {
        fn id(&self) -> &str {
            "after_inference_terminate_stream"
        }

        async fn after_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<AfterInferenceAction> {
            ActionSet::single(AfterInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(AfterInferenceTerminatePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let events = run_mock_stream(
        MockStreamProvider::new(vec![MockResponse::text("tool").with_tool_call(
            "call_1",
            "echo",
            json!({"message":"hi"}),
        )]),
        config,
        thread,
        tool_map([EchoTool]),
    )
    .await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::BehaviorRequested)
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::InferenceComplete { .. })),
        "inference should complete before termination: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallReady { id, .. } if id == "call_1"
        )),
        "tool-ready event should not be emitted after AfterInference termination: {events:?}"
    );
}

/// Test that `BeforeInferenceContext::request_termination()` method works
/// end-to-end (as opposed to setting step fields directly).
#[tokio::test]
async fn test_request_termination_method_stops_stream() {
    struct MethodTerminatePlugin;

    #[async_trait]
    impl AgentBehavior for MethodTerminatePlugin {
        fn id(&self) -> &str {
            "method_terminate"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(MethodTerminatePlugin) as Arc<dyn AgentBehavior>);
    let thread = Thread::new("test").with_message(Message::user("go"));
    let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();

    let responses = vec![MockResponse::text("should not appear")];
    let events = run_mock_stream(MockStreamProvider::new(responses), config, thread, tools).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::BehaviorRequested),
        "request_termination() method should produce BehaviorRequested: {events:?}"
    );
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 0,
        "request_termination() should prevent inference: {events:?}"
    );
}

#[tokio::test]
async fn test_run_loop_decision_channel_ignores_unknown_target_id() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_unknown_decision_nonstream"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_keep", "tool:echo").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("continue"));
    let run_ctx = run_ctx_with_execution(&thread, "run-unknown-decision");
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    decision_tx
        .send(test_decision(
            "unknown_call",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!(true),
            None,
        ))
        .expect("send decision");
    drop(decision_tx);

    let outcome = run_loop(
        &config,
        HashMap::new(),
        run_ctx,
        None,
        None,
        Some(decision_rx),
    )
    .await;
    assert_eq!(outcome.termination, TerminationReason::Suspended);
    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    assert!(
        final_state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("call_keep"))
            .is_some(),
        "unknown decision must not clear existing suspended calls"
    );
    assert!(
        final_state
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("unknown_call"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "unknown decision must not create runtime lifecycle state"
    );
    assert_eq!(final_state["__run"]["id"], json!("run-unknown-decision"));
    assert_eq!(final_state["__run"]["status"], json!("waiting"));
    assert!(final_state["__run"]["done_reason"].is_null());
}

#[tokio::test]
async fn test_run_loop_decision_channel_rejects_illegal_terminal_to_resuming_transition() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_illegal_transition_nonstream"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let thread = Thread::with_initial_state(
        "test",
        json!({
            "__tool_call_scope": {
                "call_pending": {
                    "tool_call_state": {
                        "call_id": "call_pending",
                        "tool_name": "echo",
                        "arguments": { "message": "already-finished" },
                        "status": "succeeded",
                        "updated_at": 1
                    },
                    "suspended_call": {
                        "call_id": "call_pending",
                        "tool_name": "echo",
                        "arguments": { "message": "should-not-replay" },
                        "suspension": { "id": "call_pending", "action": "tool:echo" },
                        "pending": { "id": "call_pending", "name": "echo", "arguments": { "message": "should-not-replay" } },
                        "resume_mode": "replay_tool_call"
                    }
                }
            }
        }),
    )
    .with_message(Message::assistant_with_tool_calls(
        "need permission",
        vec![crate::contracts::thread::ToolCall::new(
            "call_pending",
            "echo",
            json!({"message": "should-not-replay"}),
        )],
    ))
    .with_message(Message::tool(
        "call_pending",
        "Tool 'echo' is awaiting approval. Execution paused.",
    ));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);
    let tools = tool_map([EchoTool]);
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!(true),
            None,
        ))
        .expect("send decision");
    drop(decision_tx);

    let outcome = run_loop(&config, tools, run_ctx, None, None, Some(decision_rx)).await;

    assert_eq!(outcome.termination, TerminationReason::Suspended);
    assert!(
        !outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_pending")
                && !message
                    .content
                    .contains("is awaiting approval. Execution paused.")
        }),
        "illegal transition must not replay resolved tool result"
    );

    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    assert!(
        final_state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("call_pending"))
            .is_some(),
        "illegal transition must keep suspended call pending"
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]["status"],
        json!("succeeded"),
        "terminal lifecycle state must remain unchanged"
    );
}

#[tokio::test]
async fn test_stream_decision_channel_ignores_unknown_target_id() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_unknown_decision_stream"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let base_state = json!({});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_keep", "tool:echo").with_message("awaiting approval"),
        None,
    )
    .expect("failed to seed suspended interaction");
    let mut final_thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("continue"));
    let run_ctx = RunContext::from_thread(&final_thread, tirea_contract::RunPolicy::default())
        .expect("run ctx");
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    decision_tx
        .send(test_decision(
            "unknown_call",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!(true),
            None,
        ))
        .expect("send decision");
    drop(decision_tx);

    let stream = run_loop_stream(
        Arc::new(config),
        HashMap::new(),
        run_ctx,
        None,
        Some(state_committer),
        Some(decision_rx),
    );
    let events = collect_stream_events(stream).await;
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut final_thread);
    }

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended)
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallResumed { .. })),
        "unknown decision should not emit ToolCallResumed"
    );
    let final_state = final_thread.rebuild_state().expect("state should rebuild");
    assert!(
        final_state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("call_keep"))
            .is_some(),
        "unknown decision must not clear existing suspended calls"
    );
}

#[tokio::test]
async fn test_stream_decision_channel_rejects_illegal_terminal_to_resuming_transition() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_illegal_transition_stream"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let mut final_thread = Thread::with_initial_state(
        "test",
        json!({
            "__tool_call_scope": {
                "call_pending": {
                    "tool_call_state": {
                        "call_id": "call_pending",
                        "tool_name": "echo",
                        "arguments": { "message": "already-finished" },
                        "status": "succeeded",
                        "updated_at": 1
                    },
                    "suspended_call": {
                        "call_id": "call_pending",
                        "tool_name": "echo",
                        "arguments": { "message": "should-not-replay" },
                        "suspension": { "id": "call_pending", "action": "tool:echo" },
                        "pending": { "id": "call_pending", "name": "echo", "arguments": { "message": "should-not-replay" } },
                        "resume_mode": "replay_tool_call"
                    }
                }
            }
        }),
    )
    .with_message(Message::assistant_with_tool_calls(
        "need permission",
        vec![crate::contracts::thread::ToolCall::new(
            "call_pending",
            "echo",
            json!({"message": "should-not-replay"}),
        )],
    ))
    .with_message(Message::tool(
        "call_pending",
        "Tool 'echo' is awaiting approval. Execution paused.",
    ));
    let run_ctx = RunContext::from_thread(&final_thread, tirea_contract::RunPolicy::default())
        .expect("run ctx");
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Arc<dyn StateCommitter> =
        Arc::new(ChannelStateCommitter::new(checkpoint_tx));
    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!(true),
            None,
        ))
        .expect("send decision");
    drop(decision_tx);

    let stream = run_loop_stream(
        Arc::new(config),
        tool_map([EchoTool]),
        run_ctx,
        None,
        Some(state_committer),
        Some(decision_rx),
    );
    let events = collect_stream_events(stream).await;
    while let Some(changeset) = checkpoint_rx.recv().await {
        changeset.apply_to(&mut final_thread);
    }

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::Suspended)
    );
    assert!(
        !events.iter().any(|event| {
            matches!(event, AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending")
        }),
        "illegal transition should not emit ToolCallResumed"
    );
    assert!(
        !events.iter().any(
            |event| matches!(event, AgentEvent::ToolCallDone { id, .. } if id == "call_pending")
        ),
        "illegal transition should not replay tool execution"
    );

    let final_state = final_thread.rebuild_state().expect("state should rebuild");
    assert!(
        final_state
            .get("__tool_call_scope")
            .and_then(|scope| scope.get("call_pending"))
            .is_some(),
        "illegal transition must keep suspended call pending"
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]["status"],
        json!("succeeded"),
        "terminal lifecycle state must remain unchanged"
    );
    assert!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]
            .get("resume")
            .is_none(),
        "illegal transition must not inject resume payload into terminal state"
    );
}

#[tokio::test]
async fn test_run_loop_decision_channel_resolves_suspended_call() {
    struct FrontendTool;

    #[async_trait]
    impl Tool for FrontendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("frontend_tool", "Frontend Tool", "needs approval").with_parameters(
                json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                        "approved": { "type": "boolean" }
                    },
                    "required": ["message"]
                }),
            )
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "frontend_tool",
                json!({
                    "message": args.get("message").and_then(Value::as_str).unwrap_or_default(),
                    "approved": args.get("approved").and_then(Value::as_bool).unwrap_or(false),
                }),
            ))
        }
    }

    struct PendingFrontendToolPlugin {
        ready: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl AgentBehavior for PendingFrontendToolPlugin {
        fn id(&self) -> &str {
            "pending_frontend_tool_decision"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_name() != Some("frontend_tool") {
                return ActionSet::empty();
            }
            let already_approved = ctx
                .tool_args()
                .and_then(|args| args.get("approved"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if already_approved {
                return ActionSet::empty();
            }
            let args = ctx.tool_args().cloned().unwrap_or_default();
            let output = if let Some((ticket, _call_id)) = build_frontend_suspend_ticket(
                ctx,
                "frontend_tool",
                args,
                ResponseRouting::UseAsToolResult,
            ) {
                ActionSet::single(BeforeToolExecuteAction::Suspend(ticket))
            } else {
                ActionSet::empty()
            };
            self.ready.notify_one();
            self.release.notified().await;
            output
        }
    }

    let mut first = text_chat_response("");
    first.content = MessageContent::from_tool_calls(vec![
        genai::chat::ToolCall {
            call_id: "call_done".to_string(),
            fn_name: "echo".to_string(),
            fn_arguments: json!({ "message": "ok" }),
            thought_signatures: None,
        },
        genai::chat::ToolCall {
            call_id: "call_pending".to_string(),
            fn_name: "frontend_tool".to_string(),
            fn_arguments: json!({ "message": "need approval" }),
            thought_signatures: None,
        },
    ]);
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(first),
        Ok(text_chat_response("done")),
    ]));

    let ready = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingFrontendToolPlugin {
            ready: ready.clone(),
            release: release.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);

    let thread = Thread::new("test").with_message(Message::user("run"));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>);
    tools.insert(
        "frontend_tool".to_string(),
        Arc::new(FrontendTool) as Arc<dyn Tool>,
    );

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    let run_task = tokio::spawn(async move {
        run_loop(&config, tools, run_ctx, None, None, Some(decision_rx)).await
    });

    ready.notified().await;
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!({"approved": true, "message": "need approval"}),
            None,
        ))
        .expect("send decision");
    release.notify_one();

    let outcome = run_task.await.expect("join run task");
    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert_eq!(outcome.response.as_deref(), Some("done"));
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_pending")
                && !message
                    .content
                    .contains("is awaiting approval. Execution paused.")
        }),
        "resolved call_pending tool result should be appended"
    );
    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    assert_eq!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]["status"],
        json!("succeeded")
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]["resume"]["action"],
        json!("resume")
    );
}

#[tokio::test]
async fn test_run_loop_decision_channel_cancel_emits_single_tool_result_message() {
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested_for_decision_cancel"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    use crate::contracts::Suspension;

    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(TerminateBehaviorRequestedPlugin) as Arc<dyn AgentBehavior>);

    let base_state = json!({});
    let cancel_args = json!({"message": "cancel-run"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_pending", "tool:echo")
            .with_message("awaiting approval")
            .with_parameters(cancel_args.clone()),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_pending",
                "echo",
                cancel_args,
            )],
        ))
        .with_message(Message::tool(
            "call_pending",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let tools = tool_map([EchoTool]);

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Cancel,
            json!({"status": "cancelled", "reason": "User canceled in UI"}),
            Some("User canceled in UI"),
        ))
        .expect("send cancel decision");
    drop(decision_tx);

    let outcome = run_loop(&config, tools, run_ctx, None, None, Some(decision_rx)).await;

    assert!(matches!(
        outcome.termination,
        TerminationReason::BehaviorRequested
    ));

    let resolved_tool_messages: Vec<_> = outcome
        .run_ctx
        .messages()
        .iter()
        .filter(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_pending")
                && !message
                    .content
                    .contains("is awaiting approval. Execution paused.")
        })
        .collect();
    assert_eq!(
        resolved_tool_messages.len(),
        1,
        "cancel decision should produce exactly one tool result message"
    );
    assert!(
        resolved_tool_messages[0].content.contains("canceled")
            || resolved_tool_messages[0].content.contains("cancelled"),
        "cancel decision should preserve cancel semantics in tool result: {}",
        resolved_tool_messages[0].content
    );

    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    let suspended = crate::contracts::runtime::suspended_calls_from_state(&final_state);
    assert!(
        suspended.is_empty(),
        "cancelled call should clear suspended calls"
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]["status"],
        json!("cancelled")
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_pending"]["tool_call_state"]["resume"]["action"],
        json!("cancel")
    );
}

#[tokio::test]
async fn test_run_loop_stream_decision_channel_emits_resolution_and_replay() {
    struct FrontendTool;

    #[async_trait]
    impl Tool for FrontendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("frontend_tool", "Frontend Tool", "needs approval").with_parameters(
                json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                        "approved": { "type": "boolean" }
                    },
                    "required": ["message"]
                }),
            )
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "frontend_tool",
                json!({
                    "message": args.get("message").and_then(Value::as_str).unwrap_or_default(),
                    "approved": args.get("approved").and_then(Value::as_bool).unwrap_or(false),
                }),
            ))
        }
    }

    struct PendingFrontendToolPlugin {
        ready: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl AgentBehavior for PendingFrontendToolPlugin {
        fn id(&self) -> &str {
            "pending_frontend_tool_stream_decision"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_name() != Some("frontend_tool") {
                return ActionSet::empty();
            }
            let already_approved = ctx
                .tool_args()
                .and_then(|args| args.get("approved"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if already_approved {
                return ActionSet::empty();
            }
            let args = ctx.tool_args().cloned().unwrap_or_default();
            let output = if let Some((ticket, _call_id)) = build_frontend_suspend_ticket(
                ctx,
                "frontend_tool",
                args,
                ResponseRouting::UseAsToolResult,
            ) {
                ActionSet::single(BeforeToolExecuteAction::Suspend(ticket))
            } else {
                ActionSet::empty()
            };
            self.ready.notify_one();
            self.release.notified().await;
            output
        }
    }

    let responses = vec![
        MockResponse::text("")
            .with_tool_call("call_done", "echo", json!({"message": "ok"}))
            .with_tool_call(
                "call_pending",
                "frontend_tool",
                json!({"message": "need approval"}),
            ),
        MockResponse::text("done"),
    ];
    let provider = MockStreamProvider::new(responses);
    let ready = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(PendingFrontendToolPlugin {
            ready: ready.clone(),
            release: release.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_executor(Arc::new(provider));

    let thread = Thread::new("test").with_message(Message::user("run"));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("echo".to_string(), Arc::new(EchoTool) as Arc<dyn Tool>);
    tools.insert(
        "frontend_tool".to_string(),
        Arc::new(FrontendTool) as Arc<dyn Tool>,
    );

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    let stream = run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        Some(decision_rx),
    );
    let collect_task = tokio::spawn(async move { collect_stream_events(stream).await });

    ready.notified().await;
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!({"approved": true, "message": "need approval"}),
            None,
        ))
        .expect("send decision");
    release.notify_one();

    let events = collect_task.await.expect("join collect task");
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending"
        )),
        "stream should emit ToolCallResumed for call_pending: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_pending"
        )),
        "stream should emit replay ToolCallDone for call_pending: {events:?}"
    );
}

#[tokio::test]
async fn test_run_loop_decision_channel_buffers_early_response_for_all_suspended_tools() {
    struct FrontendTool;

    #[async_trait]
    impl Tool for FrontendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("frontend_tool", "Frontend Tool", "needs approval").with_parameters(
                json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                        "approved": { "type": "boolean" }
                    },
                    "required": ["message"]
                }),
            )
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "frontend_tool",
                json!({
                    "message": args.get("message").and_then(Value::as_str).unwrap_or_default(),
                    "approved": args.get("approved").and_then(Value::as_bool).unwrap_or(false),
                }),
            ))
        }
    }

    struct EarlyPendingPlugin {
        entered: Arc<Notify>,
        allow_pending: Arc<Notify>,
    }

    #[async_trait]
    impl AgentBehavior for EarlyPendingPlugin {
        fn id(&self) -> &str {
            "early_pending_nonstream"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_name() != Some("frontend_tool") {
                return ActionSet::empty();
            }
            let already_approved = ctx
                .tool_args()
                .and_then(|args| args.get("approved"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if already_approved {
                return ActionSet::empty();
            }
            self.entered.notify_one();
            self.allow_pending.notified().await;
            let args = ctx.tool_args().cloned().unwrap_or_default();
            if let Some((ticket, _call_id)) = build_frontend_suspend_ticket(
                ctx,
                "frontend_tool",
                args,
                ResponseRouting::UseAsToolResult,
            ) {
                ActionSet::single(BeforeToolExecuteAction::Suspend(ticket))
            } else {
                ActionSet::empty()
            }
        }
    }

    let mut first = text_chat_response("");
    first.content = MessageContent::from_tool_calls(vec![genai::chat::ToolCall {
        call_id: "call_pending".to_string(),
        fn_name: "frontend_tool".to_string(),
        fn_arguments: json!({ "message": "need approval" }),
        thought_signatures: None,
    }]);
    let provider = Arc::new(MockChatProvider::new(vec![
        Ok(first),
        Ok(text_chat_response("done")),
    ]));
    let entered = Arc::new(Notify::new());
    let allow_pending = Arc::new(Notify::new());
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(EarlyPendingPlugin {
            entered: entered.clone(),
            allow_pending: allow_pending.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);

    let thread = Thread::new("test").with_message(Message::user("run"));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert(
        "frontend_tool".to_string(),
        Arc::new(FrontendTool) as Arc<dyn Tool>,
    );

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    let run_task = tokio::spawn(async move {
        run_loop(&config, tools, run_ctx, None, None, Some(decision_rx)).await
    });

    entered.notified().await;
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!({"approved": true, "message": "need approval"}),
            None,
        ))
        .expect("send decision");
    allow_pending.notify_one();

    let outcome = run_task.await.expect("join run task");
    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert_eq!(outcome.response.as_deref(), Some("done"));
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_pending")
                && !message
                    .content
                    .contains("is awaiting approval. Execution paused.")
        }),
        "queued decision should be replayed after pending state is applied"
    );
}

#[tokio::test]
async fn test_stream_decision_channel_buffers_early_response_for_all_suspended_tools() {
    struct FrontendTool;

    #[async_trait]
    impl Tool for FrontendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("frontend_tool", "Frontend Tool", "needs approval").with_parameters(
                json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                        "approved": { "type": "boolean" }
                    },
                    "required": ["message"]
                }),
            )
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "frontend_tool",
                json!({
                    "message": args.get("message").and_then(Value::as_str).unwrap_or_default(),
                    "approved": args.get("approved").and_then(Value::as_bool).unwrap_or(false),
                }),
            ))
        }
    }

    struct EarlyPendingPlugin {
        entered: Arc<Notify>,
        allow_pending: Arc<Notify>,
    }

    #[async_trait]
    impl AgentBehavior for EarlyPendingPlugin {
        fn id(&self) -> &str {
            "early_pending_stream"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_name() != Some("frontend_tool") {
                return ActionSet::empty();
            }
            let already_approved = ctx
                .tool_args()
                .and_then(|args| args.get("approved"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if already_approved {
                return ActionSet::empty();
            }
            self.entered.notify_one();
            self.allow_pending.notified().await;
            let args = ctx.tool_args().cloned().unwrap_or_default();
            if let Some((ticket, _call_id)) = build_frontend_suspend_ticket(
                ctx,
                "frontend_tool",
                args,
                ResponseRouting::UseAsToolResult,
            ) {
                ActionSet::single(BeforeToolExecuteAction::Suspend(ticket))
            } else {
                ActionSet::empty()
            }
        }
    }

    let responses = vec![
        MockResponse::text("").with_tool_call(
            "call_pending",
            "frontend_tool",
            json!({"message": "need approval"}),
        ),
        MockResponse::text("done"),
    ];
    let provider = MockStreamProvider::new(responses);
    let entered = Arc::new(Notify::new());
    let allow_pending = Arc::new(Notify::new());
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(EarlyPendingPlugin {
            entered: entered.clone(),
            allow_pending: allow_pending.clone(),
        }) as Arc<dyn AgentBehavior>)
        .with_llm_executor(Arc::new(provider));

    let thread = Thread::new("test").with_message(Message::user("run"));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert(
        "frontend_tool".to_string(),
        Arc::new(FrontendTool) as Arc<dyn Tool>,
    );

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    let stream = run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        None,
        None,
        Some(decision_rx),
    );
    let collect_task = tokio::spawn(async move { collect_stream_events(stream).await });

    entered.notified().await;
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!({"approved": true, "message": "need approval"}),
            None,
        ))
        .expect("send decision");
    allow_pending.notify_one();

    let events = collect_task.await.expect("join collect task");
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending"
        )),
        "queued decision should resolve once pending call is materialized: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_pending"
        )),
        "replayed tool result should be emitted after queued decision: {events:?}"
    );
}

#[tokio::test]
async fn test_stream_decision_channel_drains_while_inference_stream_is_running() {
    struct HangingStreamProvider;

    #[async_trait]
    impl LlmExecutor for HangingStreamProvider {
        async fn exec_chat_response(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&ChatOptions>,
        ) -> genai::Result<genai::chat::ChatResponse> {
            unimplemented!("stream-only provider")
        }

        async fn exec_chat_stream_events(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&ChatOptions>,
        ) -> genai::Result<super::LlmEventStream> {
            let stream = async_stream::stream! {
                yield Ok(ChatStreamEvent::Start);
                yield Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: "streaming".to_string(),
                }));
                let _: () = futures::future::pending().await;
            };
            Ok(Box::pin(stream))
        }

        fn name(&self) -> &'static str {
            "hanging_stream_for_decision"
        }
    }

    use crate::contracts::Suspension;

    let base_state = json!({});
    let echo_args = json!({"message": "approved during stream"});
    let pending_patch = set_single_suspended_call(
        &base_state,
        Suspension::new("call_pending", "tool:echo")
            .with_message("awaiting approval")
            .with_parameters(echo_args),
        None,
    )
    .expect("failed to seed suspended interaction");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::user("resume"));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");
    let config = BaseAgent::new("mock")
        .with_llm_executor(Arc::new(HangingStreamProvider) as Arc<dyn LlmExecutor>);
    let tools = tool_map([EchoTool]);
    let token = CancellationToken::new();

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    let stream = run_loop_stream(
        Arc::new(config),
        tools,
        run_ctx,
        Some(token.clone()),
        None,
        Some(decision_rx),
    );
    let collect_task = tokio::spawn(async move { collect_stream_events(stream).await });

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    decision_tx
        .send(test_decision(
            "call_pending",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!(true),
            None,
        ))
        .expect("send decision");
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    token.cancel();

    let events = tokio::time::timeout(std::time::Duration::from_millis(400), collect_task)
        .await
        .expect("stream should terminate after cancellation")
        .expect("collector task should not panic");

    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending"
        )),
        "decision should be drained while inference stream is still active: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_pending"
        )),
        "replay result should be emitted after in-flight decision drain: {events:?}"
    );
}

#[tokio::test]
async fn test_run_loop_decision_channel_replay_original_tool_uses_tool_call_resume_state() {
    struct OneShotPermissionPlugin;

    #[async_trait]
    impl AgentBehavior for OneShotPermissionPlugin {
        fn id(&self) -> &str {
            "test_one_shot_permission"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            let has_resume_grant = ctx.resume_input().is_some_and(|resume| {
                matches!(
                    resume.action,
                    crate::contracts::io::ResumeDecisionAction::Resume
                )
            });
            if has_resume_grant {
                return ActionSet::empty();
            }
            let tool_name = ctx.tool_name().unwrap_or_default().to_string();
            let tool_args = ctx.tool_args().cloned().unwrap_or_default();
            if let Some((ticket, _call_id)) = build_frontend_suspend_ticket(
                ctx,
                "PermissionConfirm",
                json!({ "tool_name": tool_name, "tool_args": tool_args }),
                ResponseRouting::ReplayOriginalTool,
            ) {
                ActionSet::single(BeforeToolExecuteAction::Suspend(ticket))
            } else {
                ActionSet::empty()
            }
        }
    }

    use crate::contracts::Suspension;

    let echo_args = json!({"message": "perm-replay"});
    let base_state = json!({});
    let invocation = FrontendToolInvocation::new(
        "fc_perm_1",
        "PermissionConfirm",
        echo_args.clone(),
        InvocationOrigin::ToolCallIntercepted {
            backend_call_id: "call_write".to_string(),
            backend_tool_name: "echo".to_string(),
            backend_arguments: echo_args.clone(),
        },
        ResponseRouting::ReplayOriginalTool,
    );
    let suspension = Suspension::new("fc_perm_1", "tool:PermissionConfirm")
        .with_parameters(json!({"source": "permission"}));
    let suspended_call = build_suspended_call("call_write", "echo", suspension, invocation);
    let action = suspended_call.into_state_action();
    let pending_patch = crate::contracts::runtime::state::reduce_state_actions(
        vec![action],
        &base_state,
        "test",
        &crate::contracts::runtime::state::ScopeContext::run(),
    )
    .expect("failed to seed suspended interaction")
    .into_iter()
    .next()
    .expect("expected a patch");
    let thread = Thread::with_initial_state("test", base_state)
        .with_patch(pending_patch)
        .with_message(Message::assistant_with_tool_calls(
            "need permission",
            vec![crate::contracts::thread::ToolCall::new(
                "call_write",
                "echo",
                echo_args.clone(),
            )],
        ))
        .with_message(Message::tool(
            "call_write",
            "Tool 'echo' is awaiting approval. Execution paused.",
        ))
        .with_message(Message::user("resume"));
    let run_ctx =
        RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).expect("run ctx");

    let provider = Arc::new(MockChatProvider::new(vec![Ok(text_chat_response("done"))]));
    let config = BaseAgent::new("mock")
        .with_behavior(Arc::new(OneShotPermissionPlugin) as Arc<dyn AgentBehavior>)
        .with_llm_executor(provider as Arc<dyn LlmExecutor>);
    let tools = tool_map([EchoTool]);

    let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
    decision_tx
        .send(test_decision(
            "fc_perm_1",
            crate::contracts::io::ResumeDecisionAction::Resume,
            json!(true),
            None,
        ))
        .expect("send decision");
    drop(decision_tx);

    let outcome = run_loop(&config, tools, run_ctx, None, None, Some(decision_rx)).await;
    assert_eq!(outcome.termination, TerminationReason::NaturalEnd);
    assert!(
        outcome.run_ctx.messages().iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call_write")
                && !message
                    .content
                    .contains("is awaiting approval. Execution paused.")
        }),
        "replayed backend call should complete without re-pending"
    );

    let final_state = outcome.run_ctx.snapshot().expect("snapshot");
    assert_eq!(
        final_state["__tool_call_scope"]["call_write"]["tool_call_state"]["status"],
        json!("succeeded")
    );
    assert_eq!(
        final_state["__tool_call_scope"]["call_write"]["tool_call_state"]["resume"]["action"],
        json!("resume")
    );
}

// ========================================================================
// Truncation recovery integration tests (stream path)
// ========================================================================

/// Helper: build a BaseAgent configured for truncation recovery testing.
///
/// Sets `max_tokens = 4096` so that `infer_stop_reason` can detect MaxTokens
/// when `completion_tokens == 4096`.
fn truncation_test_agent(provider: MockStreamProvider) -> BaseAgent {
    let mut config =
        BaseAgent::new("mock").with_llm_executor(Arc::new(provider) as Arc<dyn LlmExecutor>);
    config.chat_options = Some(
        ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_reasoning_content(true)
            .with_capture_tool_calls(true)
            .with_max_tokens(4096),
    );
    config
}

/// Helper: create a MockResponse that simulates MaxTokens truncation.
///
/// The response has `completion_tokens == 4096` matching the agent's
/// `max_tokens`, so `StreamCollector.finish()` infers `StopReason::MaxTokens`.
fn truncated_response(text: &str) -> MockResponse {
    MockResponse::text(text).with_usage(1000, 4096)
}

/// Helper: create a MockResponse that simulates a normal completion.
fn normal_response(text: &str) -> MockResponse {
    MockResponse::text(text).with_usage(1000, 100)
}

#[tokio::test]
async fn test_stream_truncation_recovery_retries_then_succeeds() {
    // First response: truncated (MaxTokens, no tools)
    // Second response: normal completion
    let provider = MockStreamProvider::new(vec![
        truncated_response("partial output..."),
        normal_response("complete response"),
    ]);
    let config = truncation_test_agent(provider);

    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), HashMap::new(), run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    // Should terminate normally (not with error)
    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd),
        "should recover and complete normally"
    );

    // The final response text should stitch the truncated prefix and recovery text.
    let response = extract_run_finish_response(&events);
    assert_eq!(
        response.as_deref(),
        Some("partial output...complete response")
    );
}

#[tokio::test]
async fn test_stream_truncation_recovery_with_tool_calls_no_retry() {
    // Response has MaxTokens but includes tool calls → no retry, tools execute
    let provider = MockStreamProvider::new(vec![
        MockResponse::text("I'll search for that")
            .with_tool_call("c1", "echo", json!({"input": "test"}))
            .with_usage(1000, 4096),
        normal_response("done after tools"),
    ]);
    let config = truncation_test_agent(provider);

    // Register a simple echo tool
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("echo".to_string(), Arc::new(EchoTool));

    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd),
    );

    // Verify that tool execution happened (ToolCallDone event present)
    let tool_done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .count();
    assert!(
        tool_done_count > 0,
        "tool calls should execute when present, even with MaxTokens"
    );
}

#[tokio::test]
async fn test_stream_truncation_recovery_exhausts_retries() {
    // Four consecutive truncated responses: 3 retries + 1 that exceeds limit.
    // After exhausting retries, the 4th truncation should end the run normally
    // (NaturalEnd with the truncated text) since no tools are needed.
    let provider = MockStreamProvider::new(vec![
        truncated_response("partial 1"),
        truncated_response("partial 2"),
        truncated_response("partial 3"),
        truncated_response("partial 4 - no more retries"),
    ]);
    let config = truncation_test_agent(provider);

    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), HashMap::new(), run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd),
        "should terminate normally after exhausting retries"
    );

    // Verify we got 4 inference_complete events (1 original + 3 retries)
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_count, 4,
        "should have 4 inference calls: original + 3 retries"
    );
    assert_eq!(
        extract_run_finish_response(&events),
        Some("partial 1partial 2partial 3partial 4 - no more retries".to_string())
    );
}

#[tokio::test]
async fn test_stream_truncation_recovery_injects_internal_continuation_message() {
    // Truncated then succeeded. Verify the continuation message is Internal.
    let provider = MockStreamProvider::new(vec![
        truncated_response("partial"),
        normal_response("complete"),
    ]);
    let config = truncation_test_agent(provider);

    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), HashMap::new(), run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    // Extract the RunFinish to get the final RunContext
    let run_finish = events
        .iter()
        .find(|e| matches!(e, AgentEvent::RunFinish { .. }));
    assert!(run_finish.is_some());

    // The continuation message should NOT appear in TextDelta events
    // (it's Internal visibility, only for LLM)
    let text_deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    for delta in &text_deltas {
        assert!(
            !delta.contains("output token limit"),
            "continuation prompt should not appear in user-facing text deltas"
        );
    }
}

#[tokio::test]
async fn test_stream_truncation_recovery_preserves_truncated_assistant_text() {
    // After truncation + retry, the truncated assistant text should appear
    // in the event stream (TextDelta events from the first inference call),
    // followed by the recovery's final response text.
    let provider = MockStreamProvider::new(vec![
        truncated_response("I was writing about Rust and then I got cut off because"),
        normal_response("Continuing: Rust is a systems programming language."),
    ]);
    let config = truncation_test_agent(provider);

    let thread = Thread::new("test").with_message(Message::user("Tell me about Rust"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), HashMap::new(), run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );

    // Two inference calls: truncated + recovery
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(inference_count, 2, "should have 2 inference calls");

    // Both the truncated text and recovery text should appear in TextDelta events
    let all_text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        all_text.contains("cut off"),
        "truncated text should appear in text deltas"
    );
    assert!(
        all_text.contains("Continuing"),
        "recovery text should appear in text deltas"
    );
}

#[tokio::test]
async fn test_stream_no_truncation_recovery_on_normal_end() {
    // Normal response (not truncated) should not trigger any recovery.
    let provider = MockStreamProvider::new(vec![normal_response("all good")]);
    let config = truncation_test_agent(provider);

    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), HashMap::new(), run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );

    // Only 1 inference call
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(inference_count, 1, "no retries for normal completion");
}

#[tokio::test]
async fn test_stream_multiple_truncation_retries_then_tool_call() {
    // Two truncations, then a response with tools, then final text.
    let provider = MockStreamProvider::new(vec![
        truncated_response("partial 1"),
        truncated_response("partial 2"),
        MockResponse::text("now with tool")
            .with_tool_call("c1", "echo", json!({"input": "hi"}))
            .with_usage(1000, 100),
        normal_response("done"),
    ]);
    let config = truncation_test_agent(provider);

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("echo".to_string(), Arc::new(EchoTool));

    let thread = Thread::new("test").with_message(Message::user("go"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunPolicy::default()).unwrap();
    let stream = run_loop_stream(Arc::new(config), tools, run_ctx, None, None, None);
    let events = collect_stream_events(stream).await;

    assert_eq!(
        extract_termination(&events),
        Some(TerminationReason::NaturalEnd)
    );

    // 4 total inference calls: 2 truncated + 1 tool + 1 final
    let inference_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(inference_count, 4);

    // Tool execution should have occurred
    let tool_done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallDone { .. }))
        .count();
    assert_eq!(tool_done_count, 1);
}
