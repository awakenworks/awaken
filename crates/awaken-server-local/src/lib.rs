//! `awaken-server-local` — the single-machine assembly.
//!
//! It owns one protocol-neutral [`SharedHost`] (the thread-keyed session
//! substrate) and mounts public protocol adapters over it. Each adapter is a thin
//! port implementation that translates its own wire vocabulary to the host's
//! neutral operations; because every adapter keys by the same thread id and drives
//! the same coordinator, a turn started through one protocol can be resumed or
//! observed through another on the *same thread*.
//!
//! Per-thread composition keeps the kernel sandbox-agnostic (ADR-0034 D6);
//! distribution stays out — remote relays and multi-node ingress plug in through
//! seams, not here.

mod host;
mod hub;
mod store;

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_protocol_a2a::port::{
    A2aRuntime, DriverError as A2aErr, Pending as A2aPending, Resume as A2aResume,
    StepOutcome as A2aStep,
};
use awaken_protocol_ag_ui::port::{
    AgUiRuntime, DriverError as AgErr, Pending as AgPending, Resume as AgResume,
    StepOutcome as AgStep,
};
use awaken_protocol_ai_sdk::port::{
    AiSdkRuntime, DriverError, Pending as AiPending, Resume as AiResume, StepOutcome,
};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeIteration, OutcomeReport, Pending, RunError, SessionRuntime,
    TurnOutcome, router,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use axum::Router;

use crate::host::{HostError, HostErrorKind, PendingTool, TurnResult, block_text};

pub use crate::host::{HostResume, SharedHost};
pub use crate::hub::{ThreadEvent, ThreadEventHub};

/// A deterministic, network-free model: it replies with the last user turn's
/// text, so the server runs end-to-end in CI and under the TypeScript SDK e2e
/// without an API key. Swap in a provider executor for real capability.
pub struct EchoModel;

#[async_trait::async_trait]
impl LlmExecutor for EchoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: None,
        })
    }
}

/// A deterministic probe model for the HITL e2e: it writes the user's text to a
/// relative `probe.txt` (asked -> parks for confirmation), reads it back (allowed
/// -> runs), then replies. Stateless: it decides from the transcript.
pub struct ProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for ProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "probe.txt", "content": user_text }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "r".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": "probe.txt" }),
            }]),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

/// A deterministic model for the outcome e2e: replies with a draft, and revises to
/// include "FINAL" once it sees the goal loop's feedback. Stateless.
pub struct ReviseModel;

#[async_trait::async_trait]
impl LlmExecutor for ReviseModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let reply = if last_user.contains("did not meet the goal") {
            "FINAL answer"
        } else {
            "a rough draft"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
        })
    }
}

/// A deterministic model for the custom-tool e2e: it calls the client-executed
/// tool `submit_answer`, then replies with the result the client returned.
pub struct CustomToolModel;

#[async_trait::async_trait]
impl LlmExecutor for CustomToolModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let output = if tool_results == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".into(),
                tool_id: "submit_answer".into(),
                arguments: serde_json::json!({ "question": "what is 6 x 7?" }),
            }])
        } else {
            let result = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ChatRole::Tool)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            AssistantOutput::text(format!("got: {result}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

/// A deterministic model for the delegation e2e. When it holds `agent_run` it
/// delegates (to `researcher`, or to `ghost` if the user asks for it) and then
/// reports the delegate's result; without `agent_run` it answers plainly, so the
/// same model serves as the delegate sub-agent.
pub struct DelegatingModel;

#[async_trait::async_trait]
impl LlmExecutor for DelegatingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let has_delegation = request.tools.iter().any(|t| t.id == "agent_run");
        if !has_delegation {
            return Ok(ChatResponse {
                output: AssistantOutput::text("researched: 42"),
                usage: None,
            });
        }
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let output = if tool_results == 0 {
            let user = request
                .messages
                .iter()
                .find(|m| m.role == ChatRole::User)
                .map(|m| block_text(&m.content))
                .unwrap_or_default();
            let agent_id = if user.contains("ghost") {
                "ghost"
            } else {
                "researcher"
            };
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "d1".into(),
                tool_id: "agent_run".into(),
                arguments: serde_json::json!({ "agent_id": agent_id, "input": "do the research" }),
            }])
        } else {
            let result = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ChatRole::Tool)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            AssistantOutput::text(format!("delegate said: {result}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

// ── Managed Agents adapter over the shared host ─────────────────────────────

/// Mint a fresh user message from plain text (Managed `user.message` content is
/// concatenated to text before it enters the host).
fn user_message(text: &str) -> Message {
    Message::text(
        MessageId(format!(
            "usr-{}",
            crate::host::BASE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )),
        Role::User,
        text,
    )
}

fn to_run_error(err: HostError) -> RunError {
    match err.kind {
        HostErrorKind::BadRequest => RunError::bad_request(err.message),
        HostErrorKind::Internal => RunError::internal(err.message),
    }
}

/// Map a neutral terminal phase to the Managed idle `stop_reason`. `RequiresAction`
/// carries no event ids here; the projection refills them from the pending tool.
fn phase_to_stop(phase: &Phase) -> StopReason {
    match phase {
        Phase::Waiting => StopReason::RequiresAction {
            event_ids: Vec::new(),
        },
        Phase::Ended(EndCause::MaxSteps) => StopReason::RetriesExhausted,
        _ => StopReason::EndTurn,
    }
}

fn to_pending(pending: Option<PendingTool>) -> Option<Pending> {
    pending.map(|p| Pending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_turn_outcome(result: TurnResult) -> TurnOutcome {
    TurnOutcome {
        stop: phase_to_stop(&result.phase),
        messages: result.new_messages,
        pending: to_pending(result.pending),
    }
}

/// The Managed Agents `SessionRuntime` port implemented over the shared host.
/// Holds only an `Arc<SharedHost>`, so it composes with any other adapter bound
/// to the same host.
pub struct ManagedHost {
    host: Arc<SharedHost>,
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
    async fn run_turn(
        &self,
        _agent: &str,
        thread: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, RunError> {
        let result = self
            .host
            .run_turn(thread, vec![user_message(user_text)])
            .await
            .map_err(to_run_error)?;
        Ok(to_turn_outcome(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: Decision,
    ) -> Result<TurnOutcome, RunError> {
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::Confirm {
                    allow: decision.allow,
                    note: decision.note,
                },
            )
            .await
            .map_err(to_run_error)?;
        Ok(to_turn_outcome(result))
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<TurnOutcome, RunError> {
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::ClientResult {
                    content: content.to_string(),
                    is_error,
                },
            )
            .await
            .map_err(to_run_error)?;
        Ok(to_turn_outcome(result))
    }

    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError> {
        self.host
            .add_system(thread, text)
            .await
            .map_err(to_run_error)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.host.interrupt(thread).await.map_err(to_run_error)
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        let report = self
            .host
            .define_outcome(thread, description, rubric, max_iterations)
            .await
            .map_err(to_run_error)?;
        Ok(OutcomeReport {
            iterations: report
                .iterations
                .into_iter()
                .map(|it| OutcomeIteration {
                    messages: it.messages,
                    outcome_id: it.outcome_id,
                    iteration: it.iteration,
                    result: it.result,
                    explanation: it.explanation,
                })
                .collect(),
        })
    }

    fn model(&self) -> String {
        self.host.model()
    }
}

// ── AI SDK adapter over the shared host ─────────────────────────────────────

fn to_driver_error(err: HostError) -> DriverError {
    match err.kind {
        HostErrorKind::BadRequest => DriverError::BadRequest(err.message),
        HostErrorKind::Internal => DriverError::Internal(err.message),
    }
}

fn to_ai_pending(pending: Option<PendingTool>) -> Option<AiPending> {
    pending.map(|p| AiPending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_step_outcome(result: TurnResult) -> StepOutcome {
    StepOutcome {
        waiting: matches!(result.phase, Phase::Waiting),
        exhausted: matches!(result.phase, Phase::Ended(EndCause::MaxSteps)),
        new_messages: result.new_messages,
        pending: to_ai_pending(result.pending),
    }
}

/// The AI SDK `AiSdkRuntime` port implemented over the shared host — the twin of
/// [`ManagedHost`]. Both hold the same `Arc<SharedHost>`, so a turn started by one
/// protocol is resumable and observable through the other on the same thread.
pub struct AiSdkHost {
    host: Arc<SharedHost>,
}

impl AiSdkHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl AiSdkRuntime for AiSdkHost {
    async fn run_turn(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        let result = self
            .host
            .run_turn(thread, messages)
            .await
            .map_err(to_driver_error)?;
        Ok(to_step_outcome(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: AiResume,
    ) -> Result<StepOutcome, DriverError> {
        let resume = match resume {
            AiResume::Confirm { allow, note } => HostResume::Confirm { allow, note },
            AiResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_driver_error)?;
        Ok(to_step_outcome(result))
    }

    async fn pending(&self, thread: &str) -> Option<AiPending> {
        to_ai_pending(self.host.pending_tool(thread).await)
    }

    async fn history(&self, thread: &str) -> Vec<Message> {
        self.host.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.host.model()
    }
}

// ── A2A adapter over the shared host ────────────────────────────────────────

fn to_a2a_error(err: HostError) -> A2aErr {
    match err.kind {
        HostErrorKind::BadRequest => A2aErr::BadRequest(err.message),
        HostErrorKind::Internal => A2aErr::Internal(err.message),
    }
}

fn to_a2a_pending(pending: Option<PendingTool>) -> Option<A2aPending> {
    pending.map(|p| A2aPending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_a2a_step(result: TurnResult) -> A2aStep {
    A2aStep {
        waiting: matches!(result.phase, Phase::Waiting),
        exhausted: matches!(result.phase, Phase::Ended(EndCause::MaxSteps)),
        new_messages: result.new_messages,
        pending: to_a2a_pending(result.pending),
    }
}

/// The A2A `A2aRuntime` port implemented over the shared host — a fourth twin of
/// [`ManagedHost`] / [`AiSdkHost`] / [`AgUiHost`] over the same `Arc<SharedHost>`.
pub struct A2aHost {
    host: Arc<SharedHost>,
}

impl A2aHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl A2aRuntime for A2aHost {
    async fn run_turn(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<A2aStep, A2aErr> {
        let result = self
            .host
            .run_turn(thread, messages)
            .await
            .map_err(to_a2a_error)?;
        Ok(to_a2a_step(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: A2aResume,
    ) -> Result<A2aStep, A2aErr> {
        let resume = match resume {
            A2aResume::Confirm { allow, note } => HostResume::Confirm { allow, note },
            A2aResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_a2a_error)?;
        Ok(to_a2a_step(result))
    }

    async fn pending(&self, thread: &str) -> Option<A2aPending> {
        to_a2a_pending(self.host.pending_tool(thread).await)
    }

    async fn history(&self, thread: &str) -> Vec<Message> {
        self.host.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.host.model()
    }
}

// ── AG-UI adapter over the shared host ──────────────────────────────────────

fn to_ag_error(err: HostError) -> AgErr {
    match err.kind {
        HostErrorKind::BadRequest => AgErr::BadRequest(err.message),
        HostErrorKind::Internal => AgErr::Internal(err.message),
    }
}

fn to_ag_pending(pending: Option<PendingTool>) -> Option<AgPending> {
    pending.map(|p| AgPending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_ag_step(result: TurnResult) -> AgStep {
    AgStep {
        waiting: matches!(result.phase, Phase::Waiting),
        exhausted: matches!(result.phase, Phase::Ended(EndCause::MaxSteps)),
        new_messages: result.new_messages,
        pending: to_ag_pending(result.pending),
    }
}

/// The AG-UI `AgUiRuntime` port implemented over the shared host — a third twin of
/// [`ManagedHost`] / [`AiSdkHost`] over the same `Arc<SharedHost>`.
pub struct AgUiHost {
    host: Arc<SharedHost>,
}

impl AgUiHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl AgUiRuntime for AgUiHost {
    async fn run_turn(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<AgStep, AgErr> {
        let result = self
            .host
            .run_turn(thread, messages)
            .await
            .map_err(to_ag_error)?;
        Ok(to_ag_step(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: AgResume,
    ) -> Result<AgStep, AgErr> {
        let resume = match resume {
            AgResume::Confirm { allow, note } => HostResume::Confirm { allow, note },
            AgResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_ag_error)?;
        Ok(to_ag_step(result))
    }

    async fn pending(&self, thread: &str) -> Option<AgPending> {
        to_ag_pending(self.host.pending_tool(thread).await)
    }

    async fn history(&self, thread: &str) -> Vec<Message> {
        self.host.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.host.model()
    }
}

// ── Router assembly ─────────────────────────────────────────────────────────

/// Mount every public protocol adapter over one shared host. Managed Agents, AI
/// SDK, and AG-UI routes have disjoint path prefixes (`/v1/sessions...`,
/// `/v1/ai-sdk...`, `/v1/ag-ui...`) and drive the same `host`, so all three
/// protocols operate on the same threads.
fn mount(host: Arc<SharedHost>) -> Router {
    let managed = router(Arc::new(ManagedState::new(ManagedHost::new(host.clone()))));
    let ai_sdk = awaken_protocol_ai_sdk::router(Arc::new(AiSdkHost::new(host.clone())));
    let ag_ui = awaken_protocol_ag_ui::router(Arc::new(AgUiHost::new(host.clone())));
    let a2a = awaken_protocol_a2a::router(Arc::new(A2aHost::new(host.clone())));
    managed.merge(ai_sdk).merge(ag_ui).merge(a2a)
}

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    mount(Arc::new(SharedHost::new(llm, model_ref)))
}

/// A router whose outcomes are graded by a judge sub-agent (`judge_agent_id`) run
/// through the kernel, rather than the deterministic keyword grader.
pub fn build_graded_router(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    judge_agent_id: impl Into<String>,
) -> Router {
    mount(Arc::new(SharedHost::with_judge(
        llm,
        model_ref,
        judge_agent_id,
    )))
}

/// The default deterministic router (echo model) — the CI / e2e server.
pub fn build_echo_router() -> Router {
    build_router(Arc::new(EchoModel), "echo-model")
}

/// A router with a client-executed tool `submit_answer` (the custom-tool e2e).
pub fn build_custom_router() -> Router {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let host = SharedHost::with_client_tools(Arc::new(CustomToolModel), "custom", client_tools);
    mount(Arc::new(host))
}

/// A router whose agent can delegate to a `researcher` sub-agent via `agent_run`
/// (the multi-agent e2e). `ghost` is deliberately absent from the roster so the
/// fail-closed path can be exercised.
pub fn build_delegation_router() -> Router {
    let roster = HashSet::from(["researcher".to_string()]);
    let host = SharedHost::with_delegates(Arc::new(DelegatingModel), "delegate", roster);
    mount(Arc::new(host))
}
