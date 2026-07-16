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

use crate::state::{AgentCapabilities, CustomTool, OutcomeIteration};
use crate::types::{OutboundKind, StopReason};

/// Reserved MCP tool-name prefix — a custom tool may not claim it.
const MCP_RESERVED_PREFIX: &str = "mcp__";
/// Anthropic's custom-tool name length ceiling.
const MAX_TOOL_NAME_LEN: usize = 128;
/// JSON-Schema composition keywords the managed toolset input_schema rejects.
const FORBIDDEN_SCHEMA_KEYS: [&str; 2] = ["$ref", "oneOf"];

/// Validate a host/client-declared custom tool against the Managed Agents rules the
/// real API enforces at definition time, so an invalid definition fails closed here
/// rather than silently working on awaken yet 400-ing on Anthropic. The `Err` string
/// is the `invalid_request_error` wire message. Rules (per the conformance matrix):
/// name charset `[A-Za-z0-9_-]`, length `1..=128`, no reserved `mcp__` prefix, and
/// no `$ref` / `oneOf` composition anywhere in `input_schema`.
pub fn validate_custom_tool(tool: &CustomTool) -> Result<(), String> {
    if tool.name.is_empty() || tool.name.len() > MAX_TOOL_NAME_LEN {
        return Err(format!(
            "custom tool name must be 1..={MAX_TOOL_NAME_LEN} characters (got {})",
            tool.name.len()
        ));
    }
    if !tool
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "custom tool name `{}` has invalid characters (allowed: A-Za-z0-9_-)",
            tool.name
        ));
    }
    if tool.name.starts_with(MCP_RESERVED_PREFIX) {
        return Err(format!(
            "custom tool name `{}` may not start with the reserved `{MCP_RESERVED_PREFIX}` prefix",
            tool.name
        ));
    }
    if let Some(key) = first_forbidden_schema_key(&tool.input_schema) {
        return Err(format!(
            "custom tool `{}` input_schema may not use `{key}`",
            tool.name
        ));
    }
    Ok(())
}

/// The first forbidden JSON-Schema composition keyword found anywhere in the tree.
fn first_forbidden_schema_key(v: &serde_json::Value) -> Option<&'static str> {
    match v {
        serde_json::Value::Object(map) => FORBIDDEN_SCHEMA_KEYS
            .into_iter()
            .find(|k| map.contains_key(*k))
            .or_else(|| map.values().find_map(first_forbidden_schema_key)),
        serde_json::Value::Array(items) => items.iter().find_map(first_forbidden_schema_key),
        _ => None,
    }
}

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
        // Each entry is the tool's *resolved* config — the SDK's
        // `BetaManagedAgentsAgentToolConfig` requires all of {name, enabled,
        // permission_policy}. Only deviations from `default_config` are listed;
        // an auto-allowed registered tool matches the default and is omitted.
        let mut configs = Vec::new();
        for name in AGENT_TOOLSET_TOOLS {
            match caps.builtin_tools.iter().find(|t| t.name == name) {
                None => configs.push(serde_json::json!({
                    "name": name,
                    "enabled": false,
                    "permission_policy": { "type": "always_allow" },
                })),
                Some(tool) if tool.ask => configs.push(serde_json::json!({
                    "name": name,
                    "enabled": true,
                    "permission_policy": { "type": "always_ask" },
                })),
                Some(_) => {} // registered + auto-allowed → matches default_config
            }
        }
        // `configs` and `default_config` are both required on the toolset object.
        // `default_config` is the resolved baseline every non-overridden tool
        // inherits: enabled and auto-allowed.
        tools.push(serde_json::json!({
            "type": AGENT_TOOLSET_TYPE,
            "configs": configs,
            "default_config": {
                "enabled": true,
                "permission_policy": { "type": "always_allow" },
            },
        }));
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

    fn fact(&mut self, event: &Fact) -> Vec<ProjectedEvent> {
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
    ManagedEncoder::default().transcode_facts(&fold(messages, pending))
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
    ManagedEncoder::default().transcode_facts(&events)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, schema: serde_json::Value) -> CustomTool {
        CustomTool {
            name: name.to_string(),
            description: "d".to_string(),
            input_schema: schema,
        }
    }

    #[test]
    fn validate_custom_tool_accepts_well_formed_tools() {
        assert!(
            validate_custom_tool(&tool(
                "submit_answer",
                serde_json::json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
            ))
            .is_ok()
        );
        // Hyphen/digit/underscore charset and a full-length (128) name are in range.
        assert!(
            validate_custom_tool(&tool("a-1_B", serde_json::json!({ "type": "object" }))).is_ok()
        );
        assert!(validate_custom_tool(&tool(&"x".repeat(128), serde_json::json!({}))).is_ok());
    }

    #[test]
    fn validate_custom_tool_rejects_the_documented_violations() {
        // Charset: spaces and dots are out.
        assert!(validate_custom_tool(&tool("has space", serde_json::json!({}))).is_err());
        assert!(validate_custom_tool(&tool("dots.bad", serde_json::json!({}))).is_err());
        // Reserved MCP prefix.
        assert!(validate_custom_tool(&tool("mcp__srv__t", serde_json::json!({}))).is_err());
        // Length: empty and 129 chars both rejected.
        assert!(validate_custom_tool(&tool("", serde_json::json!({}))).is_err());
        assert!(validate_custom_tool(&tool(&"x".repeat(129), serde_json::json!({}))).is_err());
        // Composition keywords rejected at the top level and when nested.
        assert!(validate_custom_tool(&tool("t", serde_json::json!({ "$ref": "#/x" }))).is_err());
        assert!(validate_custom_tool(&tool("t", serde_json::json!({ "oneOf": [] }))).is_err());
        assert!(
            validate_custom_tool(&tool(
                "t",
                serde_json::json!({
                    "type": "object",
                    "properties": { "y": { "oneOf": [{ "type": "string" }] } }
                }),
            ))
            .is_err()
        );
    }
}
