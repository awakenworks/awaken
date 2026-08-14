//! Session thread projections for [`ManagedState`]: the primary thread and
//! subagent child threads.

use super::*;

impl ManagedState {
    /// Project the Session's single committed event log into one Thread stream.
    /// The primary owns coordinator events and receives child status cross-posts.
    /// A child sees only its own status plus messages expressed from that child's
    /// perspective. This is a projection, not a second event store.
    pub(crate) fn project_event_for_thread(
        session_id: &str,
        thread_id: &str,
        mut event: Event,
        owner_thread_id: Option<&str>,
    ) -> Option<Event> {
        if thread_id == format!("{session_id}:primary") {
            return owner_thread_id.is_none().then_some(event);
        }
        if owner_thread_id == Some(thread_id) {
            return Some(event);
        }
        if owner_thread_id.is_some() {
            return None;
        }
        let is_own_status = match &event.kind {
            OutboundKind::SessionThreadStatusRunning {
                session_thread_id, ..
            }
            | OutboundKind::SessionThreadStatusIdle {
                session_thread_id, ..
            }
            | OutboundKind::SessionThreadStatusRescheduled {
                session_thread_id, ..
            }
            | OutboundKind::SessionThreadStatusTerminated {
                session_thread_id, ..
            } => session_thread_id == thread_id,
            _ => false,
        };
        if is_own_status {
            return Some(event);
        }
        if matches!(
            &event.kind,
            OutboundKind::UserInterrupt { session_thread_id }
                if session_thread_id.as_deref().is_none_or(|id| id == thread_id)
        ) {
            return Some(event);
        }
        event.kind = match event.kind {
            OutboundKind::AgentThreadMessageSent {
                to_session_thread_id,
                content,
                ..
            } if to_session_thread_id == thread_id => OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id: format!("{session_id}:primary"),
                from_agent_name: None,
                content,
            },
            OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id,
                content,
                ..
            } if from_session_thread_id == thread_id => OutboundKind::AgentThreadMessageSent {
                to_session_thread_id: format!("{session_id}:primary"),
                to_agent_name: None,
                content,
            },
            _ => return None,
        };
        Some(event)
    }

    const fn thread_status(status: SessionStatus) -> SessionThreadStatus {
        match status {
            SessionStatus::Idle => SessionThreadStatus::Idle,
            SessionStatus::Rescheduling => SessionThreadStatus::Rescheduling,
            SessionStatus::Terminated => SessionThreadStatus::Terminated,
            SessionStatus::Running => SessionThreadStatus::Running,
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

    pub(crate) fn thread_agent_from_profile(
        agent_id: &str,
        profile: awaken_executable_agent_contract::ExecutableAgentSessionProfile,
    ) -> SessionThreadAgent {
        let tools =
            crate::project::managed_tools(&awaken_session_contract::SessionToolConfiguration {
                toolsets: profile.toolsets,
                client_tools: profile.client_tools,
            });
        SessionThreadAgent {
            id: agent_id.to_owned(),
            kind: "agent",
            version: profile.source_revision.max(1),
            model: ModelConfig::from_inference(
                profile
                    .model
                    .or(profile.execution_model_ref)
                    .unwrap_or_default(),
                profile.inference,
            ),
            name: profile.name.unwrap_or_else(|| agent_id.to_owned()),
            description: profile.description,
            system: profile.system,
            tools,
            mcp_servers: super::session_mcp_projection::profile_mcp_servers(&profile.mcp_servers),
            skills: profile
                .skills
                .into_iter()
                .map(crate::types::agent::AgentSkill::from_binding)
                .collect(),
        }
    }

    /// A subagent child thread freezes the exact Agent definition already held
    /// by the parent Session; thread creation never looks up current authoring
    /// state or fabricates a second minimal Agent representation.
    pub(crate) fn child_thread(
        session: &Session,
        thread_id: &str,
        agent: SessionThreadAgent,
    ) -> SessionThread {
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

    /// `GET /v1/sessions/{id}/threads/{thread_id}/events` — one typed projection
    /// over the authoritative Session event log, cursor-paginated after filtering.
    pub fn list_thread_events(
        &self,
        id: &str,
        thread_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let primary_id = format!("{id}:primary");
        if thread_id != primary_id && !record.child_threads.iter().any(|t| t.id == thread_id) {
            return Err(StateError::NotFound);
        }
        let events = record
            .events
            .iter()
            .cloned()
            .filter_map(|event| {
                let owner = record.event_thread_owners.get(&event.id);
                Self::project_event_for_thread(id, thread_id, event, owner.map(String::as_str))
            })
            .collect::<Vec<_>>();
        let page = paginate_by_id(&events, cursor, limit, |event| event.id.as_str())
            .map_err(|_| RunError::bad_request("unknown pagination cursor"))?;
        Ok(ListEventsResponse {
            data: page.items.to_vec(),
            next_page: page.next_page,
            has_more: page.has_more,
        })
    }

    /// Return the disposable thread owner of one committed projection event.
    /// Runtime transcripts remain authoritative; this lookup only lets the live
    /// broadcaster apply the same isolation rule as paginated listing.
    pub(crate) fn event_thread_owner(&self, id: &str, event_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .and_then(|record| record.event_thread_owners.get(event_id).cloned())
    }

    /// `POST /v1/sessions/{id}/threads/{thread_id}/archive`. Archiving the primary
    /// thread archives the session; archiving a subagent child thread terminates
    /// that thread (emitting `session.thread_status_terminated`).
    pub async fn archive_thread(
        &self,
        id: &str,
        thread_id: &str,
    ) -> Result<SessionThread, StateError> {
        if format!("{id}:primary") == thread_id {
            self.archive_session(id).await?;
            return self.get_thread(id, thread_id);
        }
        let already_terminated = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            let child = record
                .child_threads
                .iter()
                .find(|thread| thread.id == thread_id)
                .ok_or(StateError::NotFound)?;
            child.status == SessionThreadStatus::Terminated
        };
        if already_terminated {
            return self.get_thread(id, thread_id);
        }

        // Runtime termination is the behavior; the wire status is committed only
        // after that effect succeeds. `end_session` is idempotent for an already
        // absent local child, so a retry after an uncertain response is safe.
        self.application
            .end_runtime_session(thread_id)
            .await
            .map_err(StateError::Run)?;

        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let Some(child) = record
            .child_threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        else {
            return Err(StateError::NotFound);
        };
        if child.status == SessionThreadStatus::Terminated {
            return Ok(child.clone());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_projects_to_the_closed_thread_status_table() {
        // Cause graph: typed Session status -> thread projection. Known terminal,
        // idle and rescheduling states retain their meaning; active states project
        // to Running without inventing a second thread lifecycle.
        let cases = [
            (SessionStatus::Idle, SessionThreadStatus::Idle),
            (
                SessionStatus::Rescheduling,
                SessionThreadStatus::Rescheduling,
            ),
            (SessionStatus::Terminated, SessionThreadStatus::Terminated),
            (SessionStatus::Running, SessionThreadStatus::Running),
        ];

        for (source, expected) in cases {
            assert_eq!(ManagedState::thread_status(source), expected, "{source:?}");
        }
    }
}
