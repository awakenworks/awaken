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

mod agent_catalog;
mod background;
mod compact;
mod config;
mod config_plane;
mod delegate;
mod durable_ops;
mod host;
mod hub;
mod judge;
mod memory;
mod skills;
mod store;
mod subagent;

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::{ContentBlock, ImageSource};
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
    AgentCapabilities, BuiltinTool, CustomTool, Decision, ManagedState, OutcomeIteration,
    OutcomeReport, Pending, RunError, SessionRuntime, TurnOutcome, router,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use axum::Router;

use crate::config::block_text;
use crate::host::{HostError, HostErrorKind, PendingTool, TurnResult};

pub use crate::host::{HostResume, SharedHost};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
// Skill authoring inputs (ADR-0036): a composition root supplies these to
// `build_router_with_skills` / `SharedHost::with_skills`. The whole set is fronted
// by the single `Skill` tool.
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_sandbox_local::content_fingerprint;
// A remote delegate's transport belongs to the A2A bounded context; re-export it so
// a composition-root caller configures a remote agent from one import.
pub use awaken_protocol_a2a::{HttpTransport, Response, Transport};

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

/// A deterministic vision-probe model: it reports the media it received on the
/// last user turn, so an e2e can assert an image survived the whole
/// adapter -> runtime -> model path (the echo model only sees text). Replies e.g.
/// `saw image/png; text: what color`.
pub struct VisionProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for VisionProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User);
        let mut medias = Vec::new();
        let mut text = String::new();
        if let Some(message) = last_user {
            for block in &message.content {
                match block {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    ContentBlock::Image { source } => medias.push(match source {
                        ImageSource::Base64 { media_type, .. } => media_type.clone(),
                        ImageSource::Url { .. } => "image/url".to_string(),
                    }),
                    _ => {}
                }
            }
        }
        let reply = if medias.is_empty() {
            format!("saw no media; text: {text}")
        } else {
            format!("saw {}; text: {text}", medias.join(","))
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
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

/// A deterministic model for the state-machine e2e. It calls `glob` (an
/// auto-allowed perception tool that succeeds against an empty sandbox) twice: the
/// first walks the machine `s0 -> s1` (firing its emit); the second is out of order
/// (`glob` is only defined from `s0`), so the machine gate rejects it as a
/// violation. Then it ends. This exercises the tool state machine's gate, advance,
/// emit, and violation paths end to end. Stateless.
pub struct StateMachineModel;

#[async_trait::async_trait]
impl LlmExecutor for StateMachineModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let steps = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let glob = |call_id: &str| {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: call_id.into(),
                tool_id: "glob".into(),
                arguments: serde_json::json!({ "pattern": "*.txt" }),
            }])
        };
        let output = match steps {
            0 => glob("g1"),
            1 => glob("g2"),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

/// A deterministic model that echoes the system prompt (an agent's instructions),
/// so a config e2e can assert a *published* agent's own instructions reached the
/// run. Reads the last system message. Stateless.
pub struct InstructionEchoModel;

#[async_trait::async_trait]
impl LlmExecutor for InstructionEchoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::System)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("instructions: {system}")),
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
fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId(format!(
            "usr-{}",
            crate::host::BASE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )),
        Role::User,
        content,
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
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        let result = self
            .host
            .run_turn(Some(agent), thread, vec![user_message(content)])
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

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its parked run (ADR-0039).
    async fn committed_messages(&self, thread: &str) -> Vec<awaken_agent_contract::Message> {
        self.host.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.host.model()
    }

    /// Advertise the host's provisioned surface on the created session: its built-in
    /// tools (folded into the agent toolset by the adapter), client tools, offered
    /// skills, and delegate roster. (MCP servers and file resources are not advertised
    /// — the local host wires no MCP capability and has no Files-API resource yet.)
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            builtin_tools: self
                .host
                .builtin_tools()
                .into_iter()
                .map(|(name, ask)| BuiltinTool { name, ask })
                .collect(),
            custom_tools: self
                .host
                .custom_tools()
                .into_iter()
                .map(|d| CustomTool {
                    name: d.id,
                    description: d.description,
                    input_schema: d.parameters,
                })
                .collect(),
            skills: self.host.skill_ids(),
            delegates: self.host.delegate_ids(),
        }
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
            .run_turn(None, thread, messages)
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
            .run_turn(None, thread, messages)
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
            .run_turn(None, thread, messages)
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
    // The durable-ingress operations surface (slice E): ADR-0009 follow-on verbs
    // (supersede / reconcile / reap / dead-letter GC) over the same shared host.
    let durable_ops = crate::durable_ops::durable_ops_router(host.clone());
    managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
}

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    mount(Arc::new(SharedHost::new(llm, model_ref)))
}

/// Build the server router offering `skills` on every thread (ADR-0036): the whole
/// set is fronted by the single `Skill` tool, whose catalog lists them and whose
/// invocation returns the activated skill's instructions.
pub fn build_router_with_skills(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    skills: Vec<SkillSpec>,
) -> Router {
    mount(Arc::new(
        SharedHost::new(llm, model_ref).with_skills(skills),
    ))
}

/// A router whose outcomes are graded by a judge sub-agent (`judge_agent_id`) run
/// through the kernel, rather than the deterministic keyword grader.
pub fn build_graded_router(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    judge_agent_id: impl Into<String>,
) -> Router {
    mount(Arc::new(
        SharedHost::new(llm, model_ref).with_judge(judge_agent_id),
    ))
}

/// The default deterministic router (echo model) — the CI / e2e server.
pub fn build_echo_router() -> Router {
    build_router(Arc::new(EchoModel), "echo-model")
}

/// A router whose model reports the media it received (the multimodal e2e): every
/// protocol adapter must carry an image block through to the model for the probe
/// reply to name its media type.
pub fn build_vision_router() -> Router {
    build_router(Arc::new(VisionProbeModel), "vision-probe")
}

/// A router with a client-executed tool `submit_answer` (the custom-tool e2e).
pub fn build_custom_router() -> Router {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let host = SharedHost::new(Arc::new(CustomToolModel), "custom").with_client_tools(client_tools);
    mount(Arc::new(host))
}

/// A router whose agent can delegate to a `researcher` sub-agent via `agent_run`
/// (the multi-agent e2e). `ghost` is deliberately absent from the roster so the
/// fail-closed path can be exercised.
pub fn build_delegation_router() -> Router {
    let roster = HashSet::from(["researcher".to_string()]);
    let host = SharedHost::new(Arc::new(DelegatingModel), "delegate").with_delegates(roster);
    mount(Arc::new(host))
}

/// A router whose agent activates the tool state machine (the state-machine e2e).
/// The machine defines `glob` as a single transition out of the initial state, so
/// the driving model's first `glob` advances it (emitting a context message) and
/// the second is a precondition violation the gate denies.
pub fn build_statemachine_router() -> Router {
    let machine = serde_json::json!({
        "machines": [{
            "name": "walk",
            "initial": "s0",
            "terminal": ["s1"],
            "transitions": [{
                "on": "glob(pattern ~ \"*\")",
                "from": ["s0"],
                "to": "s1",
                "emit": { "target": "system", "content": "advanced to s1", "cooldown_turns": 0 },
                "on_violation": { "action": "deny", "reason": "glob is only allowed from the start state" }
            }]
        }]
    });
    let host =
        SharedHost::new(Arc::new(StateMachineModel), "statemachine").with_state_machine(machine);
    mount(Arc::new(host))
}

/// A router with the config data plane (`/v1/config/agents/*`) over an in-memory
/// SQLite config store, plus the protocol adapters. A session for a *published*
/// agent runs with that agent's installed config (slice A); the model echoes the
/// agent's instructions so an e2e can assert the published config took effect.
pub fn build_config_router() -> Router {
    let registry = Arc::new(
        awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
    );
    let tools = config::advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
    let service = Arc::new(config_plane::ConfigService::new(registry, tools));
    let host = SharedHost::new(Arc::new(InstructionEchoModel), "config")
        .with_config_service(service.clone());
    mount(Arc::new(host)).merge(config_plane::config_router(service))
}

/// A tool gate that defers every tool call as a committed `ScheduledAction`
/// (ADR-0020, slice E): instead of running inline or parking for a human, the call
/// is scheduled, keyed by its call id, and the durable dispatch worker performs it
/// out of band. In direct mode a scheduled run would park; under
/// `AWAKEN_INGRESS=durable` the worker's scheduled-action loop performs it and the
/// run completes autonomously.
struct ScheduleGate;

#[async_trait::async_trait]
impl awaken_runtime_contract::permission::ToolGateHook for ScheduleGate {
    async fn gate(
        &self,
        ctx: &awaken_runtime_contract::permission::PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> awaken_runtime_contract::permission::GateOutcome {
        awaken_runtime_contract::permission::GateOutcome::Schedule {
            correlation_id: format!("sched-{}", ctx.call_id),
            action_kind: None,
        }
    }
}

/// A router whose tool gate defers every tool call as a `ScheduledAction`
/// (ADR-0020, slice E). Drive it with `AWAKEN_INGRESS=durable` so the dispatch
/// worker performs the deferred actions out of band: the probe model's
/// write→read tool calls are each scheduled and auto-performed, so the run
/// completes without any human confirmation.
pub fn build_schedule_router() -> Router {
    let host = SharedHost::new(Arc::new(ProbeModel), "schedule")
        .with_gate_override(Arc::new(ScheduleGate));
    mount(Arc::new(host))
}
