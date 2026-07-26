//! Session thread projections for [`ManagedState`]: the primary thread and
//! subagent child threads.

use super::*;

impl ManagedState {
    fn thread_status(status: &str) -> SessionThreadStatus {
        match status {
            "idle" => SessionThreadStatus::Idle,
            "rescheduling" => SessionThreadStatus::Rescheduling,
            "terminated" | "deleted" | "activation_failed" => SessionThreadStatus::Terminated,
            "running" | "preparing" | "activating" | _ => SessionThreadStatus::Running,
        }
    }

    /// The session's primary thread projection (`BetaManagedAgentsSessionThread`).
    /// A session has one primary thread addressed by `<session_id>:primary`;
    /// delegated child Runs extend this list under their stable Run ids.
    fn primary_thread(record: &SessionRecord) -> SessionThread {
        let session = &record.session;
        SessionThread {
            id: format!("{}:primary", session.id),
            kind: "session_thread",
            session_id: session.id.clone(),
            parent_thread_id: None,
            agent: SessionThreadAgent::from(&session.agent),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            archived_at: session.archived_at.clone(),
            status: Self::thread_status(session.status),
            stats: None,
            usage: None,
        }
    }

    /// A subagent child thread: a `session_thread` whose parent is the primary and
    /// whose `agent` is a minimal snapshot of the delegate `agent_name`.
    pub(crate) fn child_thread(
        session: &Session,
        thread_id: &str,
        agent_name: &str,
    ) -> SessionThread {
        // Reuse the one `SessionAgent` shape rather than rebuild the agent object
        // inline; the delegate's config is unknown here, so it is a minimal snapshot.
        let agent = SessionThreadAgent::from(&SessionAgent {
            id: agent_name.to_string(),
            kind: "agent",
            version: 1,
            model: ModelConfig::new(""),
            name: agent_name.to_string(),
            description: None,
            system: None,
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            multiagent: None,
        });
        SessionThread {
            id: thread_id.to_string(),
            kind: "session_thread",
            session_id: session.id.clone(),
            parent_thread_id: Some(format!("{}:primary", session.id)),
            agent,
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            archived_at: None,
            status: SessionThreadStatus::Running,
            stats: None,
            usage: None,
        }
    }

    /// `GET /v1/sessions/{id}/threads` — the primary thread plus any subagent
    /// child threads spawned by delegation.
    pub fn list_threads(&self, id: &str) -> Result<Vec<SessionThread>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let mut threads = vec![Self::primary_thread(record)];
        threads.extend(record.child_threads.iter().cloned());
        Ok(threads)
    }

    /// `GET /v1/sessions/{id}/threads/{thread_id}` — the primary or a child thread.
    pub fn get_thread(&self, id: &str, thread_id: &str) -> Result<SessionThread, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let primary = Self::primary_thread(record);
        if primary.id == thread_id {
            return Ok(primary);
        }
        record
            .child_threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .cloned()
            .ok_or(StateError::NotFound)
    }

    /// `POST /v1/sessions/{id}/threads/{thread_id}/archive`. Archiving the primary
    /// thread archives the session; archiving a subagent child thread terminates
    /// that thread (emitting `session.thread_status_terminated`).
    pub fn archive_thread(&self, id: &str, thread_id: &str) -> Result<SessionThread, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        if format!("{id}:primary") == thread_id {
            record.session.archived_at = Some(PROCESSED_AT.to_string());
            return Ok(Self::primary_thread(record));
        }
        let Some(child) = record
            .child_threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        else {
            return Err(StateError::NotFound);
        };
        child.archived_at = Some(PROCESSED_AT.to_string());
        child.updated_at = PROCESSED_AT.to_string();
        child.status = SessionThreadStatus::Terminated;
        let agent_name = child.agent.name.clone();
        let archived = child.clone();
        let from = record.events.len();
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionThreadStatusTerminated {
                session_thread_id: thread_id.to_string(),
                agent_name,
            },
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        // Publish the terminated event on the live SSE broadcast — like
        // `append_step`/`append_outcome`/`delete_session` — so a client with an
        // already-open stream sees the child-thread termination in real time
        // instead of only on a reconnect/replay.
        self.broadcast_committed_from(id, record, from);
        Ok(archived)
    }
}
