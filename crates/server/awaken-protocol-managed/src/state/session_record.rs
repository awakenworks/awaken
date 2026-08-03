//! Disposable per-process projection of one durable Session aggregate.

use std::collections::HashSet;

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
    /// Last committed Run lifecycle fact consumed by this disposable projection.
    pub(super) projected_lifecycle_cursor: awaken_agent_contract::LifecycleCursor,
    /// Exact committed lifecycle positions already lowered locally or from the
    /// feed. A Run may await and resume repeatedly, so Run id alone is not an
    /// idempotency key; each terminal transition owns a distinct cursor.
    pub(super) projected_terminal_cursors: HashSet<awaken_agent_contract::LifecycleCursor>,
    /// Subagent child threads spawned in this Session. Each is announced by a
    /// `session.thread_created` event and remains a projection of durable truth.
    pub(super) child_threads: Vec<SessionThread>,
}

impl SessionRecord {
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
            .active
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(&session.id, input))
            .collect();
        session
    }
}
