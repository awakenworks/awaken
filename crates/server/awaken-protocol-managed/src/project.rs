//! Project committed neutral messages into public Managed Agents events.
//!
//! The message fold is shared (`awaken_agent_contract::event`); this module
//! only owns the Managed *transcoder* — the `Fact -> OutboundKind` mapping
//! — and the terminal `session.status_idle` shape. An assistant turn becomes an
//! `agent.message` plus an `agent.tool_use` (or `agent.custom_tool_use`) per call;
//! a tool message becomes an `agent.tool_result`; a step ends with
//! `session.status_idle`. A pending client tool projects as `agent.custom_tool_use`;
//! a pending built-in tool as `agent.tool_use{ask}`; anything else ran
//! (`agent.tool_use{allow}`).

use awaken_agent_contract::event::{
    Fact, ToolDisposition, Transcoder, fold_messages as fold, terminal_waiting,
};

use crate::state::{AgentCapabilities, OutcomeIteration};
use crate::types::{OutboundKind, StopReason};

/// The versioned built-in toolset id (Managed Agents wire vocabulary, G16).
const AGENT_TOOLSET_TYPE: &str = "agent_toolset_20260401";

/// The canonical tools the versioned agent toolset bundles. A built-in tool the host
/// does *not* register is disabled in `configs`; a registered tool that requires
/// confirmation carries an `always_ask` permission policy.
const AGENT_TOOLSET_TOOLS: [&str; 8] = [
    "bash",
    "read",
    "write",
    "edit",
    "glob",
    "grep",
    "web_fetch",
    "web_search",
];

/// Project the agent's tool surface onto the public `agent.tools` array. The built-in
/// tools fold into a single `agent_toolset_20260401` reference (never per-tool
/// definitions) with `configs` that disable the toolset tools the host does not
/// register and mark the confirmation-gated ones `always_ask`; each client tool
/// becomes a `custom` tool definition.
pub fn agent_tools(caps: &AgentCapabilities) -> Vec<serde_json::Value> {
    let mut tools = Vec::new();
    if !caps.builtin_tools.is_empty() {
        let mut configs = Vec::new();
        for name in AGENT_TOOLSET_TOOLS {
            match caps.builtin_tools.iter().find(|t| t.name == name) {
                None => configs.push(serde_json::json!({ "name": name, "enabled": false })),
                Some(tool) if tool.ask => configs.push(serde_json::json!({
                    "name": name,
                    "permission_policy": { "type": "always_ask" },
                })),
                Some(_) => {} // registered and auto-allowed → toolset default
            }
        }
        let mut toolset = serde_json::json!({ "type": AGENT_TOOLSET_TYPE });
        if !configs.is_empty() {
            toolset["configs"] = serde_json::Value::Array(configs);
        }
        tools.push(toolset);
    }
    for tool in &caps.custom_tools {
        tools.push(serde_json::json!({
            "type": "custom",
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.input_schema,
        }));
    }
    tools
}

/// Project the agent's offered skills onto the public `agent.skills` array. Each is a
/// `custom` skill reference (the host offers them locally, not from the Skills API).
pub fn agent_skills(caps: &AgentCapabilities) -> Vec<serde_json::Value> {
    caps.skills
        .iter()
        .map(|id| serde_json::json!({ "type": "custom", "skill_id": id, "version": "latest" }))
        .collect()
}

/// Project the agent's delegate roster onto the public `agent.multiagent` coordinator
/// object, or `None` when the agent delegates to no one.
pub fn agent_multiagent(caps: &AgentCapabilities) -> Option<serde_json::Value> {
    if caps.delegates.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "type": "coordinator",
        "agents": caps.delegates,
    }))
}

/// The public wire object for one recorded outcome evaluation on the session: the
/// outcome id and the verdict. This is the durable `outcome_evaluations` entry on the
/// session object, distinct from the transient `span.outcome_evaluation_*` events.
pub fn outcome_evaluation(round: &OutcomeIteration) -> serde_json::Value {
    serde_json::json!({
        "outcome_id": round.outcome_id,
        "result": round.result,
    })
}

/// One projected event, with an optional stable id. A tool-use event carries the
/// tool call's own id (so a `user.tool_confirmation` can reference it); other
/// events let the adapter mint an `evt_*` id.
pub struct ProjectedEvent {
    pub id: Option<String>,
    pub kind: OutboundKind,
}

impl ProjectedEvent {
    fn minted(kind: OutboundKind) -> Self {
        Self { id: None, kind }
    }
    fn with_id(id: String, kind: OutboundKind) -> Self {
        Self { id: Some(id), kind }
    }
}

/// The MCP tool-name prefix (`mcp__<server>__<tool>`). A call whose tool name
/// carries it projects as the distinct `agent.mcp_tool_use`/`agent.mcp_tool_result`
/// events rather than the generic `agent.tool_use`/`agent.tool_result`.
const MCP_TOOL_PREFIX: &str = "mcp__";

/// The MCP server segment of an `mcp__<server>__<tool>` name (`""` when absent).
fn mcp_server_name(name: &str) -> String {
    name.strip_prefix(MCP_TOOL_PREFIX)
        .and_then(|rest| rest.split("__").next())
        .unwrap_or_default()
        .to_string()
}

/// The Managed Agents transcoder: neutral projection events to public
/// `OutboundKind`. `RunStarted` is dropped (Managed has no per-step start event);
/// terminal events become `session.status_idle`. Stateful within one turn: it
/// remembers `mcp__` tool-use ids so their results project as MCP results too.
#[derive(Default)]
pub struct ManagedEncoder {
    mcp_ids: std::collections::HashSet<String>,
}

impl Transcoder for ManagedEncoder {
    type Output = ProjectedEvent;

    fn transcode(&mut self, event: &Fact) -> Vec<ProjectedEvent> {
        match event {
            Fact::RunStarted => Vec::new(),
            Fact::AssistantMessage { content, .. } => {
                vec![ProjectedEvent::minted(OutboundKind::AgentMessage {
                    content: content.clone(),
                })]
            }
            // An MCP tool call (`mcp__server__tool`) is host-executed like a
            // built-in; project it as the distinct MCP events.
            Fact::ToolCall {
                id,
                name,
                input,
                disposition,
            } if name.starts_with(MCP_TOOL_PREFIX) => {
                self.mcp_ids.insert(id.clone());
                let evaluated_permission = match disposition {
                    ToolDisposition::PendingBuiltin => Some("ask".to_string()),
                    _ => Some("allow".to_string()),
                };
                vec![ProjectedEvent::with_id(
                    id.clone(),
                    OutboundKind::AgentMcpToolUse {
                        name: name.clone(),
                        mcp_server_name: mcp_server_name(name),
                        input: input.clone(),
                        evaluated_permission,
                    },
                )]
            }
            Fact::ToolCall {
                id,
                name,
                input,
                disposition,
            } => {
                let kind = match disposition {
                    ToolDisposition::PendingClient => OutboundKind::AgentCustomToolUse {
                        name: name.clone(),
                        input: input.clone(),
                    },
                    ToolDisposition::PendingBuiltin => OutboundKind::AgentToolUse {
                        name: name.clone(),
                        input: input.clone(),
                        evaluated_permission: Some("ask".to_string()),
                    },
                    ToolDisposition::Executed => OutboundKind::AgentToolUse {
                        name: name.clone(),
                        input: input.clone(),
                        evaluated_permission: Some("allow".to_string()),
                    },
                };
                vec![ProjectedEvent::with_id(id.clone(), kind)]
            }
            Fact::ToolResult { id, content, .. } if self.mcp_ids.contains(id) => {
                vec![ProjectedEvent::minted(OutboundKind::AgentMcpToolResult {
                    mcp_tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: None,
                })]
            }
            Fact::ToolResult { id, content, .. } => {
                vec![ProjectedEvent::minted(OutboundKind::AgentToolResult {
                    tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: None,
                })]
            }
            Fact::Waiting {
                pending_tool_use_id,
            } => vec![ProjectedEvent::minted(OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction {
                    event_ids: pending_tool_use_id.clone().into_iter().collect(),
                },
            })],
            Fact::RunFinished { exhausted } => {
                let stop_reason = if *exhausted {
                    StopReason::RetriesExhausted
                } else {
                    StopReason::EndTurn
                };
                vec![ProjectedEvent::minted(OutboundKind::SessionStatusIdle {
                    stop_reason,
                })]
            }
            // The managed wire vocabulary has no error stop reason; a failed
            // run still idles the session with EndTurn — the fault stays
            // authoritative in the run's committed phase.
            Fact::RunFailed { .. } => {
                vec![ProjectedEvent::minted(OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::EndTurn,
                })]
            }
            // A continuation-guard round is an internal audit fact, not an
            // agent-visible managed wire event; the fold never emits it here.
            Fact::Continuation { .. } => Vec::new(),
        }
    }
}

/// Project just the agent-visible events for a batch of committed messages (no
/// terminal `session.status_idle`). Used by both a turn and an outcome iteration.
/// `pending` is `(tool_use_id, client_executed)` of the tool the run parked on.
pub fn project_messages(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<(&str, bool)>,
) -> Vec<ProjectedEvent> {
    ManagedEncoder::default().transcode_all(&fold(messages, pending))
}

/// Project the messages committed during one step, then a terminal
/// `session.status_idle` derived from `stop`. When `stop` is `RequiresAction` the
/// pending tool's id populates `requires_action.event_ids`.
pub fn project_step(
    messages: &[awaken_agent_contract::agent::message::Message],
    stop: StopReason,
    pending: Option<(&str, bool)>,
) -> Vec<ProjectedEvent> {
    let mut events = fold(messages, pending);
    events.push(terminal_event(stop, pending));
    ManagedEncoder::default().transcode_all(&events)
}

/// The neutral terminal event for a Managed `stop_reason`. `RequiresAction`'s
/// event ids are refilled from the pending tool.
fn terminal_event(stop: StopReason, pending: Option<(&str, bool)>) -> Fact {
    match stop {
        StopReason::RequiresAction { .. } => terminal_waiting(pending.map(|p| p.0)),
        StopReason::RetriesExhausted => Fact::RunFinished { exhausted: true },
        StopReason::EndTurn => Fact::RunFinished { exhausted: false },
    }
}
