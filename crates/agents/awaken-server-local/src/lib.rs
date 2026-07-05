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
mod authz;
mod background;
mod compact;
mod config;
mod config_plane;
mod delegate;
mod durable_ops;
mod host;
mod hub;
mod judge;
mod mcp;
mod memory;
mod model_route;
mod skills;
mod store;
mod subagent;

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::{ContentBlock, ImageSource};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_config_resolver::ResolvedInference;
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, ManagedState, OutcomeIteration,
    OutcomeReport, Pending, RunError, SessionRuntime, TurnOutcome, router,
};
use awaken_protocol_transport::{
    DriverError, Pending as PortPending, ProtocolRuntime, Resume as PortResume, StepOutcome,
};
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use axum::Router;

use crate::config::block_text;
use crate::host::{HostError, HostErrorKind, PendingTool, TurnResult};

// Embedded management-plane IAM (ADR-0042/0043 P1): the authorizer, its boot
// fn, the mint spec (tests / operator embeddings), and the bootstrap constants.
pub use crate::authz::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz, TokenSpec,
    embedded_iam,
};
pub use crate::host::{HostResume, SharedHost};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe — exposed so an
// integration test (and another composition root) can drive them directly.
pub use crate::mcp::{ExtMcpProbe, PreparedMcpRefresh, VaultRefresher};
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
            stop_reason: None,
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
            stop_reason: None,
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
            stop_reason: None,
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
            stop_reason: None,
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
            stop_reason: None,
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
            stop_reason: None,
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
            stop_reason: None,
        })
    }
}

/// A deterministic model for the MCP e2e (ADR-0043 Phase 3), following the
/// `CustomToolModel` idiom. `add <a> <b>` emits a call to the namespaced MCP tool
/// `mcp__calc__add` — the fixture contract: the session registers the mock MCP
/// server under the name `calc` and the server offers a tool `add`, which
/// `awaken_ext_mcp::to_tool_id("calc", "add")` maps to exactly this id (the e2e
/// fixture must mirror that naming). A tool result is echoed as
/// `result: <text>`; anything else echoes like `EchoModel`, so the existing
/// echo-based expectations on the management server keep holding. Stateless.
pub struct McpToolModel;

#[async_trait::async_trait]
impl LlmExecutor for McpToolModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        // A tool result came back: report it (`result: <text>`), ending the turn.
        if let Some(last) = request.messages.last()
            && last.role == ChatRole::Tool
        {
            let result: String = last
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                    _ => None,
                })
                .collect();
            return Ok(ChatResponse {
                output: AssistantOutput::text(format!("result: {result}")),
                usage: None,
                stop_reason: None,
            });
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        // `add <a> <b>` (two integers) → call the MCP calculator.
        let parts: Vec<&str> = last_user.split_whitespace().collect();
        if let ["add", a, b] = parts.as_slice()
            && let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>())
        {
            return Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    // Unique per step so multi-turn tool-use events keep distinct ids.
                    call_id: format!("mcp-{}", request.messages.len()),
                    tool_id: "mcp__calc__add".into(),
                    arguments: serde_json::json!({ "a": a, "b": b }),
                }]),
                usage: None,
                stop_reason: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: None,
            stop_reason: None,
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
                stop_reason: None,
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
            stop_reason: None,
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
/// Holds only an `Arc<SharedHost>` (plus, on the management server, the MCP
/// stores `prepare_session` reads), so it composes with any other adapter bound
/// to the same host.
pub struct ManagedHost {
    host: Arc<SharedHost>,
    mcp: Option<ManagedMcp>,
}

/// The session-ingress MCP wiring (ADR-0043 Phase 3): the stores
/// `prepare_session` reads to materialize a binding's vault credential and the
/// management plane's agent↔MCP config. Present only via [`ManagedHost::with_mcp`].
struct ManagedMcp {
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    mcp_store: Arc<dyn awaken_admin_config_api::McpStore>,
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host, mcp: None }
    }

    /// Wire the MCP stores so `prepare_session` materializes a session's MCP
    /// credential bindings and merges the management plane's agent↔MCP config
    /// (ADR-0043 Phase 3). Hosts built without this keep the trait's no-op
    /// `prepare_session`, so no other server mode changes behavior.
    #[must_use]
    pub fn with_mcp(
        mut self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
        mcp_store: Arc<dyn awaken_admin_config_api::McpStore>,
    ) -> Self {
        self.mcp = Some(ManagedMcp {
            credentials,
            secrets,
            mcp_store,
        });
        self
    }
}

/// A resolver credential lookup prefetched from the repo, scoped to exactly the
/// sources and pools the given MCP server defs' bindings name. The admin resolve
/// route builds its lookup by listing a workspace; the managed session ingress
/// has no workspace parameter on the wire, so it prefetches per binding instead —
/// same lookup shape, same fail-closed outcome (a missing source stays absent and
/// `resolve_mcp_servers` errors on it).
#[derive(Default)]
struct PrefetchedSourceLookup {
    sources: std::collections::HashMap<String, awaken_credential_vault::CredentialSource>,
    pools: std::collections::HashMap<String, awaken_credential_vault::CredentialPool>,
}

impl awaken_config_resolver::SourceLookup for PrefetchedSourceLookup {
    fn get(&self, id: &str) -> Option<&awaken_credential_vault::CredentialSource> {
        self.sources.get(id)
    }
    fn get_pool(&self, id: &str) -> Option<&awaken_credential_vault::CredentialPool> {
        self.pools.get(id)
    }
}

impl PrefetchedSourceLookup {
    /// Fetch every source/pool the defs' bindings reference. A row the repo does
    /// not hold is simply not inserted; resolution then fails closed on it.
    async fn for_defs(
        defs: &[awaken_config_resolver::McpServerDef],
        repo: &dyn awaken_credential_vault::repo::CredentialRepo,
    ) -> Self {
        use awaken_credential_vault::CredentialBinding;
        let mut lookup = Self::default();
        for def in defs {
            match &def.credential_binding {
                CredentialBinding::None => {}
                CredentialBinding::Exact {
                    credential_source_id,
                } => {
                    if let Ok(row) = repo.get(credential_source_id).await {
                        lookup.sources.insert(row.id.0.clone(), row);
                    }
                }
                CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
                    if let Ok(pool) = repo.get_pool(credential_pool_id).await {
                        for member in &pool.members {
                            if let Ok(row) = repo.get(&member.credential_source_id).await {
                                lookup.sources.insert(row.id.0.clone(), row);
                            }
                        }
                        lookup.pools.insert(pool.id.0.clone(), pool);
                    }
                }
            }
        }
        lookup
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

    /// Provision a new session's MCP servers on its thread (ADR-0043 Phase 3),
    /// BEFORE the session record exists — a failure fails the create.
    ///
    /// 1. Each binding's vault credential is materialized to a bearer (a binding
    ///    without a credential stays bearer-less); a missing/broken credential
    ///    row is the caller's fault (`bad_request`, fail closed).
    /// 2. Management-plane merge: the agent's authored [`AgentMcpConfig`]
    ///    (admin `/v1/config/agents/{id}/mcp`), resolved through
    ///    `resolve_mcp_servers`, is appended AFTER the session-inline servers;
    ///    on a duplicate URL the session-inline server wins. A management-plane
    ///    config that cannot resolve is the deployment's fault (`internal`,
    ///    fail closed — the admin routes validated it at write time).
    /// 3. The prepared set is staged on the shared host; the thread's first turn
    ///    connects them (`SharedHost::register_thread_mcp` → `ctx_for`).
    ///
    /// Hosts built without [`ManagedHost::with_mcp`] keep the trait's no-op.
    ///
    /// [`AgentMcpConfig`]: awaken_config_resolver::AgentMcpConfig
    async fn prepare_session(
        &self,
        thread: &str,
        init: awaken_protocol_managed::SessionInit,
    ) -> Result<(), RunError> {
        let Some(mcp) = &self.mcp else {
            return Ok(());
        };
        let mut prepared: Vec<crate::host::PreparedMcpServer> =
            Vec::with_capacity(init.mcp_servers.len());
        for binding in &init.mcp_servers {
            let (bearer, refresh) = match &binding.credential_source_id {
                Some(source_id) => {
                    let row = mcp.credentials.get(source_id).await.map_err(|e| {
                        RunError::bad_request(format!("mcp server `{}`: {e}", binding.name))
                    })?;
                    let bearer = awaken_credential_vault::materialize(&row, &*mcp.secrets)
                        .await
                        .map_err(|e| {
                            RunError::bad_request(format!("mcp server `{}`: {e}", binding.name))
                        })?;
                    // The binding's refresh configuration becomes a live
                    // refresher on the transport: it needs the row's
                    // material_ref to reseal the fresh access token (a vault
                    // row always has one; anything else cannot refresh).
                    let refresh = match (&binding.refresh, &row.material_ref) {
                        (Some(r), Some(access_token_ref)) => Some(crate::mcp::PreparedMcpRefresh {
                            token_endpoint: r.token_endpoint.clone(),
                            client_id: r.client_id.clone(),
                            token_endpoint_auth: r.token_endpoint_auth.clone(),
                            scope: r.scope.clone(),
                            resource: r.resource.clone(),
                            refresh_token_ref: r.refresh_token_ref.clone(),
                            access_token_ref: access_token_ref.clone(),
                            secrets: mcp.secrets.clone(),
                        }),
                        _ => None,
                    };
                    (Some(bearer), refresh)
                }
                None => (None, None),
            };
            prepared.push(crate::host::PreparedMcpServer {
                name: binding.name.clone(),
                url: binding.url.clone(),
                bearer,
                refresh,
            });
        }
        if let Some(config) = mcp.mcp_store.get_agent_config(&init.agent_id) {
            let mut defs = Vec::with_capacity(config.mcp_server_ids.len());
            for server_id in &config.mcp_server_ids {
                defs.push(mcp.mcp_store.get_server(&server_id.0).ok_or_else(|| {
                    RunError::internal(format!(
                        "agent `{}` references unknown mcp server `{}`",
                        init.agent_id, server_id.0
                    ))
                })?);
            }
            let lookup = PrefetchedSourceLookup::for_defs(&defs, &*mcp.credentials).await;
            let resolved =
                awaken_config_resolver::resolve_mcp_servers(&defs, &lookup, &*mcp.secrets)
                    .await
                    .map_err(|e| {
                        RunError::internal(format!(
                            "agent `{}` mcp config did not resolve: {e}",
                            init.agent_id
                        ))
                    })?;
            for server in resolved {
                // Session-inline wins on a duplicate URL: the caller's explicit
                // request (and its vault binding) overrides the authored default.
                if prepared.iter().any(|p| p.url == server.url) {
                    continue;
                }
                prepared.push(crate::host::PreparedMcpServer {
                    name: server.name,
                    url: server.url,
                    bearer: server.credential,
                    // Management-plane servers resolve through the admin
                    // credential model, which has no OAuth refresh object.
                    refresh: None,
                });
            }
        }
        self.host.register_thread_mcp(thread, prepared);
        Ok(())
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

// ── Protocol adapter over the shared host ───────────────────────────────────
//
// AG-UI, AI SDK, and A2A all drive one neutral `ProtocolRuntime` seam
// (`awaken-protocol-transport`), so a single host impl backs all three: a turn
// started through one protocol is resumable and observable through another on the
// same thread. Each wire adapter keeps only its own encoder + router.

fn to_driver_error(err: HostError) -> DriverError {
    match err.kind {
        HostErrorKind::BadRequest => DriverError::BadRequest(err.message),
        HostErrorKind::Internal => DriverError::Internal(err.message),
    }
}

fn to_port_pending(pending: Option<PendingTool>) -> Option<PortPending> {
    pending.map(|p| PortPending {
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
        pending: to_port_pending(result.pending),
    }
}

/// The neutral `ProtocolRuntime` port implemented once over the shared host and
/// wired behind every wire adapter (AI SDK / AG-UI / A2A) — a twin of
/// [`ManagedHost`]. All hold the same `Arc<SharedHost>`, so a turn started by one
/// protocol is resumable and observable through the others on the same thread.
pub struct ProtocolHost {
    host: Arc<SharedHost>,
}

impl ProtocolHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl ProtocolRuntime for ProtocolHost {
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
        resume: PortResume,
    ) -> Result<StepOutcome, DriverError> {
        let resume = match resume {
            PortResume::Confirm { allow, note } => HostResume::Confirm { allow, note },
            PortResume::ClientResult { content, is_error } => {
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

    async fn pending(&self, thread: &str) -> Option<PortPending> {
        to_port_pending(self.host.pending_tool(thread).await)
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
    mount_with_managed(
        host.clone(),
        Arc::new(ManagedState::new(ManagedHost::new(host))),
    )
}

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
fn mount_with_managed(host: Arc<SharedHost>, managed_state: Arc<ManagedState>) -> Router {
    let managed = router(managed_state);
    // One neutral port impl behind the three wire adapters (each `router` takes
    // `Arc<dyn ProtocolRuntime>`), so they share the host with no per-protocol twin.
    let port: Arc<dyn ProtocolRuntime> = Arc::new(ProtocolHost::new(host.clone()));
    let ai_sdk = awaken_protocol_ai_sdk::router(port.clone());
    let ag_ui = awaken_protocol_ag_ui::router(port.clone());
    let a2a = awaken_protocol_a2a::router(port.clone());
    // The durable-ingress operations surface (slice E): ADR-0009 follow-on verbs
    // (supersede / reconcile / reap / dead-letter GC) over the same shared host.
    let durable_ops = crate::durable_ops::durable_ops_router(host.clone());
    managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
}

/// The composition seam refuses to build an executor from an incomplete or
/// unservable [`ResolvedInference`] (ADR-0043, fail-closed).
#[derive(Debug, thiserror::Error)]
pub enum ResolvedExecutorError {
    #[error("resolved inference has no base_url for adapter `{0}`")]
    MissingBaseUrl(&'static str),
    #[error("resolved inference carries no credential (unauthenticated run refused)")]
    MissingCredential,
    #[error("no provider executor in this build serves adapter `{0}`")]
    UnsupportedAdapter(String),
}

/// Build the run-loop's model executor from a management-plane [`ResolvedInference`]
/// (ADR-0043). The resolver already produced the execution triple's adapter kind,
/// endpoint base URL, and the *resolved* credential value; this composition seam is
/// the only place that turns that into the concrete provider executor the host
/// drives. The runtime never sees the credential binding — only the already-resolved
/// [`RedactedString`](awaken_agent_contract::RedactedString) crosses in here (D6/D9),
/// and it is exposed exactly once to construct the client. Fail-closed on a missing
/// base URL/credential or an adapter this build cannot serve.
pub fn executor_from_resolved(
    inference: &ResolvedInference,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    match inference.adapter_kind {
        // The genai provider speaks the Anthropic Messages wire (native + the many
        // Anthropic-compatible gateways). Its base URL and key come from the catalog
        // endpoint and the resolved credential, never inlined by the Managed wire.
        "anthropic" => {
            let base_url = inference
                .base_url
                .clone()
                .ok_or(ResolvedExecutorError::MissingBaseUrl("anthropic"))?;
            let credential = inference
                .credential
                .as_ref()
                .ok_or(ResolvedExecutorError::MissingCredential)?;
            Ok(Arc::new(GenaiExecutor::anthropic_compatible(
                base_url,
                credential.expose_secret(),
            )))
        }
        other => Err(ResolvedExecutorError::UnsupportedAdapter(other.to_string())),
    }
}

/// Build the full server router for a resolved run: turn the [`ResolvedInference`]
/// into the host's model executor and mount the protocol adapters over it. The
/// router's model ref is the resolved triple's model id, so a run started here calls
/// the exact model the management plane bound. This is the composition-root end of
/// the config → resolve → run chain (ADR-0043).
pub fn build_resolved_router(
    inference: &ResolvedInference,
) -> Result<Router, ResolvedExecutorError> {
    let executor = executor_from_resolved(inference)?;
    Ok(build_router(executor, inference.triple.model_id.clone()))
}

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    mount(Arc::new(SharedHost::new(llm, model_ref)))
}

/// A server backed by a **live** Anthropic-compatible model, configured from the
/// environment: `ANTHROPIC_API_KEY` (or `KIMI_API_KEY`), `ANTHROPIC_BASE_URL` (or
/// `KIMI_BASE_URL`), `ANTHROPIC_MODEL` (or `KIMI_MODEL`). This is the same
/// `RedactedString` → `GenaiExecutor` seam the resolver's `executor_from_resolved`
/// uses (verified equivalent by `tests/resolved_run.rs`), exposed as a server mode
/// so the TypeScript e2e can drive a real turn through the managed / ai-sdk / a2a
/// adapters. Panics if no API key is set, so a misconfigured run fails loudly.
pub fn build_real_router() -> Router {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());
    let executor = GenaiExecutor::anthropic_compatible(base, key);
    build_router(Arc::new(executor), model)
}

/// A server backed by **Gemini on Vertex AI**, authenticated by an OAuth2 Bearer
/// token (ADR-0043 Phase 3 multi-flavor + OAuth). The token is refreshed through
/// the credential domain's OAuth helper: `GEMINI_ACCESS_TOKEN` if set, else
/// `gcloud auth print-access-token` (which holds the long-lived Google grant).
/// Config from the environment: `GEMINI_PROJECT` (required), `GEMINI_LOCATION`
/// (default `global`), `GEMINI_MODEL` (default `gemini-2.5-flash`). Exposed as a
/// server mode so the TypeScript e2e can drive a real Gemini turn — proving the
/// OAuth + Gemini path through the managed / ai-sdk adapters.
pub async fn build_real_gemini_router() -> Router {
    use awaken_credential_vault::{CommandTokenSource, TokenSource};

    let project = std::env::var("GEMINI_PROJECT")
        .expect("set GEMINI_PROJECT for AWAKEN_MODEL_MODE=real-gemini");
    let location = std::env::var("GEMINI_LOCATION").unwrap_or_else(|_| "global".to_string());
    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
    let token = match std::env::var("GEMINI_ACCESS_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => CommandTokenSource::gcloud()
            .access_token()
            .await
            .expect("refresh a Google OAuth2 token via gcloud")
            .expose_secret()
            .to_string(),
    };
    let executor = GenaiExecutor::vertex_gemini(project, location, token);
    build_router(Arc::new(executor), model)
}

/// A live-model server whose executor is built **through the resolver**
/// (ADR-0043): it authors an in-memory catalog + enters a credential from the
/// environment, then `resolve_inference` + `executor_from_resolved` produce the
/// host executor — the same config → resolve → run path a managed run takes, rather
/// than constructing the provider directly (as `build_real_router` does). Exposed
/// so a session e2e exercises the resolver end to end against a real model. Env:
/// `ANTHROPIC_API_KEY`/`KIMI_API_KEY` (+ `*_BASE_URL`, `*_MODEL`).
pub async fn build_resolved_real_router() -> Router {
    use std::collections::HashMap;

    use awaken_agent_contract::RedactedString;
    use awaken_config_resolver::resolve_inference;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialBinding, CredentialCreateParams, CredentialKind, CredentialSource,
        CredentialSourceId, InMemorySecretStore,
    };
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };

    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real-resolved");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());

    // Author the catalog: one provider + endpoint + offering for `model`.
    let catalog_repo = InMemoryCatalogRepo::new();
    catalog_repo
        .put_provider(Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        })
        .await
        .expect("put provider");
    catalog_repo
        .put_endpoint(ProtocolEndpoint {
            id: ProtocolEndpointId::new("ep1"),
            provider_id: ProviderId::new("anthropic"),
            flavor: ModelApiCompat::AnthropicMessages,
            base_url: Some(base),
            timeout_secs: 300,
            display_name: "prod".into(),
            version: 1,
        })
        .await
        .expect("put endpoint");
    catalog_repo
        .put_offering(Offering {
            model_id: model.clone(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        })
        .await
        .expect("put offering");

    // Enter the credential (secret-in), then resolve the inference against the
    // authored catalog and build the executor from the resolved value.
    let secrets = InMemorySecretStore::new();
    let cred_repo = InMemoryCredentialRepo::new();
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(key)),
        },
        &secrets,
        &cred_repo,
    )
    .await
    .expect("enter credential");
    let catalog = catalog_repo.snapshot().await.expect("catalog snapshot");
    let row = cred_repo.get(&source.id).await.expect("credential row");
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(row.id.0.clone(), row);
    let inference = resolve_inference(
        &catalog,
        &model,
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(source.id.0.clone()),
        },
        &sources,
        &secrets,
    )
    .await
    .expect("resolve inference");
    let executor = executor_from_resolved(&inference).expect("build executor from resolved");
    build_router(executor, inference.triple.model_id.clone())
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

/// A richer tool state machine (coverage): a per-key machine (`key`/`key_normalizer`)
/// whose transitions gate on the tool *result* (`when`) rather than just its args —
/// exercising the result matchers (status + content) and the key template. The
/// driving model calls `glob` twice with the same pattern: the first advances
/// `s0 -> s1` on a `success` result (its emit fires), the second advances `s1 -> s2`
/// (terminal) on `any` result (a second emit). No violation — both calls advance.
pub fn build_statemachine_rich_router() -> Router {
    let machine = serde_json::json!({
        "machines": [{
            "name": "keyed",
            "key": "${pattern}",
            "key_normalizer": "lowercase",
            "initial": "s0",
            "terminal": ["s2"],
            "transitions": [
                {
                    "on": "glob(pattern ~ \"*\")",
                    "from": ["s0"],
                    "to": "s1",
                    "when": "success",
                    "emit": { "target": "system", "content": "first glob succeeded", "cooldown_turns": 0 }
                },
                {
                    "on": "glob(pattern ~ \"*\")",
                    "from": ["s1"],
                    "to": "s2",
                    "when": { "status": "success", "content": "*" },
                    "emit": { "target": "system", "content": "second glob advanced", "cooldown_turns": 0 }
                }
            ]
        }]
    });
    let host = SharedHost::new(Arc::new(StateMachineModel), "statemachine-rich")
        .with_state_machine(machine);
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

/// The live credential-validation probe port (ADR-0043), backed by provider-genai.
/// This is the only place the model SDK is named for validation — the admin CRUD
/// crate depends on the `CredentialProbe` trait, not on genai.
struct GenaiProbe;

#[async_trait::async_trait]
impl awaken_admin_config_api::CredentialProbe for GenaiProbe {
    async fn probe(
        &self,
        base_url: &str,
        secret: &awaken_agent_contract::RedactedString,
        model: &str,
    ) -> awaken_admin_config_api::ProbeStatus {
        use awaken_admin_config_api::ProbeStatus;
        use awaken_provider_genai::CredentialProbe;
        match awaken_provider_genai::probe_credential(base_url, secret.expose_secret(), model).await
        {
            CredentialProbe::Valid => ProbeStatus::Valid,
            CredentialProbe::Invalid => ProbeStatus::Invalid,
            CredentialProbe::Unknown => ProbeStatus::Unknown,
        }
    }
}

/// The store set the management plane runs over — one instance of each port,
/// shared by the admin router, the vault front door, and session prepare.
struct ManagementStores {
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>,
    mcp: Arc<dyn awaken_admin_config_api::McpStore>,
}

/// Ephemeral management stores: everything in process memory (dev / e2e default).
fn in_memory_management_stores() -> ManagementStores {
    ManagementStores {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
    }
}

/// Durable management stores under `dir` (created if absent), ADR-0043
/// sqlite-repos: one SQLite file per domain bundle —
///
/// - `catalog.db`   — the `awaken.catalog` bundle (providers/endpoints/offerings)
/// - `credential.db` — the `awaken.credential` bundle: the secret-free
///   source/pool rows (`SqliteCredentialRepo`) **and** the AEAD-sealed secret
///   blobs (`SqliteSealedBlobStore` under `SealedAeadSecretStore::over`, sealed
///   with `key`). The two adapters share the one file safely: both run the same
///   `credential` migration bundle, and the scoped-migration ledger makes the
///   second run a no-op; admin-plane writes are short single statements, so two
///   connections on one file do not contend in practice.
/// - `admin.db`     — the `awaken.admin` bundle (profiles / MCP defs / agent↔MCP)
///
/// Panics on open/migrate failure: the binary's mode selection has no error
/// channel (matching e.g. `build_config_router`), and a management server that
/// silently fell back to ephemeral stores would be worse than one that refuses
/// to start.
fn durable_management_stores(dir: &std::path::Path, key: &[u8; 32]) -> ManagementStores {
    std::fs::create_dir_all(dir).expect("create AWAKEN_MGMT_DIR");
    let db = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let catalog = awaken_model_catalog::sqlite::SqliteCatalogRepo::open(&db("catalog.db"))
        .expect("open catalog.db under AWAKEN_MGMT_DIR");
    let credentials = awaken_credential_vault::SqliteCredentialRepo::open(&db("credential.db"))
        .expect("open credential.db under AWAKEN_MGMT_DIR");
    let blobs = awaken_credential_vault::SqliteSealedBlobStore::open(&db("credential.db"))
        .expect("open credential.db sealed-blob store under AWAKEN_MGMT_DIR");
    let admin = Arc::new(
        awaken_admin_config_api::SqliteAdminStore::open(&db("admin.db"))
            .expect("open admin.db under AWAKEN_MGMT_DIR"),
    );
    ManagementStores {
        catalog: Arc::new(catalog),
        credentials: Arc::new(credentials),
        // The only durable secret path is sealed: `nonce ‖ ciphertext` under the
        // operator-held key — plaintext never reaches the disk.
        secrets: Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
            key,
            Arc::new(blobs),
        )),
        profiles: admin.clone(),
        mcp: admin,
    }
}

/// Parse `AWAKEN_MGMT_SEAL_KEY`: exactly 64 hex characters (a 32-byte AEAD key).
fn parse_seal_key(hex: &str) -> Result<[u8; 32], String> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(format!(
            "expected 64 hex characters (a 32-byte key), got {} characters",
            hex.len()
        ));
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("not hex at position {}", 2 * i))?;
    }
    Ok(key)
}

/// The AEAD key for the durable management plane, from `AWAKEN_MGMT_SEAL_KEY`
/// (64 hex characters = 32 bytes). Fails loudly when unset or malformed: a
/// durable store sealed under an ephemeral random key would look healthy until
/// the first restart, then every persisted secret would be unopenable.
fn mgmt_seal_key_from_env() -> [u8; 32] {
    let hex = std::env::var("AWAKEN_MGMT_SEAL_KEY").unwrap_or_else(|_| {
        panic!(
            "AWAKEN_MGMT_DIR is set but AWAKEN_MGMT_SEAL_KEY is not. A durable \
             management store needs a stable AEAD key (64 hex characters = 32 bytes); \
             sealing under an ephemeral key would brick every restart."
        )
    });
    parse_seal_key(&hex).unwrap_or_else(|reason| {
        panic!("AWAKEN_MGMT_SEAL_KEY is malformed: {reason}. Provide 64 hex characters (a 32-byte key).")
    })
}

/// Serve the management plane (admin + vaults + sessions) with **persistence
/// selected from the environment** (mirrors `AWAKEN_STORE` / `AWAKEN_INGRESS`):
///
/// - `AWAKEN_MGMT_DIR` unset — in-memory stores, exactly the previous behavior.
/// - `AWAKEN_MGMT_DIR=<dir>` — SQLite-backed stores under `<dir>`
///   (`catalog.db` / `credential.db` / `admin.db`), with secrets AEAD-sealed
///   under `AWAKEN_MGMT_SEAL_KEY` (**required** then: 64 hex characters = a
///   32-byte key; unset or malformed panics rather than sealing under a key
///   that cannot survive a restart).
///
/// What persists across a restart is the authored **domain** state: the catalog,
/// the secret-free credential/pool rows plus their sealed secrets, and the
/// admin aggregates (inference profiles, MCP server defs, agent↔MCP bindings).
/// The Managed **wire** bookkeeping stays host-ephemeral by design: vault ids /
/// vault-credential wire objects (`VaultState`), sessions, and thread state are
/// rebuilt fresh per process (session durability has its own axis,
/// `AWAKEN_STORAGE_DIR`). After a restart a vault wire GET 404s while the
/// domain row it entered is still there for the resolver.
///
/// Additionally (ADR-0042/0043 P1), `AWAKEN_MGMT_IAM=embedded` gates the
/// management surfaces (`/v1/config/*` + `/v1/vaults/*`) behind bearer
/// `ApiToken` authn + preset-role authz (see [`crate::authz`]); it requires
/// `AWAKEN_MGMT_DIR` (the token/binding rows live in `<dir>/iam.sqlite`) and
/// panics with a clear message when it is missing. Unset — the default — is
/// today's open behavior, byte-identical.
pub fn build_management_router() -> Router {
    let iam = match std::env::var("AWAKEN_MGMT_IAM") {
        Ok(mode) if mode == "embedded" => {
            let dir = std::env::var("AWAKEN_MGMT_DIR").unwrap_or_else(|_| {
                panic!(
                    "AWAKEN_MGMT_IAM=embedded requires AWAKEN_MGMT_DIR: the embedded \
                     IAM persists its API tokens and role bindings under \
                     <AWAKEN_MGMT_DIR>/iam.sqlite; an in-memory token directory would \
                     mint a fresh bootstrap admin token on every restart."
                )
            });
            Some(embedded_iam(std::path::Path::new(&dir)))
        }
        Ok(other) => panic!(
            "unsupported AWAKEN_MGMT_IAM value `{other}`: only `embedded` (or unset for \
             the open management plane) is supported"
        ),
        Err(_) => None,
    };
    match std::env::var("AWAKEN_MGMT_DIR") {
        Ok(dir) => {
            let key = mgmt_seal_key_from_env();
            management_router_over(
                durable_management_stores(std::path::Path::new(&dir), &key),
                iam,
            )
        }
        Err(_) => management_router_over(in_memory_management_stores(), iam),
    }
}

/// [`build_management_router`] with explicit persistence inputs (no environment
/// read): the durable management plane over `dir`, sealing secrets under `key`.
/// Exposed so a restart test can rebuild a router over one directory across
/// simulated process lifetimes without racing on process-global env vars.
/// No IAM guard — the open (default) management plane.
pub fn build_durable_management_router(dir: &std::path::Path, key: &[u8; 32]) -> Router {
    management_router_over(durable_management_stores(dir, key), None)
}

/// [`build_durable_management_router`] with the embedded IAM guard enabled —
/// the env-free equivalent of `AWAKEN_MGMT_IAM=embedded`. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint
/// further workspace tokens against the same policy state.
pub fn build_secured_management_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let router = management_router_over(durable_management_stores(dir, key), Some(iam.clone()));
    (router, iam)
}

/// Mount the management plane over an explicit store set, optionally gated by
/// the embedded IAM guard (`iam`). The guard wraps ONLY the admin + vault
/// routers: the Managed session surface keeps its own axis and P1 does not
/// gate it (ADR-0043).
fn management_router_over(stores: ManagementStores, iam: Option<Arc<ManagementAuthz>>) -> Router {
    let ManagementStores {
        catalog,
        credentials,
        secrets,
        profiles,
        mcp: mcp_store,
    } = stores;
    // ONE MCP store across the admin router and the ManagedHost, and ONE
    // credential repo + secret store across admin, vaults, and sessions: a
    // credential or MCP config entered through any surface is the same row a
    // session's prepare reads (ADR-0043 Phase 3).
    let admin = awaken_admin_config_api::admin_router(awaken_admin_config_api::AdminState {
        catalog,
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles,
        mcp: mcp_store.clone(),
        // The live credential probe is backed by provider-genai here — the only
        // place the model SDK is named; the admin CRUD crate stays SDK-free.
        probe: Some(Arc::new(GenaiProbe)),
    });
    let vault_state = Arc::new(
        awaken_protocol_managed::VaultState::new(secrets.clone(), credentials.clone())
            // The live MCP probe is backed by ext-mcp here — the only place the
            // MCP client is named for validation; the adapter crate stays
            // wire-client-free (mirrors the GenaiProbe pattern above).
            .with_probe(Arc::new(crate::mcp::ExtMcpProbe)),
    );
    let vaults = awaken_protocol_managed::vault_router(vault_state.clone());

    // The IAM guard (when enabled) wraps the admin + vault routers only. An
    // axum layer binds to the routes present when it is applied, so merging
    // the guarded sub-router later leaves every other surface untouched. The
    // token-management routes exist ONLY under the guard (they authorize
    // against the same embedded IAM the guard authenticates with), and they
    // are merged before the layer so the guard authenticates them first.
    let mut mgmt = admin.merge(vaults);
    if let Some(iam) = iam {
        mgmt = mgmt.merge(crate::authz::token_router(iam.clone()));
        mgmt = mgmt.layer(axum::middleware::from_fn_with_state(
            iam,
            crate::authz::management_guard,
        ));
    }

    // The MCP-driving deterministic model, so an e2e can hold a real multi-turn
    // conversation through ext-mcp (`add a b` → mcp__calc__add → `result: …`);
    // non-`add` turns still echo, preserving the prior expectations.
    let host = Arc::new(SharedHost::new(Arc::new(McpToolModel), "management"));
    let managed_state = Arc::new(
        ManagedState::new(ManagedHost::new(host.clone()).with_mcp(credentials, secrets, mcp_store))
            .with_vaults(vault_state),
    );
    mount_with_managed(host, managed_state).merge(mgmt)
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

/// A router that routes `agent_run` for `researcher` to a REMOTE A2A agent at
/// `AWAKEN_REMOTE_AGENT_URL` (instead of a local sub-run). Exercises the remote
/// delegation path — `message:send` → poll `get_task` → result — across a real A2A
/// hop to a peer server. The peer echoes, so the delegate result round-trips back.
pub fn build_remote_delegation_router() -> Router {
    let url = std::env::var("AWAKEN_REMOTE_AGENT_URL")
        .expect("AWAKEN_REMOTE_AGENT_URL must be set for delegate-remote mode");
    let host = SharedHost::new(Arc::new(DelegatingModel), "delegate-remote")
        .with_remote_a2a("researcher", Arc::new(HttpTransport::new(url)));
    mount(Arc::new(host))
}

/// A deterministic model for the skills e2e (ADR-0036). On the user turn it calls
/// `list_skills` to discover the offered skills; given the catalog it activates the
/// `greet` skill via the `Skill` tool; given the activation instructions it replies
/// with them — so an e2e can assert discover → activate → use end to end. Stateless.
pub struct SkillDrivingModel;

#[async_trait::async_trait]
impl LlmExecutor for SkillDrivingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request.messages.last().expect("a message");
        // A tool result carries its text in nested blocks, which `block_text` skips;
        // read those too so the catalog (a tool result) is visible to the model.
        let last_text: String = last
            .content
            .iter()
            .flat_map(|b| match b {
                ContentBlock::Text { text } => vec![text.clone()],
                ContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(|inner| match inner {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            })
            .collect::<Vec<_>>()
            .join("");
        let output = match last.role {
            ChatRole::User => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "l".into(),
                tool_id: "list_skills".into(),
                arguments: serde_json::json!({}),
            }]),
            ChatRole::Tool if last_text.contains("\"skills\"") => {
                // The catalog came back — activate the offered `greet` skill.
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s".into(),
                    tool_id: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "greet" }),
                }])
            }
            ChatRole::Tool => AssistantOutput::text(format!("USED-SKILL: {last_text}")),
            _ => AssistantOutput::text("hmm"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A router offering skills on every thread (ADR-0036), driven by a model that
/// discovers, activates, and uses one. The whole skill set is fronted by the single
/// `Skill` tool plus `list_skills`; activation returns the skill's instructions.
pub fn build_skills_router() -> Router {
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING-FROM-SKILL");
    let review = SkillSpec::new("review", "Review", "review code", "REVIEW-BODY");
    build_router_with_skills(Arc::new(SkillDrivingModel), "skills", vec![greet, review])
}
