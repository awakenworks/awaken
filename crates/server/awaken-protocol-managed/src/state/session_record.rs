//! Disposable per-process projection of one durable Session aggregate.

use std::collections::{HashMap, HashSet};

use super::*;

pub(super) struct SessionRecord {
    pub(super) agent_id: String,
    pub(super) session: Session,
    /// Durable source of truth for the runtime's currently applied input projection.
    pub(super) resource_state: awaken_session_contract::SessionResourceState,
    pub(super) events: Vec<Event>,
    /// Runtime message identities already lowered into `events`. The transcript
    /// is durable authority; this set only prevents a peer refresh and the local
    /// request finisher from projecting the same committed message twice.
    pub(super) projected_message_ids: HashSet<String>,
    /// Child Runtime message identities already lowered into the same Session
    /// event projection. The qualified key prevents two isolated child Runs from
    /// colliding when a provider reuses a message id.
    pub(super) projected_child_message_ids: HashSet<(String, String)>,
    /// Disposable ownership index for events projected from child transcripts.
    /// The event remains in `events`; this map only filters the one projection
    /// into the primary or matching child Managed Thread.
    pub(super) event_thread_owners: HashMap<String, String>,
    /// Last committed Run lifecycle fact consumed by this disposable projection.
    pub(super) projected_lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor,
    /// Exact committed lifecycle positions already lowered locally or from the
    /// feed. A Run may await and resume repeatedly, so Run id alone is not an
    /// idempotency key; each terminal transition owns a distinct cursor.
    pub(super) projected_terminal_cursors: HashSet<awaken_agent_contract::RunLifecycleCursor>,
    /// Subagent child threads spawned in this Session. Each is announced by a
    /// `session.thread_created` event and remains a projection of durable truth.
    pub(super) child_threads: Vec<SessionThread>,
}

impl SessionRecord {
    /// Project a Runtime lifecycle hint without allowing the disposable event
    /// view to reverse the Session application's realization, recovery, or
    /// terminal transition. Runtime feeds may be consumed after any of those
    /// CASes, but they are not a second authority for Session lifecycle state.
    pub(super) fn project_runtime_status(&mut self, status: SessionStatus) {
        if Self::accepts_runtime_status(self.session.status) {
            self.session.status = status;
        }
    }

    fn accepts_runtime_status(status: SessionStatus) -> bool {
        matches!(status, SessionStatus::Idle | SessionStatus::Running)
    }

    /// IDs already lowered into tool-use events, plus the MCP subset needed to
    /// classify later results. One scan owns both projections so refresh and
    /// local completion cannot grow separate deduplication rules.
    pub(super) fn projected_tool_ids(&self) -> (HashSet<String>, Vec<String>) {
        let mut all = HashSet::new();
        let mut mcp = Vec::new();
        for event in &self.events {
            match event.kind {
                OutboundKind::AgentToolUse { .. } | OutboundKind::AgentCustomToolUse { .. } => {
                    all.insert(event.id.clone());
                }
                OutboundKind::AgentMcpToolUse { .. } => {
                    all.insert(event.id.clone());
                    mcp.push(event.id.clone());
                }
                _ => {}
            }
        }
        (all, mcp)
    }

    pub(super) fn new(
        agent_id: String,
        session: Session,
        resource_state: awaken_session_contract::SessionResourceState,
        events: Vec<Event>,
        projected_message_ids: HashSet<String>,
    ) -> Self {
        Self {
            agent_id,
            session,
            resource_state,
            events,
            projected_message_ids,
            projected_child_message_ids: Default::default(),
            event_thread_owners: Default::default(),
            projected_lifecycle_cursor: Default::default(),
            projected_terminal_cursors: Default::default(),
            child_threads: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_status_projection_decision_table_preserves_application_authority() {
        // Cause/effect graph: C1=the durable application status is an ordinary
        // Runtime phase (idle/running); C2=it is a realization/recovery phase
        // (rescheduling, including internal prepare/activate); C3=it is terminal;
        // C4=a delayed Runtime lifecycle hint arrives.
        // E1=the disposable wire status follows the Runtime hint; E2=the wire
        // status retains the application's stronger state. Constraint: exactly
        // one of C1/C2/C3 is true. Decision table:
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1   | T  | F  | F  | T  | E1     |
        // | R2   | F  | T  | F  | T  | E2     |
        // | R3   | F  | F  | T  | T  | E2     |
        for initial in [SessionStatus::Idle, SessionStatus::Running] {
            assert!(
                SessionRecord::accepts_runtime_status(initial),
                "R1/{initial:?}"
            );
        }
        for initial in [SessionStatus::Rescheduling, SessionStatus::Terminated] {
            assert!(
                !SessionRecord::accepts_runtime_status(initial),
                "R2-R3/{initial:?}"
            );
        }
    }
}
