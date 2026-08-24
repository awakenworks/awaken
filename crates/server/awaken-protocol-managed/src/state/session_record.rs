//! Disposable per-process projection of one durable Session aggregate.

use std::collections::{HashMap, HashSet};

use super::*;

pub(super) struct ProjectedToolIndex {
    pub(super) all: HashSet<String>,
    pub(super) mcp: Vec<String>,
    pub(super) latest: HashMap<String, String>,
    /// Exact committed source occurrence of every qualified tool-use Event.
    /// Rebuilt from the append-only Event log; never persisted as another
    /// projection cursor or tool authority.
    pub(super) sources: HashSet<(String, String)>,
}

pub(super) struct SessionRecord {
    pub(super) agent_id: String,
    pub(super) session: Session,
    /// Durable source of truth for the runtime's currently applied input projection.
    pub(super) resource_state: awaken_session_contract::SessionResourceState,
    pub(super) events: Vec<Event>,
    /// Process-local Session update events waiting for the durable command that
    /// preceded their root CAS to acquire its immutable projection anchor.
    /// Keeping them outside `events` prevents a PATCH from becoming visible
    /// before an earlier accepted input, while the Session root remains the
    /// only durable ordering authority.
    pub(super) pending_transient_events: Vec<(Event, String)>,
    /// Disposable predecessor hints for visible `evt_N` overlays. The key is a
    /// process-local event id and the value is the stable durable event id after
    /// which it was committed. These hints never cross replicas and therefore
    /// cannot become another Managed event source of truth.
    pub(super) transient_event_anchors: HashMap<String, String>,
    /// Runtime message identities already lowered into `events`. The transcript
    /// is durable authority; this set only prevents a peer refresh and the local
    /// request finisher from projecting the same committed message twice.
    pub(super) projected_thread_message_ids: HashSet<(String, String)>,
    /// Latest ordinary Run identity observed for each derived child Thread. This
    /// lets the local primary-step projection detect a newly committed follow-up
    /// before the warm projector has refreshed the disposable Thread cache.
    pub(super) projected_child_latest_run_ids:
        std::collections::HashMap<String, awaken_agent_contract::agent::run::Id>,
    /// The optional root Run owner and reason whose aggregate idle event is
    /// waiting for coordinated Thread settlement. The owner lets a later terminal
    /// of that same Run supersede an earlier Awaiting reason without allowing an
    /// unrelated terminal to overwrite it. This is disposable event-projector
    /// continuation state, never Session lifecycle authority.
    pub(super) deferred_session_stop_reason:
        Option<(Option<awaken_agent_contract::agent::run::Id>, StopReason)>,
    /// Disposable visibility owner for transcript-derived events. Most entries
    /// name the child whose local stream owns the event; advisor advice names the
    /// primary because the official wire excludes that delivery from the
    /// advisor Thread stream. The event itself remains in the one `events` log.
    pub(super) event_thread_owners: HashMap<String, String>,
    /// Last committed Run lifecycle fact consumed by this disposable projection.
    pub(super) projected_lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor,
    /// Exact committed lifecycle positions already lowered locally or from the
    /// feed. A Run may await and resume repeatedly, so Run id alone is not an
    /// idempotency key; each terminal transition owns a distinct cursor.
    pub(super) projected_terminal_cursors: HashSet<awaken_agent_contract::RunLifecycleCursor>,
    /// Latest append-only shared-budget transition lowered into Managed events.
    /// This is a disposable projection cursor over `SessionBudgetState`, never
    /// a second accounting or terminal-state authority.
    pub(super) projected_budget_reach_generation: u64,
    /// Subagent child threads spawned in this Session. Each is announced by a
    /// `session.thread_created` event and remains a projection of durable truth.
    pub(super) child_threads: Vec<SessionThread>,
    /// Cumulative usage for the primary logical Thread, read from the same
    /// parent Session commit partition as child usage. This is a disposable wire
    /// projection; committed ThreadUsage remains the sole accounting authority.
    pub(super) primary_thread_usage: Option<crate::types::SessionThreadUsage>,
}

impl SessionRecord {
    /// Derive the exact tool-use Events named by a primary-visible
    /// `requires_action` boundary. The append-only Event vector remains the
    /// only projection truth; this set exists only for one list/live filtering
    /// pass and is never persisted as a parallel index.
    pub(super) fn primary_answerable_event_ids(&self) -> HashSet<&str> {
        self.events
            .iter()
            .filter(|event| {
                self.event_thread_owners
                    .get(&event.id)
                    .is_none_or(|owner| owner == &self.session.id)
            })
            .filter_map(|event| match &event.kind {
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                }
                | OutboundKind::SessionThreadStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                    ..
                } => Some(event_ids.as_slice()),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// One owner-aware index over the sole event projection. All tool
    /// classification, MCP correlation, and public-id lookup consume this same
    /// scan so root/child and legacy-qualified ids cannot drift.
    pub(super) fn projected_tool_index_for(
        &self,
        owner_thread_id: Option<&str>,
    ) -> ProjectedToolIndex {
        let mut all = HashSet::new();
        let mut mcp = Vec::new();
        let mut latest = HashMap::new();
        let mut sources = HashSet::new();
        for event in &self.events {
            if self.event_thread_owners.get(&event.id).map(String::as_str) != owner_thread_id {
                continue;
            }
            let runtime_call_id = crate::project::decode_managed_tool_event_id(&event.id)
                .map_or(event.id.as_str(), |identity| identity.call_id);
            if let Some(identity) = crate::project::decode_managed_tool_event_id(&event.id) {
                sources.insert((identity.source_id.to_string(), identity.call_id.to_string()));
            }
            match event.kind {
                OutboundKind::AgentToolUse { .. } | OutboundKind::AgentCustomToolUse { .. } => {
                    all.insert(runtime_call_id.to_string());
                    latest.insert(runtime_call_id.to_string(), event.id.clone());
                }
                OutboundKind::AgentMcpToolUse { .. } => {
                    all.insert(runtime_call_id.to_string());
                    mcp.push(runtime_call_id.to_string());
                    latest.insert(runtime_call_id.to_string(), event.id.clone());
                }
                _ => {}
            }
        }
        ProjectedToolIndex {
            all,
            mcp,
            latest,
            sources,
        }
    }

    pub(super) fn message_was_projected(&self, thread_id: &str, message_id: &str) -> bool {
        self.projected_thread_message_ids
            .contains(&(thread_id.to_string(), message_id.to_string()))
    }

    pub(super) fn consume_message(&mut self, thread_id: &str, message_id: &str) -> bool {
        self.projected_thread_message_ids
            .insert((thread_id.to_string(), message_id.to_string()))
    }

    pub(super) fn new(
        agent_id: String,
        session: Session,
        resource_state: awaken_session_contract::SessionResourceState,
        events: Vec<Event>,
    ) -> Self {
        Self {
            agent_id,
            session,
            resource_state,
            events,
            pending_transient_events: Vec::new(),
            transient_event_anchors: Default::default(),
            projected_thread_message_ids: Default::default(),
            projected_child_latest_run_ids: Default::default(),
            deferred_session_stop_reason: None,
            event_thread_owners: Default::default(),
            projected_lifecycle_cursor: Default::default(),
            projected_terminal_cursors: Default::default(),
            projected_budget_reach_generation: 0,
            child_threads: Vec::new(),
            primary_thread_usage: None,
        }
    }

    /// Project the HTTP Session DTO from typed aggregate state. The stored
    /// `Session` keeps `resources` empty so JSON cannot become another index.
    pub(super) fn session_projection(&self) -> Session {
        let mut session = self.session.clone();
        session.resources = self
            .resource_state
            .desired()
            .inputs()
            .iter()
            .map(|input| resolved_resource_dto(&session.id, input))
            .collect();
        session
    }
}
