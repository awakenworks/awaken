//! Project committed neutral messages into public Managed Agents events.
//!
//! The message fold is shared (`awaken_agent_contract::event`); this module
//! only owns the Managed *transcoder* — the `Fact -> OutboundKind` mapping
//! — and the terminal `session.status_idle` shape. An assistant Step becomes an
//! `agent.message` plus an `agent.tool_use` (or `agent.custom_tool_use`) per call;
//! a tool message becomes an `agent.tool_result`; a step ends with
//! `session.status_idle`. A pending client tool projects as `agent.custom_tool_use`;
//! a pending built-in tool as `agent.tool_use{ask}`; anything else ran
//! (`agent.tool_use{allow}`).

use awaken_agent_contract::event::{Fact, ToolDisposition, Transcoder, fold_messages as fold};
use awaken_session_contract::Pending;

use crate::state::{AgentCapabilities, CustomTool, OutcomeIteration};
use crate::types::{EvaluatedPermission, OutboundKind, StopReason};

/// Versioned, length-delimited public identity for a Managed tool-use event.
/// Runtime/provider call ids are only batch-local; qualifying them by logical
/// Thread and committed source message prevents owner/pagination collisions and
/// remains exactly reconstructible after cache loss. A synthetic pending call
/// uses its durable Run id as `source_id`.
const MANAGED_TOOL_EVENT_ID_PREFIX: &str = "mtool_v1_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedToolEventIdentity<'a> {
    pub(crate) thread_id: &'a str,
    pub(crate) source_id: &'a str,
    pub(crate) call_id: &'a str,
}

#[must_use]
pub(crate) fn managed_tool_event_id(thread_id: &str, source_id: &str, call_id: &str) -> String {
    format!(
        "{MANAGED_TOOL_EVENT_ID_PREFIX}{}:{}:{thread_id}{source_id}{call_id}",
        thread_id.len(),
        source_id.len()
    )
}

#[must_use]
pub(crate) fn decode_managed_tool_event_id(id: &str) -> Option<ManagedToolEventIdentity<'_>> {
    let encoded = id.strip_prefix(MANAGED_TOOL_EVENT_ID_PREFIX)?;
    let (thread_len, encoded) = encoded.split_once(':')?;
    let (source_len, payload) = encoded.split_once(':')?;
    let thread_len = thread_len.parse::<usize>().ok()?;
    let source_len = source_len.parse::<usize>().ok()?;
    let source_end = thread_len.checked_add(source_len)?;
    Some(ManagedToolEventIdentity {
        thread_id: payload.get(..thread_len)?,
        source_id: payload.get(thread_len..source_end)?,
        call_id: payload.get(source_end..)?,
    })
}

/// Reserved MCP tool-name prefix — a custom tool may not claim it.
const MCP_RESERVED_PREFIX: &str = awaken_agent_contract::MCP_TOOL_ID_PREFIX;
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
    let agents = delegate_ids
        .iter()
        .cloned()
        .map(crate::types::agent::MultiagentRosterEntry::Id)
        .collect::<Vec<_>>();
    Some(crate::types::agent::MultiagentConfig::Coordinator { agents })
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
        completed_at: terminal.then(|| crate::state::LEGACY_UNRECORDED_AT.to_string()),
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
    /// The committed tool call this event came from, when it came from one.
    ///
    /// `id` starts out holding the same value and is then rewritten into the
    /// public event id, so a later stage that needs the call identity cannot
    /// read `id` without depending on running before that rewrite. Carrying the
    /// call id under its own name removes that positional coupling: moving the
    /// rewrite earlier can no longer make a lookup silently stop matching — and
    /// a silent miss here would report a refused call as permitted.
    pub source_tool_call_id: Option<String>,
}

impl ProjectedEvent {
    fn minted(kind: OutboundKind) -> Self {
        Self {
            id: None,
            kind,
            source_tool_call_id: None,
        }
    }
    /// A projection of one committed tool call, retaining that call's identity.
    fn for_tool_call(call_id: String, kind: OutboundKind) -> Self {
        Self {
            id: Some(call_id.clone()),
            kind,
            source_tool_call_id: Some(call_id),
        }
    }
}

/// The MCP server behind an `mcp__<server>__<tool>` name, in the operator's own
/// spelling (`""` when the name carries no recoverable MCP identity).
///
/// The id is decomposed by its owning type, so the server reported on the wire
/// is the name the operator configured rather than its encoded segment.
fn mcp_server_name(name: &str) -> String {
    awaken_agent_contract::McpToolId::parse(name)
        .map(|id| id.server_name())
        .unwrap_or_default()
}

/// The Managed Agents transcoder: neutral projection events to public
/// `OutboundKind`. `RunStarted` is dropped (Managed has no per-step start event);
/// terminal events become `session.status_idle`. Stateful within one Run: it
/// remembers `mcp__` tool-use ids so their results project as MCP results too.
/// The permission a committed transcript can establish on its own.
///
/// A call still awaiting a decision is evidence: the committed ticket says a
/// person was asked. A call with a result is not — a refused call and an
/// executed call whose tool errored look identical here, so this returns `None`
/// and the recorded gate decision fills it in. Inferring `allow` from "a result
/// exists" is what reported refused calls as permitted, and an unrecognized
/// decision label would have fallen straight back into that inference.
fn awaited_permission(disposition: &ToolDisposition) -> Option<EvaluatedPermission> {
    match disposition {
        ToolDisposition::PendingBuiltin => Some(EvaluatedPermission::Ask),
        ToolDisposition::PendingClient | ToolDisposition::Executed => None,
    }
}

#[derive(Default)]
pub struct ManagedEncoder {
    mcp_ids: std::collections::HashSet<String>,
}

impl Transcoder for ManagedEncoder {
    type Output = ProjectedEvent;

    fn fact(&mut self, event: &Fact) -> Vec<ProjectedEvent> {
        match event {
            Fact::RunStarted => Vec::new(),
            // The Step reasoned: a contentless forward-progress marker, before the
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
            } if awaken_agent_contract::McpToolId::claims_namespace(name) => {
                self.mcp_ids.insert(id.clone());
                let evaluated_permission = awaited_permission(disposition);
                vec![ProjectedEvent::for_tool_call(
                    id.clone(),
                    OutboundKind::AgentMcpToolUse {
                        name: name.clone(),
                        mcp_server_name: mcp_server_name(name),
                        input: input.clone(),
                        evaluated_permission,
                        // Which policy asked is not in the transcript either;
                        // the recorded gate decision fills it in.
                        evaluation: None,
                        session_thread_id: None,
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
                        session_thread_id: None,
                    },
                    ToolDisposition::PendingBuiltin | ToolDisposition::Executed => {
                        OutboundKind::AgentToolUse {
                            name: name.clone(),
                            input: input.clone(),
                            evaluated_permission: awaited_permission(disposition),
                            evaluation: None,
                            session_thread_id: None,
                        }
                    }
                };
                vec![ProjectedEvent::for_tool_call(id.clone(), kind)]
            }
            Fact::ToolResult {
                id,
                content,
                is_error,
            } if self.mcp_ids.contains(id) => {
                vec![ProjectedEvent::minted(OutboundKind::AgentMcpToolResult {
                    mcp_tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: Some(*is_error),
                })]
            }
            Fact::ToolResult {
                id,
                content,
                is_error,
            } => {
                vec![ProjectedEvent::minted(OutboundKind::AgentToolResult {
                    tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: Some(*is_error),
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
/// terminal `session.status_idle`). Used by both a Run and an outcome iteration.
/// `pending` is the exact tool the run awaits.
pub fn project_messages(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
) -> Vec<ProjectedEvent> {
    let advisor_blocks = advisor_consultation_blocks(messages);
    project_messages_with_mcp_ids(
        messages,
        pending,
        &std::collections::HashSet::new(),
        std::iter::empty(),
        &advisor_blocks,
        &[],
    )
}

/// Rebuild the exact Advisor ToolUse/ToolResult block coordinates from committed
/// transcript truth. Provider call ids are only batch-local, so the ordered fold
/// pairs each result with the corresponding call occurrence rather than globally
/// hiding every result that reuses the same raw id. A warm refresh may observe
/// the call before its result; classification therefore uses the full committed
/// prefix instead of a process-local hidden-call cursor.
#[derive(Default)]
pub(crate) struct AdvisorConsultationBlocks {
    calls: std::collections::HashSet<(String, usize)>,
    results: std::collections::HashSet<(String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeToolProjectionPhase {
    Processed,
    PendingClient,
    PendingBuiltin,
    Resolving,
    Unclassified,
}

/// Server admission effect derived from the same phase that owns transcript
/// visibility. The queue variant delegates to the existing durable Session
/// Event selector; it is not a second scheduler or pending registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeToolFollowupAdmission {
    Reject,
    Queue,
    Ready,
}

impl RuntimeToolProjectionPhase {
    pub(crate) const fn from_pending(client_executed: bool) -> Self {
        if client_executed {
            Self::PendingClient
        } else {
            Self::PendingBuiltin
        }
    }

    pub(crate) const fn from_retained_reply(processed: bool) -> Self {
        if processed {
            Self::Processed
        } else {
            Self::Resolving
        }
    }

    pub(crate) const fn is_pending(self) -> bool {
        matches!(self, Self::PendingClient | Self::PendingBuiltin)
    }

    pub(crate) const fn followup_admission(self) -> RuntimeToolFollowupAdmission {
        match self {
            Self::PendingClient | Self::PendingBuiltin | Self::Unclassified => {
                RuntimeToolFollowupAdmission::Reject
            }
            Self::Resolving => RuntimeToolFollowupAdmission::Queue,
            Self::Processed => RuntimeToolFollowupAdmission::Ready,
        }
    }

    const fn is_transcript_visible(self) -> bool {
        !matches!(self, Self::Unclassified)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CommittedToolOccurrence {
    message_id: String,
    block_ordinal: usize,
    call_id: String,
}

/// One read-only classification of a committed transcript prefix. This is the
/// shared root/child evidence seam: ToolResult pairs and the current ResumeTicket
/// classify exact ToolUse occurrences, while the existing Event log identifies
/// occurrences that were already emitted during an earlier partial refresh.
pub(crate) struct CommittedTranscriptEvidence {
    advisor_blocks: AdvisorConsultationBlocks,
    dispositions: std::collections::HashMap<CommittedToolOccurrence, RuntimeToolProjectionPhase>,
}

pub(crate) struct CommittedMessageProjection {
    pub(crate) message: awaken_agent_contract::agent::message::Message,
    pub(crate) fully_classified: bool,
    pub(crate) pending_occurrence: bool,
    pub(crate) retained_pending_occurrence: bool,
}

impl CommittedTranscriptEvidence {
    pub(crate) fn advisor_blocks(&self) -> &AdvisorConsultationBlocks {
        &self.advisor_blocks
    }

    /// Select only content whose committed disposition is known. Non-tool
    /// content stays in the source Message and is deduplicated by its existing
    /// stable public identity at append time. An unclassified ToolUse remains
    /// unconsumed; a previously emitted occurrence is derived from the existing
    /// source-qualified Event id rather than a second projection registry.
    pub(crate) fn project_message(
        &self,
        message: &awaken_agent_contract::agent::message::Message,
        projected_tool_sources: &std::collections::HashSet<(String, String)>,
        pending_source_id: Option<&str>,
        pending_history: &[&Pending],
    ) -> CommittedMessageProjection {
        use awaken_agent_contract::agent::content::ContentBlock;

        let mut projected = message.clone();
        let mut fully_classified = true;
        let mut pending_occurrence = false;
        let mut retained_pending_occurrence = false;
        projected.content = message
            .content
            .iter()
            .enumerate()
            .filter_map(|(block_ordinal, block)| {
                let ContentBlock::ToolUse { id, name, input } = block else {
                    return Some(block.clone());
                };
                if self
                    .advisor_blocks
                    .calls
                    .contains(&(message.id.0.clone(), block_ordinal))
                {
                    return Some(block.clone());
                }
                let key = CommittedToolOccurrence {
                    message_id: message.id.0.clone(),
                    block_ordinal,
                    call_id: id.clone(),
                };
                let mut disposition = self
                    .dispositions
                    .get(&key)
                    .copied()
                    .unwrap_or(RuntimeToolProjectionPhase::Unclassified);
                // A closed wait remains committed classification evidence even
                // when failure/cancellation removed its current ResumeTicket.
                // The caller has scoped history to this Thread/Run/message;
                // preserve unmatched sibling intents as Unclassified.
                if disposition == RuntimeToolProjectionPhase::Unclassified
                    && let Some(historical) = pending_history.iter().find(|pending| {
                        pending.tool_use_id == *id
                            && pending.name == *name
                            && pending.input == *input
                    })
                {
                    disposition =
                        RuntimeToolProjectionPhase::from_pending(historical.client_executed);
                }
                let is_pending = disposition.is_pending();
                pending_occurrence |= is_pending;
                let already_projected = projected_tool_sources
                    .contains(&(message.id.0.clone(), id.clone()))
                    || (is_pending
                        && pending_source_id.is_some_and(|source_id| {
                            projected_tool_sources.contains(&(source_id.to_string(), id.clone()))
                        }));
                if already_projected {
                    return None;
                }
                if !disposition.is_transcript_visible() {
                    fully_classified = false;
                    return None;
                }
                match disposition {
                    RuntimeToolProjectionPhase::PendingClient
                    | RuntimeToolProjectionPhase::PendingBuiltin => {
                        retained_pending_occurrence = true;
                        Some(block.clone())
                    }
                    RuntimeToolProjectionPhase::Processed
                    | RuntimeToolProjectionPhase::Resolving => Some(block.clone()),
                    RuntimeToolProjectionPhase::Unclassified => {
                        unreachable!("transcript visibility checked above")
                    }
                }
            })
            .collect();
        CommittedMessageProjection {
            message: projected,
            fully_classified,
            pending_occurrence,
            retained_pending_occurrence,
        }
    }
}

/// Fold the full committed prefix once into exact occurrence evidence. A result
/// consumes the earliest unmatched call with the same provider-local id; the
/// current ticket then classifies the earliest still-unmatched occurrence. This
/// ordered rule prevents raw-id reuse from turning an unrelated call into either
/// an executed or answerable Managed event.
pub(crate) fn committed_transcript_evidence(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
) -> CommittedTranscriptEvidence {
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_runtime_contract::resolved::ADVISOR_TOOL_ID;

    struct Occurrence {
        key: CommittedToolOccurrence,
        advisor: bool,
        classification: RuntimeToolProjectionPhase,
    }

    let mut occurrences = Vec::<Occurrence>::new();
    let mut unmatched =
        std::collections::HashMap::<String, std::collections::VecDeque<usize>>::new();
    let mut advisor_blocks = AdvisorConsultationBlocks::default();
    for message in messages {
        if message.id.is_agent_thread_report() {
            continue;
        }
        match message.role {
            Role::Assistant => {
                for (block_ordinal, block) in message.content.iter().enumerate() {
                    let ContentBlock::ToolUse { id, name, .. } = block else {
                        continue;
                    };
                    let advisor = name == ADVISOR_TOOL_ID;
                    if advisor {
                        advisor_blocks
                            .calls
                            .insert((message.id.0.clone(), block_ordinal));
                    }
                    let index = occurrences.len();
                    occurrences.push(Occurrence {
                        key: CommittedToolOccurrence {
                            message_id: message.id.0.clone(),
                            block_ordinal,
                            call_id: id.clone(),
                        },
                        advisor,
                        classification: RuntimeToolProjectionPhase::Unclassified,
                    });
                    unmatched.entry(id.clone()).or_default().push_back(index);
                }
            }
            Role::Tool => {
                for (block_ordinal, block) in message.content.iter().enumerate() {
                    let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                        continue;
                    };
                    let Some(index) = unmatched
                        .get_mut(tool_use_id)
                        .and_then(std::collections::VecDeque::pop_front)
                    else {
                        continue;
                    };
                    if occurrences[index].advisor {
                        advisor_blocks
                            .results
                            .insert((message.id.0.clone(), block_ordinal));
                    } else {
                        occurrences[index].classification = RuntimeToolProjectionPhase::Processed;
                    }
                }
            }
            Role::User | Role::System => {}
        }
    }
    if let Some(pending) = pending
        && let Some(index) = unmatched
            .get(&pending.tool_use_id)
            .into_iter()
            .flatten()
            .copied()
            .find(|index| !occurrences[*index].advisor)
    {
        occurrences[index].classification =
            RuntimeToolProjectionPhase::from_pending(pending.client_executed);
    }
    let dispositions = occurrences
        .into_iter()
        .filter(|occurrence| !occurrence.advisor)
        .map(|occurrence| (occurrence.key, occurrence.classification))
        .collect();
    CommittedTranscriptEvidence {
        advisor_blocks,
        dispositions,
    }
}

#[cfg(kani)]
#[kani::proof]
fn runtime_tool_projection_phase_is_total_distinct_and_fail_closed() {
    // Causes: C1 a current ticket is client/builtin; C2 an exact retained reply
    // exists; C3 that reply is processed. Effects: E1 !C2 stays Pending with
    // exact family; E2 C2+!C3 is Resolving; E3 C2+C3 is Processed; E4 missing
    // evidence is Unclassified, transcript-invisible, and rejected by server
    // admission. E5 Resolving durably queues behind the retained reply; E6
    // Processed is ready. All eight C1-C3 rows call the same production
    // constructors/predicates consumed by projection and admission; durable
    // identity matching, queue persistence and snapshot reads remain TCB.
    let client_executed: bool = kani::any();
    let retained: bool = kani::any();
    let processed: bool = kani::any();
    let phase = if retained {
        RuntimeToolProjectionPhase::from_retained_reply(processed)
    } else {
        RuntimeToolProjectionPhase::from_pending(client_executed)
    };

    assert_eq!(phase.is_pending(), !retained);
    assert_eq!(
        phase == RuntimeToolProjectionPhase::PendingClient,
        !retained && client_executed
    );
    assert_eq!(
        phase == RuntimeToolProjectionPhase::PendingBuiltin,
        !retained && !client_executed
    );
    assert_eq!(
        phase == RuntimeToolProjectionPhase::Resolving,
        retained && !processed
    );
    assert_eq!(
        phase == RuntimeToolProjectionPhase::Processed,
        retained && processed
    );
    assert!(phase.is_transcript_visible());
    assert!(!RuntimeToolProjectionPhase::Unclassified.is_transcript_visible());
    assert_eq!(
        phase.followup_admission(),
        if !retained {
            RuntimeToolFollowupAdmission::Reject
        } else if processed {
            RuntimeToolFollowupAdmission::Ready
        } else {
            RuntimeToolFollowupAdmission::Queue
        }
    );
    assert_eq!(
        RuntimeToolProjectionPhase::Unclassified.followup_admission(),
        RuntimeToolFollowupAdmission::Reject
    );
}

pub(crate) fn advisor_consultation_blocks(
    messages: &[awaken_agent_contract::agent::message::Message],
) -> AdvisorConsultationBlocks {
    committed_transcript_evidence(messages, None).advisor_blocks
}

/// Project a committed transcript delta while retaining MCP call identities
/// learned from the already-projected prefix. Active-active cache refresh uses
/// this same encoder path; it does not invent a second recovery transcoder.
pub(crate) fn project_messages_with_mcp_ids(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
    mcp_ids: impl IntoIterator<Item = String>,
    advisor_blocks: &AdvisorConsultationBlocks,
    pending_history: &[&Pending],
) -> Vec<ProjectedEvent> {
    let mut encoder = ManagedEncoder::default();
    encoder.mcp_ids.extend(mcp_ids);
    let mut facts = fold_with_pending(messages, pending, projected_tool_ids, advisor_blocks);
    // The transcript establishes occurrence visibility; committed AwaitTarget
    // history establishes its permission/ownership at that occurrence. Preserve
    // every prior ask/client call even when a cold read sees all results at once.
    for fact in &mut facts {
        if let Fact::ToolCall {
            id,
            name,
            input,
            disposition,
        } = fact
            && let Some(pending) = pending_history.iter().find(|pending| {
                pending.tool_use_id == *id && pending.name == *name && pending.input == *input
            })
        {
            *disposition = if pending.client_executed {
                ToolDisposition::PendingClient
            } else {
                ToolDisposition::PendingBuiltin
            };
        }
    }
    encoder.transcode_facts(&facts)
}

fn fold_with_pending(
    messages: &[awaken_agent_contract::agent::message::Message],
    pending: Option<&Pending>,
    projected_tool_ids: &std::collections::HashSet<String>,
    advisor_blocks: &AdvisorConsultationBlocks,
) -> Vec<Fact> {
    // Root continuations and Advisor consultations stay durable for Runtime
    // replay and Host reconstruction, but neither is an ordinary Managed
    // message/tool event. Filter both at this one message decoder instead of
    // teaching warm and cold callers separate suppression rules.
    let requires_filter = messages.iter().any(|message| {
        message.id.is_agent_thread_report()
            || message
                .content
                .iter()
                .enumerate()
                .any(|(ordinal, block)| match block {
                    awaken_agent_contract::agent::content::ContentBlock::ToolUse { .. } => {
                        advisor_blocks
                            .calls
                            .contains(&(message.id.0.clone(), ordinal))
                    }
                    awaken_agent_contract::agent::content::ContentBlock::ToolResult { .. } => {
                        advisor_blocks
                            .results
                            .contains(&(message.id.0.clone(), ordinal))
                    }
                    _ => false,
                })
    });
    let visible_messages;
    let messages = if requires_filter {
        visible_messages =
            messages
                .iter()
                .filter(|message| !message.id.is_agent_thread_report())
                .map(|message| {
                    let mut message = message.clone();
                    let message_id = message.id.0.clone();
                    message.content =
                        message
                            .content
                            .into_iter()
                            .enumerate()
                            .filter_map(|(ordinal, block)| {
                                match &block {
                        awaken_agent_contract::agent::content::ContentBlock::ToolUse { .. }
                            if advisor_blocks.calls.contains(&(message_id.clone(), ordinal)) =>
                        {
                            None
                        }
                        awaken_agent_contract::agent::content::ContentBlock::ToolResult {
                            ..
                        } if advisor_blocks
                            .results
                            .contains(&(message_id.clone(), ordinal)) => None,
                        _ => Some(block),
                    }
                            })
                            .collect();
                    message
                })
                .collect::<Vec<_>>();
        visible_messages.as_slice()
    } else {
        messages
    };
    let mut facts = fold(
        messages,
        pending.map(|pending| (pending.tool_use_id.as_str(), pending.client_executed)),
    );
    // Advisor is a Managed orchestration primitive, not an ordinary public
    // tool. Retain the Runtime's committed ToolUse/ToolResult for model replay
    // and Host link reconstruction, but remove the pair only at this Managed
    // transcoder. The call is recognized by its reserved descriptor identity;
    // the later result is correlated to its exact message block by an ordered
    // fold of the full committed prefix.
    if let Some(pending) = pending
        && pending.name != awaken_runtime_contract::resolved::ADVISOR_TOOL_ID
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

/// Project the Run's sole committed disposition to the one Managed terminal
/// classification used by foreground and cold lifecycle reconstruction.
pub(crate) fn terminal_stop_reason(
    state: &awaken_agent_contract::agent::run::RunState,
    pending: Option<&Pending>,
    await_reason: Option<&awaken_agent_contract::agent::awaiting::AwaitReason>,
    aggregate_budget_reached: bool,
) -> StopReason {
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    match state {
        RunState::Awaiting
            if await_reason
                == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached) =>
        {
            StopReason::BudgetReached
        }
        RunState::Awaiting => StopReason::RequiresAction {
            event_ids: pending
                .map(|pending| vec![pending.tool_use_id.clone()])
                .unwrap_or_default(),
        },
        RunState::Ended(EndCause::MaxSteps | EndCause::Error(_)) => StopReason::RetriesExhausted,
        RunState::Ended(EndCause::NaturalEnd) if aggregate_budget_reached => {
            StopReason::BudgetReached
        }
        RunState::Ended(_) => StopReason::EndTurn,
        RunState::Running => unreachable!("StepOutcome cannot contain Running"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_tool_projection_phase_decision_table() {
        // Causes: C1 current ticket is builtin/client; C2 an exact retained
        // reply exists; C3 its root entry is processed. Effects: P1 !C2 stays
        // Pending in the exact family and blocks follow-up; P2 C2+!C3 is
        // Resolving; P3 C2+C3 is Processed; P4 missing disposition evidence is
        // Unclassified, withheld and rejected. P5 Resolving queues through the
        // durable selector; P6 Processed is ready. Rules cover all C1-C3
        // combinations; the exact identity adapter and production queue effect
        // are exercised by Managed event recovery tests.
        for client_executed in [false, true] {
            for retained in [false, true] {
                for processed in [false, true] {
                    let phase = if retained {
                        RuntimeToolProjectionPhase::from_retained_reply(processed)
                    } else {
                        RuntimeToolProjectionPhase::from_pending(client_executed)
                    };
                    assert_eq!(phase.is_pending(), !retained, "P1-P3");
                    assert_eq!(
                        phase == RuntimeToolProjectionPhase::Resolving,
                        retained && !processed,
                        "P2"
                    );
                    assert_eq!(
                        phase == RuntimeToolProjectionPhase::Processed,
                        retained && processed,
                        "P3"
                    );
                    assert!(phase.is_transcript_visible(), "P1-P3");
                    assert_eq!(
                        phase.followup_admission(),
                        if !retained {
                            RuntimeToolFollowupAdmission::Reject
                        } else if processed {
                            RuntimeToolFollowupAdmission::Ready
                        } else {
                            RuntimeToolFollowupAdmission::Queue
                        },
                        "P1/P5/P6"
                    );
                }
            }
        }
        assert!(
            !RuntimeToolProjectionPhase::Unclassified.is_transcript_visible(),
            "P4"
        );
        assert_eq!(
            RuntimeToolProjectionPhase::Unclassified.followup_admission(),
            RuntimeToolFollowupAdmission::Reject,
            "P4"
        );
    }

    #[test]
    fn managed_tool_event_identity_is_stable_qualified_and_reversible() {
        // Causes: the fixtures below establish `managed tool event identity` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 root/two child Threads reuse one batch-local
        // call id; C2 message ids differ; C3 ids contain delimiter/non-ASCII
        // content; C4 malformed or unversioned input is decoded. Effects: E1 all
        // public ids are distinct; E2 each round-trips exact source/runtime ids;
        // E3 C4 fails closed. Decision rules: I1=C1+C2=>E1,E2;
        // I2=C3=>E2; I3=C4=>E3.
        let identities = [
            ("sthr_root", "msg:root", "same-call"),
            ("child-a", "消息:a", "same-call"),
            ("child-b", "msg:b", "same-call"),
        ];
        let encoded = identities
            .iter()
            .map(|(thread, source, call)| managed_tool_event_id(thread, source, call))
            .collect::<Vec<_>>();
        assert_eq!(
            encoded
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            identities.len(),
            "I1/E1"
        );
        for ((thread, source, call), encoded) in identities.iter().zip(encoded.iter()) {
            let decoded = decode_managed_tool_event_id(encoded).expect("I1-I2/E2");
            assert_eq!(decoded.thread_id, *thread, "I1-I2/E2");
            assert_eq!(decoded.source_id, *source, "I1-I2/E2");
            assert_eq!(decoded.call_id, *call, "I1-I2/E2");
        }
        for malformed in ["same-call", "mtool_v1_x:1:a", "mtool_v1_9:9:short"] {
            assert!(
                decode_managed_tool_event_id(malformed).is_none(),
                "I3/E3: {malformed}"
            );
        }
    }
    use awaken_agent_contract::agent::content::ContentBlock;

    #[test]
    fn internal_child_report_message_is_not_a_managed_user_event() {
        // Causes: the fixtures below establish `internal child report message` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 settlement commits the typed internal child
        // report User message; C2 an ordinary assistant continuation follows.
        // Effects: E1 C1 produces no public event; E2 C2 remains visible.
        // Decision rules: R1=C1=>E1 and R2=C1+C2=>E1+E2. The classifier is the
        // MessageId family, so arbitrary report text cannot alter the result.
        use awaken_agent_contract::agent::message::{Id, Message, Role};
        use awaken_agent_contract::agent::run::Id as RunId;

        let messages = vec![
            Message::text(
                Id::agent_thread_report(&RunId("child-run".into())),
                Role::User,
                "arbitrary internal envelope",
            ),
            Message::text(Id("assistant".into()), Role::Assistant, "continued"),
        ];
        let projected = project_messages(&messages, None);
        assert_eq!(projected.len(), 1, "R1-R2/E1-E2");
        assert!(
            matches!(
                &projected[0].kind,
                OutboundKind::AgentMessage { content }
                    if content == &vec![ContentBlock::text("continued")]
            ),
            "R2/E2"
        );
    }

    #[test]
    fn advisor_consultation_pair_is_hidden_while_ordinary_sibling_tools_remain_visible() {
        // Causes: the fixtures below establish `advisor consultation pair` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 an assistant commits the reserved Advisor call;
        // C2 its ToolResult is committed in the same refresh or a later delta;
        // C3 the batch also contains an ordinary tool call/result; C4 a later
        // ordinary batch reuses the Advisor call's provider-local raw id.
        // Effects: E1 C1 and its paired C2 emit no ordinary Managed tool events;
        // E2 C3 keeps its exact tool-use/result pair; E3 delayed C2 is still
        // classified from the full committed prefix without a process-local
        // hidden-call cursor; E4 C4 remains visible rather than being suppressed
        // by a global raw-id set.
        // Decision table:
        // | Rule | C1 | C2 timing | C3 | C4 | Effects    |
        // | A1   | T  | absent    | T  | F  | E1,E2      |
        // | A2   | T  | later     | T  | F  | E1,E2,E3   |
        // | A3   | T  | committed | T  | T  | E1,E2,E4   |
        use awaken_agent_contract::agent::message::{Id, Message, Role};
        use awaken_runtime_contract::resolved::ADVISOR_TOOL_ID;

        let assistant = Message::new(
            Id("advisor-and-sibling-calls".into()),
            Role::Assistant,
            vec![
                ContentBlock::tool_use("advisor-call", ADVISOR_TOOL_ID, serde_json::json!({})),
                ContentBlock::tool_use(
                    "ordinary-call",
                    "ordinary_tool",
                    serde_json::json!({"input":"visible"}),
                ),
            ],
        );
        let results = Message::new(
            Id("advisor-and-sibling-results".into()),
            Role::Tool,
            vec![
                ContentBlock::tool_result(
                    "advisor-call",
                    vec![ContentBlock::text("private advice")],
                ),
                ContentBlock::tool_result(
                    "ordinary-call",
                    vec![ContentBlock::text("visible result")],
                ),
            ],
        );
        let reused_call = Message::new(
            Id("ordinary-reused-call".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "advisor-call",
                "ordinary_tool",
                serde_json::json!({"input":"reused"}),
            )],
        );
        let reused_result = Message::new(
            Id("ordinary-reused-result".into()),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "advisor-call",
                vec![ContentBlock::text("reused result")],
            )],
        );
        let committed_prefix = vec![
            assistant.clone(),
            results.clone(),
            reused_call.clone(),
            reused_result.clone(),
        ];
        let advisor_blocks = advisor_consultation_blocks(&committed_prefix);

        let call_events = project_messages_with_mcp_ids(
            &[assistant],
            None,
            &std::collections::HashSet::new(),
            std::iter::empty(),
            &advisor_blocks,
            &[],
        );
        assert_eq!(call_events.len(), 1, "A1/E1-E2");
        assert!(matches!(
            &call_events[0].kind,
            OutboundKind::AgentToolUse { name, .. } if name == "ordinary_tool"
        ));

        let result_events = project_messages_with_mcp_ids(
            &[results],
            None,
            &std::collections::HashSet::from(["ordinary-call".into()]),
            std::iter::empty(),
            &advisor_blocks,
            &[],
        );
        assert_eq!(result_events.len(), 1, "A2/E1-E3");
        assert!(matches!(
            &result_events[0].kind,
            OutboundKind::AgentToolResult { tool_use_id, content, .. }
                if tool_use_id == "ordinary-call"
                    && content == &vec![ContentBlock::text("visible result")]
        ));

        let reused_call_events = project_messages_with_mcp_ids(
            &[reused_call],
            None,
            &std::collections::HashSet::new(),
            std::iter::empty(),
            &advisor_blocks,
            &[],
        );
        let reused_result_events = project_messages_with_mcp_ids(
            &[reused_result],
            None,
            &std::collections::HashSet::from(["advisor-call".into()]),
            std::iter::empty(),
            &advisor_blocks,
            &[],
        );
        assert!(
            matches!(
                &reused_call_events[..],
                [ProjectedEvent {
                    kind: OutboundKind::AgentToolUse { name, .. },
                    ..
                }] if name == "ordinary_tool"
            ),
            "A3/E4 call"
        );
        assert!(
            matches!(
                &reused_result_events[..],
                [ProjectedEvent {
                    kind: OutboundKind::AgentToolResult { content, .. },
                    ..
                }] if content == &vec![ContentBlock::text("reused result")]
            ),
            "A3/E4 result"
        );
    }

    #[test]
    fn committed_tool_evidence_is_fifo_occurrence_scoped_and_revisitable() {
        // Cause/effect graph: C1 two Assistant Messages reuse one provider-local
        // call id; C2 the first occurrence has a matching ToolResult; C3 the
        // second occurrence owns the current built-in ResumeTicket; C4 a sibling
        // ToolUse has neither a result nor ticket; C5 the sibling results later;
        // C6 the current occurrence was already emitted from its source-qualified
        // id. Effects: E1 C2 is Resolved, E2 C3 is PendingBuiltin, E3 C4 is
        // withheld and keeps its source Message revisitable, E4 C6 emits neither
        // duplicate call nor duplicate non-tool identity at append, and E5 after
        // C5 every occurrence is classified so the Message may be consumed.
        // K1 Result and ResumeTicket are the only disposition authorities; K2
        // raw ids never identify an occurrence; K3 this pure evidence fold is
        // shared by root and child and owns no cursor or stored state.
        //
        // | Rule | Prior result | Current ticket | Sibling result | Prior Event | Effect |
        // |---|---|---|---|---|---|
        // | F1 | yes | exact | no | no | E1,E2,E3 |
        // | F2 | yes | exact | no | exact current | E1,E3,E4 |
        // | F3 | yes | none | yes for both current calls | no | E1,E5 |
        use awaken_agent_contract::agent::message::{Id, Message, Role};

        let first = Message::new(
            Id("fifo-first-call".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "same",
                "write",
                serde_json::json!({"path":"first"}),
            )],
        );
        let first_result = Message::new(
            Id("fifo-first-result".into()),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "same",
                vec![ContentBlock::text("first done")],
            )],
        );
        let current = Message::new(
            Id("fifo-current-calls".into()),
            Role::Assistant,
            vec![
                ContentBlock::text("current batch"),
                ContentBlock::tool_use("same", "write", serde_json::json!({"path":"second"})),
                ContentBlock::tool_use("other", "write", serde_json::json!({"path":"third"})),
            ],
        );
        let pending = Pending {
            tool_use_id: "same".into(),
            name: "write".into(),
            input: serde_json::json!({"path":"second"}),
            client_executed: false,
        };
        let prefix = vec![first, first_result, current.clone()];
        let evidence = committed_transcript_evidence(&prefix, Some(&pending));
        let selected =
            evidence.project_message(&current, &std::collections::HashSet::new(), None, &[]);
        assert!(!selected.fully_classified, "F1/E3");
        assert!(selected.pending_occurrence, "F1/E2");
        assert!(selected.retained_pending_occurrence, "F1/E2");
        assert!(
            matches!(
                &selected.message.content[..],
                [
                    ContentBlock::Text { .. },
                    ContentBlock::ToolUse { id, .. }
                ] if id == "same"
            ),
            "F1/E1-E3 exact occurrence selection"
        );

        let selected = evidence.project_message(
            &current,
            &std::collections::HashSet::from([(current.id.0.clone(), "same".to_string())]),
            None,
            &[],
        );
        assert!(!selected.fully_classified, "F2/E3");
        assert!(selected.pending_occurrence, "F2/E4 source remains known");
        assert!(!selected.retained_pending_occurrence, "F2/E4");
        assert!(
            matches!(&selected.message.content[..], [ContentBlock::Text { .. }]),
            "F2/E4 only stable non-tool content is revisited"
        );

        let completed = [
            prefix,
            vec![Message::new(
                Id("fifo-current-results".into()),
                Role::Tool,
                vec![
                    ContentBlock::tool_result("same", vec![ContentBlock::text("second done")]),
                    ContentBlock::tool_result("other", vec![ContentBlock::text("third done")]),
                ],
            )],
        ]
        .concat();
        let evidence = committed_transcript_evidence(&completed, None);
        let selected =
            evidence.project_message(&current, &std::collections::HashSet::new(), None, &[]);
        assert!(selected.fully_classified, "F3/E5");
        assert_eq!(
            selected
                .message
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                .count(),
            2,
            "F3/E1,E5 both exact resolved occurrences"
        );
    }

    #[test]
    fn cold_permission_history_classifies_every_call_in_one_response() {
        // Graph: C1 a cold read sees two calls and both results together; C2
        // each call has a committed permission/client boundary; C3 the history
        // arrives in either order; C4 history is absent or has a different input.
        // H1 C1+C2+C3 -> both exact ask/custom events; H2 C4 -> no transferred
        // permission. Cartesian H1 covers builtin/MCP/client x history order.
        // Current ticket is absent: completed calls must retain their original
        // wire ownership, without creating a new pending Session or state store.
        use awaken_agent_contract::agent::message::{Id, Message, Role};
        for (name, client) in [
            ("write", false),
            ("mcp__files__write", false),
            ("custom", true),
        ] {
            let history = ["first", "second"].map(|id| Pending {
                tool_use_id: id.into(),
                name: name.into(),
                input: serde_json::json!({"path":id}),
                client_executed: client,
            });
            let messages = vec![
                Message::new(
                    Id("calls".into()),
                    Role::Assistant,
                    history
                        .iter()
                        .map(|p| ContentBlock::tool_use(&p.tool_use_id, name, p.input.clone()))
                        .collect(),
                ),
                Message::new(
                    Id("results".into()),
                    Role::Tool,
                    history
                        .iter()
                        .map(|p| {
                            ContentBlock::tool_result(
                                &p.tool_use_id,
                                vec![ContentBlock::text("done")],
                            )
                        })
                        .collect(),
                ),
            ];
            for order in [[&history[0], &history[1]], [&history[1], &history[0]]] {
                let projected = project_messages_with_mcp_ids(
                    &messages,
                    None,
                    &std::collections::HashSet::new(),
                    std::iter::empty(),
                    &advisor_consultation_blocks(&messages),
                    &order,
                );
                assert_eq!(
                    projected
                        .iter()
                        .filter(|event| matches!(
                            event.kind,
                            OutboundKind::AgentToolUse {
                                evaluated_permission: Some(EvaluatedPermission::Ask),
                                ..
                            } | OutboundKind::AgentMcpToolUse {
                                evaluated_permission: Some(EvaluatedPermission::Ask),
                                ..
                            } | OutboundKind::AgentCustomToolUse { .. }
                        ))
                        .count(),
                    2,
                    "H1 {name}"
                );
            }
            let mut unrelated = history[0].clone();
            unrelated.input = serde_json::json!({"path":"elsewhere"});
            let projected = project_messages_with_mcp_ids(
                &messages,
                None,
                &std::collections::HashSet::new(),
                std::iter::empty(),
                &advisor_consultation_blocks(&messages),
                &[&unrelated],
            );
            // H2 states the property directly: an unrelated pending record must
            // not lend its `ask` to these calls. It no longer asserts `allow`,
            // because a committed transcript cannot establish that a call was
            // permitted — only the recorded gate decision can, and inferring it
            // from the presence of a result is what reported refused calls as
            // permitted. "Nothing established here" is now `None`.
            assert_eq!(
                projected
                    .iter()
                    .filter(|event| matches!(
                        event.kind,
                        OutboundKind::AgentToolUse {
                            evaluated_permission: None,
                            ..
                        } | OutboundKind::AgentMcpToolUse {
                            evaluated_permission: None,
                            ..
                        }
                    ))
                    .count(),
                2,
                "H2 no permission transfer {name}"
            );
            assert!(
                !projected.iter().any(|event| matches!(
                    event.kind,
                    OutboundKind::AgentToolUse {
                        evaluated_permission: Some(EvaluatedPermission::Ask),
                        ..
                    } | OutboundKind::AgentMcpToolUse {
                        evaluated_permission: Some(EvaluatedPermission::Ask),
                        ..
                    }
                )),
                "H2 an unrelated pending record never transfers its ask {name}"
            );
        }
    }

    #[test]
    fn managed_tool_results_preserve_success_and_error() {
        // Cause/effect decision table: C1 ordinary/MCP identity; C2
        // success/error. Every C1×C2 row emits the matching Managed event and
        // exact boolean E1. FMECA: `null` erases Runtime failure and lets a BFF
        // project an unsuccessful side effect as a completed application result.
        let facts = [
            Fact::ToolResult {
                id: "ordinary".into(),
                content: vec![ContentBlock::text("ok")],
                is_error: false,
            },
            Fact::ToolResult {
                id: "mcp".into(),
                content: vec![ContentBlock::text("failed")],
                is_error: true,
            },
        ];
        let mut encoder = ManagedEncoder::default();
        encoder.mcp_ids.insert("mcp".into());
        let projected = encoder.transcode_facts(&facts);
        assert!(matches!(
            &projected[0].kind,
            OutboundKind::AgentToolResult {
                is_error: Some(false),
                ..
            }
        ));
        assert!(matches!(
            &projected[1].kind,
            OutboundKind::AgentMcpToolResult {
                is_error: Some(true),
                ..
            }
        ));
    }

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
}
