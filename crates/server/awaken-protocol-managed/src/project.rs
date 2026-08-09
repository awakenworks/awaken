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
    Fact, ToolDisposition, Transcoder, fold_messages as fold, terminal_awaiting,
};
use awaken_session_contract::Pending;

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

use awaken_session_contract::{
    AgentTool, CustomToolInputSchema, resolved_toolsets, toolset_policies,
};

/// Lower the Managed tool union into the Session's one neutral durable owner.
pub fn session_tool_configuration(
    tools: &[AgentTool],
) -> awaken_session_contract::SessionToolConfiguration {
    awaken_session_contract::SessionToolConfiguration {
        toolsets: toolset_policies(tools),
        client_tools: tools
            .iter()
            .filter_map(|tool| match tool {
                AgentTool::Custom {
                    name,
                    description,
                    input_schema,
                } => Some(awaken_agent_contract::ClientToolDescriptor {
                    name: name.clone(),
                    description: description.clone(),
                    input_schema: serde_json::to_value(input_schema)
                        .expect("Managed custom tool schema serializes"),
                }),
                AgentTool::AgentToolset20260401 { .. } | AgentTool::McpToolset { .. } => None,
            })
            .collect(),
    }
}

/// Project one neutral Session tool configuration onto the Managed union.
pub fn managed_tools(
    configuration: &awaken_session_contract::SessionToolConfiguration,
) -> Vec<AgentTool> {
    let mut tools = resolved_toolsets(&configuration.toolsets);
    tools.extend(configuration.client_tools.iter().map(|tool| {
        AgentTool::Custom {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: CustomToolInputSchema::from_value(tool.input_schema.clone())
                .expect("published client tool schemas are object schemas"),
        }
    }));
    tools
}

/// Resolve nullable toolset fields while preserving custom tool definitions.
pub fn resolved_tools(tools: &[AgentTool]) -> Vec<AgentTool> {
    managed_tools(&session_tool_configuration(tools))
}

/// Project the agent's tool surface onto the public `agent.tools` array. The built-in
/// tools fold into a single `agent_toolset_20260401` reference (never per-tool
/// definitions) with `configs` that disable the toolset tools the host does not
/// register and mark the confirmation-gated ones `always_ask`; each client tool
/// becomes a `custom` tool definition.
pub fn agent_tools(caps: &AgentCapabilities) -> Vec<AgentTool> {
    managed_tools(&awaken_session_contract::SessionToolConfiguration::from_capabilities(caps))
}

/// Project exact client-owned descriptors from a published Agent snapshot. This
/// path is separate from host capabilities because equal names must not transfer
/// execution ownership to the host registry.
pub fn agent_client_tools(
    tools: &[awaken_agent_contract::ClientToolDescriptor],
) -> Result<Vec<AgentTool>, String> {
    tools
        .iter()
        .map(|tool| {
            Ok(AgentTool::Custom {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: CustomToolInputSchema::from_value(tool.input_schema.clone())?,
            })
        })
        .collect()
}

/// Project the agent's offered skills onto the public `agent.skills` array. Each is a
/// `custom` skill reference (the host offers them locally, not from the Skills API).
pub fn agent_skills(caps: &AgentCapabilities) -> Vec<crate::types::agent::AgentSkill> {
    caps.skills
        .iter()
        .map(|id| crate::types::agent::AgentSkill::Custom {
            skill_id: id.clone(),
            version: Some("latest".into()),
        })
        .collect()
}

/// Project the agent's delegate roster onto the public `agent.multiagent` coordinator
/// object, or `None` when the agent delegates to no one.
pub fn agent_multiagent(caps: &AgentCapabilities) -> Option<crate::types::agent::MultiagentConfig> {
    agent_multiagent_ids(&caps.delegates)
}

/// Project an already-resolved published roster. Session creation uses this
/// before runtime preparation, while later reads use [`agent_multiagent`].
pub fn agent_multiagent_ids(
    delegate_ids: &[String],
) -> Option<crate::types::agent::MultiagentConfig> {
    if delegate_ids.is_empty() {
        return None;
    }
    Some(crate::types::agent::MultiagentConfig::Coordinator {
        agents: delegate_ids
            .iter()
            .cloned()
            .map(crate::types::agent::MultiagentRosterEntry::Id)
            .collect(),
    })
}

/// The public wire object for one recorded outcome evaluation on the session: the
/// outcome id and the verdict. This is the durable `outcome_evaluations` entry on the
/// session object, distinct from the transient `span.outcome_evaluation_*` events.
pub fn outcome_evaluation(round: &OutcomeIteration) -> crate::types::OutcomeEvaluation {
    let terminal = matches!(
        round.result.as_str(),
        "satisfied" | "max_iterations_reached" | "failed" | "interrupted"
    );
    crate::types::OutcomeEvaluation {
        completed_at: terminal.then(|| crate::state::PROCESSED_AT.to_string()),
        description: round.description.clone(),
        explanation: Some(round.explanation.clone()),
        iteration: round.iteration,
        outcome_id: round.outcome_id.clone(),
        result: round.result.clone(),
        kind: "outcome_evaluation",
    }
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
            // The turn reasoned: a contentless forward-progress marker, before the
            // answer (`BetaManagedAgentsAgentThinkingEvent` carries no content).
            Fact::AssistantThinking => {
                vec![ProjectedEvent::minted(OutboundKind::AgentThinking {})]
            }
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
            Fact::Awaiting {
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
/// `pending` is the exact tool the run awaits.
pub fn project_messages(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
) -> Vec<ProjectedEvent> {
    project_messages_with_mcp_ids(
        messages,
        pending,
        &std::collections::HashSet::new(),
        std::iter::empty(),
    )
}

/// Project a committed transcript delta while retaining MCP call identities
/// learned from the already-projected prefix. Active-active cache refresh uses
/// this same encoder path; it does not invent a second recovery transcoder.
pub(crate) fn project_messages_with_mcp_ids(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
    mcp_ids: impl IntoIterator<Item = String>,
) -> Vec<ProjectedEvent> {
    let mut encoder = ManagedEncoder::default();
    encoder.mcp_ids.extend(mcp_ids);
    encoder.transcode_facts(&fold_with_pending(messages, pending, projected_tool_ids))
}

fn fold_with_pending(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
) -> Vec<Fact> {
    let mut facts = fold(
        messages,
        pending.map(|pending| (pending.tool_use_id.as_str(), pending.client_executed)),
    );
    if let Some(pending) = pending
        && !projected_tool_ids.contains(&pending.tool_use_id)
        && !facts
            .iter()
            .any(|fact| matches!(fact, Fact::ToolCall { id, .. } if id == &pending.tool_use_id))
    {
        facts.push(Fact::ToolCall {
            id: pending.tool_use_id.clone(),
            name: pending.name.clone(),
            input: pending.input.clone(),
            disposition: if pending.client_executed {
                ToolDisposition::PendingClient
            } else {
                ToolDisposition::PendingBuiltin
            },
        });
    }
    facts
}

/// Project the messages committed during one step, then a terminal
/// `session.status_idle` derived from `stop`. When `stop` is `RequiresAction` the
/// pending tool's id populates `requires_action.event_ids`.
pub(crate) fn project_step(
    messages: &[awaken_agent_contract::agent::message::Message],
    state: &awaken_agent_contract::agent::run::RunState,
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
    mcp_ids: impl IntoIterator<Item = String>,
) -> Vec<ProjectedEvent> {
    let mut events = fold_with_pending(messages, pending, projected_tool_ids);
    events.push(terminal_event(state, pending));
    let mut encoder = ManagedEncoder::default();
    encoder.mcp_ids.extend(mcp_ids);
    encoder.transcode_facts(&events)
}

/// Project the run's sole lifecycle authority to the Managed terminal fact.
fn terminal_event(
    state: &awaken_agent_contract::agent::run::RunState,
    pending: Option<&Pending>,
) -> Fact {
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    match state {
        RunState::Awaiting => {
            terminal_awaiting(pending.map(|pending| pending.tool_use_id.as_str()))
        }
        RunState::Ended(EndCause::MaxSteps | EndCause::Error(_)) => {
            Fact::RunFinished { exhausted: true }
        }
        RunState::Ended(_) => Fact::RunFinished { exhausted: false },
        RunState::Running => unreachable!("StepOutcome cannot contain Running"),
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

    /// A continuation-guard round (steering) is an audit lifecycle fact
    /// (`classify().live == false`) with no agent-visible managed wire event — the
    /// managed `OutboundKind` union is conformance-locked to the official SDK set,
    /// which has no steering event. Steering stays observable via the audit
    /// projection (`RunEvent::Continuation`). This pins the omission so a future
    /// edit can't leak a guard round onto the managed stream (and silently break
    /// the SDK-parity golden gate).
    #[test]
    fn continuation_steering_is_not_projected() {
        for steered in [false, true] {
            let out = ManagedEncoder::default().fact(&Fact::Continuation {
                steered,
                detail: serde_json::json!({ "reason": "auto_continue" }),
            });
            assert!(
                out.is_empty(),
                "steered={steered} projected {} events",
                out.len()
            );
        }
    }

    #[test]
    fn transcript_external_pending_tool_projects_an_answerable_event() {
        // Cause/effect graph: C1 pending id has/has-not a transcript ToolUse;
        // C2 pending is client/built-in executed. E1 reuse the transcript event;
        // E2 synthesize one event with the exact pending id; E3 choose custom vs
        // built-in wire kind; E4 requires_action references that same id.
        // Decision rules exercised here: R1 !C1+client -> E2(custom)+E4;
        // R2 !C1+built-in -> E2(tool_use/ask)+E4. Existing adapter projection
        // tests own C1 -> E1 and prove the synthesis guard prevents duplication.
        for (client_executed, expected_type) in
            [(true, "agent.custom_tool_use"), (false, "agent.tool_use")]
        {
            let pending = Pending {
                tool_use_id: "remote-input".into(),
                name: "agent_input".into(),
                input: serde_json::json!({ "reason": "user_input" }),
                client_executed,
            };
            let projected = project_step(
                &[],
                &awaken_agent_contract::agent::run::RunState::Awaiting,
                Some(&pending),
                &std::collections::HashSet::new(),
                std::iter::empty(),
            );
            assert_eq!(projected.len(), 2);
            assert_eq!(projected[0].id.as_deref(), Some("remote-input"));
            assert_eq!(projected[0].kind.type_str(), expected_type, "E3");
            let OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
            } = &projected[1].kind
            else {
                panic!("E4 expected requires_action terminal")
            };
            assert_eq!(event_ids, &["remote-input"], "E4");
        }
    }
}
